use crate::agent::sub_executor::{SubAgentExecutor, SubAgentResult, SubAgentStatus};
use crate::agent::text::truncate_str;
use crate::cancel::CancellationToken;
use crate::config::ResolvedConfig as Config;
use crate::context::AgentSharedContext;
use crate::tools::metadata::{ToolFailureKind, ToolStatus};
use crate::tools::runner::ToolExecution;
use futures::FutureExt;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const SUB_AGENT_ABORT_GRACE_MS: u64 = 250;
const SUB_AGENT_TICK_MS: u64 = 10;

pub(crate) type SubAgentRunner = Arc<
    dyn Fn(
            Arc<AgentSharedContext>,
            String,
            String,
            bool,
            CancellationToken,
        ) -> Pin<Box<dyn Future<Output = SubAgentResult> + Send>>
        + Send
        + Sync,
>;

/// Why one batch stopped collecting before every launch reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchEnd {
    Interrupted,
    TimedOut,
    ChannelClosed,
    ProtocolError,
}

impl BatchEnd {
    /// Output string for still-pending entries (same vocabulary as before).
    fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "cancelled",
            Self::TimedOut => "timed_out",
            Self::ChannelClosed => "channel_closed",
            // Protocol faults are reported separately; pending entries are
            // marked as failures for the caller.
            Self::ProtocolError => "failed",
        }
    }
}

struct SubAgentLaunch {
    session_id: String,
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

/// Sole owner of not-yet-completed launches in one batch.
///
/// The map key is the result slot index (also the channel payload index), so
/// there is no second completion counter or completion-order set.
struct SubAgentBatch {
    pending: BTreeMap<usize, SubAgentLaunch>,
}

impl Drop for SubAgentBatch {
    fn drop(&mut self) {
        // The collector future may be dropped at any await: every remaining
        // task gets an explicit cancel + abort request instead of detaching.
        for (_, launch) in std::mem::take(&mut self.pending) {
            launch.cancel.cancel();
            launch.handle.abort();
        }
    }
}

pub struct SubAgentCoordinator {
    ctx: Arc<AgentSharedContext>,
}

impl SubAgentCoordinator {
    pub fn new(ctx: Arc<AgentSharedContext>) -> Self {
        Self { ctx }
    }

    pub async fn process(
        &self,
        results: Vec<ToolExecution>,
        sub_agent_config: &Config,
    ) -> Vec<ToolExecution> {
        self.process_with_runner(results, default_sub_agent_runner(sub_agent_config.clone()))
            .await
    }

    pub(crate) async fn process_with_runner(
        &self,
        results: Vec<ToolExecution>,
        runner: SubAgentRunner,
    ) -> Vec<ToolExecution> {
        let mut processed_results = Vec::new();
        let (sub_result_tx, sub_result_rx) =
            tokio::sync::mpsc::unbounded_channel::<(usize, SubAgentResult)>();
        let mut batch = SubAgentBatch {
            pending: BTreeMap::new(),
        };
        let sub_semaphore = Arc::new(tokio::sync::Semaphore::new(8));

        for mut result in results {
            if let Some(request) = result.take_sub_agent_request() {
                if self.ctx.is_sub_agent {
                    self.ctx.display.render_info(
                        "Sub-agent recursion blocked: sub-agent cannot spawn sub-agents.",
                    );
                    result.content =
                        "Error: sub-agent recursion blocked: sub-agent cannot spawn sub-agents."
                            .to_string();
                    result.status = ToolStatus::Failed(ToolFailureKind::SafetyBlocked);
                    result.spawns_sub_agent = false;
                    processed_results.push(result);
                    continue;
                }
                let session_id = format!("sub_{}", crate::session::paths::chrono_session_id());
                let fork = request.fork;
                let prompt = request.prompt;

                self.ctx
                    .display
                    .render_sub_agent_status(&session_id, "launched", 0, 0);
                self.ctx.log_event(crate::events::EventLog::SubAgent {
                    session_id: session_id.clone(),
                    status: "launched".into(),
                    input_tokens: None,
                    output_tokens: None,
                });

                let sub_idx = processed_results.len();
                processed_results.push(result);

                let tx = sub_result_tx.clone();
                let ctx = self.ctx.clone();
                let sid = session_id.clone();
                let cancel = self.ctx.cancel.linked_child_token();
                let sub_semaphore = sub_semaphore.clone();
                let runner = runner.clone();
                let launch_cancel = cancel.clone();
                let launch_session_id = session_id.clone();
                let handle = tokio::spawn(async move {
                    // Single send point: either the permit path failed with an
                    // already-classified result, or the runner produced one.
                    let sa = match acquire_sub_agent_permit(sub_semaphore, &cancel).await {
                        Ok(permit) => {
                            let sa =
                                run_sub_agent_runner(runner, ctx, sid, prompt, fork, cancel).await;
                            drop(permit);
                            sa
                        }
                        Err(cancelled) => cancelled,
                    };
                    let _ = tx.send((sub_idx, sa));
                });
                batch.pending.insert(
                    sub_idx,
                    SubAgentLaunch {
                        session_id: launch_session_id,
                        cancel: launch_cancel,
                        handle,
                    },
                );
            } else {
                processed_results.push(result);
            }
        }

        // Drop the original sender so `recv()` can observe a closed channel
        // once every task sender has gone away.
        drop(sub_result_tx);

        self.collect_results(processed_results, sub_result_rx, batch)
            .await
    }

    async fn collect_results(
        &self,
        mut processed_results: Vec<ToolExecution>,
        mut sub_result_rx: tokio::sync::mpsc::UnboundedReceiver<(usize, SubAgentResult)>,
        mut batch: SubAgentBatch,
    ) -> Vec<ToolExecution> {
        let timeout = self.ctx.tool_config.sub_agent_timeout_secs.max(0);
        let deadline = Instant::now() + Duration::from_secs(timeout as u64);
        let mut end: Option<BatchEnd> = None;

        while !batch.pending.is_empty() {
            if self.ctx.cancel.is_cancelled() || self.ctx.interrupt.load(Ordering::SeqCst) {
                self.ctx
                    .display
                    .render_info("Sub-agent collection cancelled.");
                end = Some(BatchEnd::Interrupted);
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                self.ctx
                    .display
                    .render_error(&format!("Sub-agent batch timed out after {}s.", timeout));
                end = Some(BatchEnd::TimedOut);
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            tokio::select! {
                _ = self.ctx.cancel.cancelled() => {
                    self.ctx.display.render_info("Sub-agent collection cancelled.");
                    end = Some(BatchEnd::Interrupted);
                    break;
                }
                _ = tokio::time::sleep(remaining) => {
                    // Absolute deadline: ticks and results never restart it.
                }
                _ = tokio::time::sleep(Duration::from_millis(SUB_AGENT_TICK_MS)) => {
                    if self.ctx.interrupt.load(Ordering::SeqCst) {
                        self.ctx.display.render_info("Sub-agent collection cancelled.");
                        end = Some(BatchEnd::Interrupted);
                        break;
                    }
                }
                received = sub_result_rx.recv() => {
                    match received {
                        Some((idx, sa)) => {
                            let Some(launch) = batch.pending.remove(&idx) else {
                                // Unknown or repeated index is an internal
                                // protocol fault, never a silent skip.
                                self.ctx.display.render_error(&format!(
                                    "Sub-agent collection protocol error: unknown result slot {idx}."
                                ));
                                end = Some(BatchEnd::ProtocolError);
                                break;
                            };
                            launch.cancel.cancel();
                            let Some(pr) = processed_results.get_mut(idx) else {
                                self.ctx.display.render_error(&format!(
                                    "Sub-agent collection protocol error: result slot {idx} out of range."
                                ));
                                end = Some(BatchEnd::ProtocolError);
                                break;
                            };
                            let session_id = launch.session_id;
                            pr.status = sa.status.to_tool_status();
                            pr.content = format!(
                                "[sub-agent {}] {} (in={}, out={})\nThinking: {}\nText: {}",
                                session_id,
                                sa.status.as_str(),
                                sa.usage.total_input_tokens,
                                sa.usage.total_output_tokens,
                                sa.thinking,
                                sa.text
                            );
                            let preview = truncate_str(&sa.thinking, 60);
                            if !matches!(sa.status, SubAgentStatus::Succeeded) {
                                self.ctx.display.render_error(&format!(
                                    "[sub-agent {}] failed: {}",
                                    session_id, preview
                                ));
                            }
                            self.ctx.display.render_sub_agent_status(
                                &session_id,
                                sa.status.as_str(),
                                sa.usage.total_input_tokens,
                                sa.usage.total_output_tokens,
                            );
                            self.ctx.log_event(crate::events::EventLog::SubAgent {
                                session_id,
                                status: sa.status.as_str().to_string(),
                                input_tokens: Some(sa.usage.total_input_tokens),
                                output_tokens: Some(sa.usage.total_output_tokens),
                            });
                            self.ctx
                                .stats
                                .record_sub_agent(
                                    sa.usage.agent_request_count,
                                    sa.usage.total_input_tokens,
                                    sa.usage.total_output_tokens,
                                    sa.usage.total_cache_read_tokens,
                                    sa.usage.total_cache_creation_tokens,
                                )
                                .await;
                        }
                        None => {
                            end = Some(BatchEnd::ChannelClosed);
                            break;
                        }
                    }
                }
            }
        }

        if let Some(end) = end {
            self.mark_pending_incomplete(&mut processed_results, &batch, end, timeout);
            self.drain_pending(&mut batch, timeout).await;
        }

        processed_results
    }

    /// Mark entries that never reported with the batch-end vocabulary.
    fn mark_pending_incomplete(
        &self,
        processed_results: &mut [ToolExecution],
        batch: &SubAgentBatch,
        end: BatchEnd,
        timeout: i32,
    ) {
        let reason = end.as_str();
        for (idx, launch) in &batch.pending {
            launch.cancel.cancel();
            self.ctx
                .display
                .render_sub_agent_status(&launch.session_id, reason, 0, 0);
            self.ctx.log_event(crate::events::EventLog::SubAgent {
                session_id: launch.session_id.clone(),
                status: reason.into(),
                input_tokens: None,
                output_tokens: None,
            });
            if let Some(pr) = processed_results.get_mut(*idx) {
                pr.status = match end {
                    BatchEnd::Interrupted => ToolStatus::Interrupted,
                    BatchEnd::TimedOut => ToolStatus::Failed(ToolFailureKind::Timeout),
                    BatchEnd::ChannelClosed | BatchEnd::ProtocolError => {
                        ToolStatus::Failed(ToolFailureKind::Unknown)
                    }
                };
                if pr.content.is_empty() {
                    pr.content = match end {
                        BatchEnd::TimedOut => format!("Sub-agent timed out after {timeout}s."),
                        BatchEnd::Interrupted => {
                            "Sub-agent cancelled before completion.".to_string()
                        }
                        BatchEnd::ChannelClosed => "Sub-agent did not complete.".to_string(),
                        BatchEnd::ProtocolError => {
                            "Sub-agent result protocol error; task aborted.".to_string()
                        }
                    };
                }
            }
        }
    }

    /// Cancel/abort remaining tasks after an optional cooperative grace.
    ///
    /// Handles stay in `batch.pending` during the grace period so dropping
    /// this future still transfers ownership to `SubAgentBatch::drop`.
    async fn drain_pending(&self, batch: &mut SubAgentBatch, timeout: i32) {
        let grace = if timeout == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(SUB_AGENT_ABORT_GRACE_MS)
        };
        let deadline = Instant::now() + grace;
        while !batch.pending.is_empty() {
            batch
                .pending
                .retain(|_, launch| !launch.handle.is_finished());
            if batch.pending.is_empty() || Instant::now() >= deadline {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(std::cmp::min(
                remaining,
                Duration::from_millis(SUB_AGENT_TICK_MS),
            ))
            .await;
        }
        // Abort requests are synchronous and no-await: a task that refuses to
        // yield (CPU loop or running spawn_blocking) is not forcibly stopped.
        for (_, launch) in std::mem::take(&mut batch.pending) {
            if !launch.handle.is_finished() {
                launch.handle.abort();
            }
        }
    }
}

async fn acquire_sub_agent_permit(
    sub_semaphore: Arc<tokio::sync::Semaphore>,
    cancel: &CancellationToken,
) -> Result<tokio::sync::OwnedSemaphorePermit, SubAgentResult> {
    tokio::select! {
        permit = sub_semaphore.acquire_owned() => match permit {
            Ok(permit) => Ok(permit),
            Err(_) => Err(SubAgentResult {
                status: SubAgentStatus::Failed,
                thinking: String::new(),
                text: "Sub-agent semaphore closed.".into(),
                usage: Default::default(),
            }),
        },
        _ = cancel.cancelled() => Err(SubAgentResult {
            status: SubAgentStatus::Interrupted,
            thinking: String::new(),
            text: "Sub-agent cancelled before execution.".into(),
            usage: Default::default(),
        }),
    }
}

async fn run_sub_agent_runner(
    runner: SubAgentRunner,
    ctx: Arc<AgentSharedContext>,
    sid: String,
    prompt: String,
    fork: bool,
    cancel: CancellationToken,
) -> SubAgentResult {
    let future = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runner(ctx, sid, prompt, fork, cancel)
    })) {
        Ok(future) => future,
        Err(panic_info) => {
            return SubAgentResult {
                status: SubAgentStatus::Failed,
                thinking: String::new(),
                text: format!("Sub-agent task panicked: {}", panic_message(panic_info)),
                usage: Default::default(),
            };
        }
    };
    match std::panic::AssertUnwindSafe(future).catch_unwind().await {
        Ok(sa) => sa,
        Err(panic_info) => SubAgentResult {
            status: SubAgentStatus::Failed,
            thinking: String::new(),
            text: format!("Sub-agent task panicked: {}", panic_message(panic_info)),
            usage: Default::default(),
        },
    }
}

fn default_sub_agent_runner(config: Config) -> SubAgentRunner {
    Arc::new(move |ctx, sid, prompt, fork, cancel| {
        let config = config.clone();
        Box::pin(async move {
            match SubAgentExecutor::new_with_cancel(ctx, sid, fork, config, cancel).await {
                Ok(executor) => executor.execute(prompt).await,
                Err(e) => SubAgentResult {
                    status: SubAgentStatus::Failed,
                    thinking: String::new(),
                    text: format!("Failed to create sub-agent: {e}"),
                    usage: Default::default(),
                },
            }
        })
    })
}

fn panic_message(panic_info: Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = panic_info.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic_info.downcast_ref::<String>() {
        s.clone()
    } else {
        "sub-agent thread panicked".to_string()
    }
}
