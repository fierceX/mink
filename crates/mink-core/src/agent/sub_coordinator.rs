use crate::agent::sub_executor::{
    SubAgentExecutor, SubAgentOrigin, SubAgentResult, SubAgentStatus, SubAgentStopReason,
};
use crate::agent::text::truncate_str;
use crate::cancel::CancellationToken;
use crate::config::ResolvedConfig as Config;
use crate::context::AgentSharedContext;
use crate::tools::metadata::{ToolFailureKind, ToolFailureSource, ToolStatus};
use crate::tools::runner::ToolExecution;
use futures::FutureExt;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

const SUB_AGENT_ABORT_GRACE_MS: u64 = 250;
const SUB_AGENT_TICK_MS: u64 = 10;
const SUB_AGENT_BATCH_LIMIT: usize = 8;
const PHASE_SETUP: u8 = 0;
const PHASE_RUN: u8 = 1;

/// Everything one spawned sub-agent needs. The runner is injectable so tests
/// can exercise collection semantics without a real child runtime.
#[derive(Clone)]
pub(crate) struct SubAgentRunSpec {
    pub session_id: String,
    pub prompt: String,
    pub label: Option<String>,
    pub model: Option<String>,
    pub fork: bool,
    pub depth: u32,
    pub cancel: CancellationToken,
    pub phase: Arc<AtomicU8>,
    /// Filled by the default runner after the child context exists; custom
    /// runners leave it empty and timeouts then report zero usage.
    pub child_stats: Arc<OnceLock<Arc<crate::session::stats::StatsTracker>>>,
}

pub(crate) type SubAgentRunner = Arc<
    dyn Fn(
            Arc<AgentSharedContext>,
            SubAgentRunSpec,
        ) -> Pin<Box<dyn Future<Output = SubAgentResult> + Send>>
        + Send
        + Sync,
>;

/// Why one batch stopped collecting before every launch reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchEnd {
    Interrupted,
    ChannelClosed,
    ProtocolError,
}

impl BatchEnd {
    /// Output string for still-pending entries (same vocabulary as before).
    fn as_str(self) -> &'static str {
        match self {
            Self::Interrupted => "cancelled",
            Self::ChannelClosed => "channel_closed",
            // Protocol faults are reported separately; pending entries are
            // marked as failures for the caller.
            Self::ProtocolError => "failed",
        }
    }
}

struct SubAgentLaunch {
    session_id: String,
    label: Option<String>,
    mode: String,
    depth: u32,
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
        let sub_semaphore = Arc::new(tokio::sync::Semaphore::new(SUB_AGENT_BATCH_LIMIT));

        for mut result in results {
            if let Some(request) = result.take_sub_agent_request() {
                let depth = self.ctx.sub_agent_depth.saturating_add(1);
                let max_depth = self.ctx.tool_config.sub_agent_max_depth;
                if depth > max_depth {
                    fail_request(
                        &mut result,
                        ToolStatus::Failed(ToolFailureKind::SafetyBlocked),
                        format!(
                            "Error: sub-agent depth {depth} exceeds sub_agent_max_depth {max_depth}."
                        ),
                    );
                    processed_results.push(result);
                    continue;
                }
                let model = match resolve_delegation_model(&self.ctx, request.model.as_deref()) {
                    Ok(model) => model,
                    Err(error) => {
                        fail_request(
                            &mut result,
                            ToolStatus::Failed(ToolFailureKind::ArgumentInvalid),
                            format!("Error: {error}"),
                        );
                        result.failure_source = Some(ToolFailureSource::ModelFormat);
                        processed_results.push(result);
                        continue;
                    }
                };
                let session_id = format!("sub_{}", crate::session::paths::chrono_session_id());
                let fork = request.fork;
                let prompt = request.prompt;
                let label = crate::session::subagent_catalog::sanitize_label(
                    request
                        .label
                        .as_deref()
                        .unwrap_or_else(|| prompt.lines().next().unwrap_or_default()),
                    &session_id,
                );
                let mode = if fork { "fork" } else { "fresh" }.to_string();

                self.ctx.display.render_sub_agent_status_meta(
                    &session_id,
                    "launched",
                    Some(&label),
                    None,
                    0,
                    0,
                );
                self.log_sub_agent_event(
                    &session_id,
                    "launched",
                    Some("queued"),
                    Some(label.clone()),
                    Some(mode.clone()),
                    Some(depth),
                    None,
                    None,
                    None,
                    None,
                );

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
                let phase = Arc::new(AtomicU8::new(PHASE_SETUP));
                let child_stats = Arc::new(OnceLock::new());
                let timeout_secs = self.ctx.tool_config.sub_agent_timeout_secs.max(1) as u64;
                let display = self.ctx.display.clone();
                let spec = SubAgentRunSpec {
                    session_id: session_id.clone(),
                    prompt,
                    label: Some(label.clone()),
                    model,
                    fork,
                    depth,
                    cancel,
                    phase: phase.clone(),
                    child_stats: child_stats.clone(),
                };
                let event_label = label.clone();
                let event_mode = mode.clone();
                let event_ctx = self.ctx.clone();
                let handle = tokio::spawn(async move {
                    // Single send point: either the permit path failed with an
                    // already-classified result, or the runner produced one.
                    let sa = match acquire_sub_agent_permit(sub_semaphore, &spec.cancel).await {
                        Ok(permit) => {
                            let started = Instant::now();
                            display.render_sub_agent_status_meta(
                                &sid,
                                "running",
                                Some(event_label.as_str()),
                                None,
                                0,
                                0,
                            );
                            event_ctx.log_event(crate::events::EventLog::SubAgent {
                                version: Some(2),
                                session_id: sid.clone(),
                                status: "running".into(),
                                input_tokens: None,
                                output_tokens: None,
                                label: Some(event_label),
                                mode: Some(event_mode),
                                parent_session_id: Some(event_ctx.config.session_id.clone()),
                                depth: Some(spec.depth),
                                stop_reason: None,
                                duration_ms: None,
                                phase: Some("run".into()),
                            });
                            let stats_slot = child_stats;
                            let cancel = spec.cancel.clone();
                            let phase = spec.phase.clone();
                            let mut run = Box::pin(run_sub_agent_runner(runner, ctx, spec));
                            let outcome = tokio::select! {
                                result = &mut run => Some(result),
                                _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => None,
                            };
                            let mut sa = match outcome {
                                Some(sa) => sa,
                                None => {
                                    // Cancel the child before giving its setup a
                                    // short grace to observe cancellation and run
                                    // its cleanup guard.
                                    cancel.cancel();
                                    let _ =
                                        tokio::time::timeout(Duration::from_millis(250), &mut run)
                                            .await;
                                    let phase = if phase.load(Ordering::SeqCst) == PHASE_RUN {
                                        "run"
                                    } else {
                                        "setup"
                                    };
                                    let usage = match stats_slot.get() {
                                        Some(tracker) => tracker.snapshot().await,
                                        None => crate::session::stats::Stats::default(),
                                    };
                                    SubAgentResult {
                                        status: SubAgentStatus::TimedOut,
                                        stop_reason: SubAgentStopReason::Timeout,
                                        thinking: String::new(),
                                        text: String::new(),
                                        diagnostic: Some(format!(
                                            "sub_agent_timeout phase={phase} after {timeout_secs}s"
                                        )),
                                        usage,
                                        elapsed_ms: u64::try_from(started.elapsed().as_millis())
                                            .unwrap_or(u64::MAX),
                                    }
                                }
                            };
                            // One clock for every outcome: permit acquisition is the
                            // start, so setup+run are included and queue time is not.
                            sa.elapsed_ms =
                                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                            drop(permit);
                            sa
                        }
                        Err(cancelled) => *cancelled,
                    };
                    let _ = tx.send((sub_idx, sa));
                });
                batch.pending.insert(
                    sub_idx,
                    SubAgentLaunch {
                        session_id: launch_session_id,
                        label: Some(label),
                        mode,
                        depth,
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
        let mut end: Option<BatchEnd> = None;

        while !batch.pending.is_empty() {
            if self.ctx.cancel.is_cancelled() || self.ctx.interrupt.load(Ordering::SeqCst) {
                self.ctx
                    .display
                    .render_info("Sub-agent collection cancelled.");
                end = Some(BatchEnd::Interrupted);
                break;
            }
            tokio::select! {
                _ = self.ctx.cancel.cancelled() => {
                    self.ctx.display.render_info("Sub-agent collection cancelled.");
                    end = Some(BatchEnd::Interrupted);
                    break;
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
                            let mut content = format!(
                                "[sub-agent {}] {} (in={}, out={})",
                                session_id,
                                sa.status.as_str(),
                                sa.usage.total_input_tokens,
                                sa.usage.total_output_tokens,
                            );
                            if !sa.text.is_empty() {
                                content.push('\n');
                                content.push_str(&sa.text);
                            }
                            if let Some(detail) = &sa.diagnostic {
                                content.push_str("\n\ndiagnostic: ");
                                content.push_str(detail);
                            }
                            pr.content = content;
                            pr.conv_content = String::new();
                            pr.state_metadata = Some(serde_json::json!({
                                "sub_agent": {
                                    "session_id": session_id,
                                    "label": launch.label,
                                    "mode": launch.mode,
                                    "depth": launch.depth,
                                    "status": sa.status.as_str(),
                                    "stop_reason": sa.stop_reason.as_str(),
                                    "input_tokens": sa.usage.total_input_tokens,
                                    "output_tokens": sa.usage.total_output_tokens,
                                    "elapsed_ms": sa.elapsed_ms,
                                }
                            }));
                            let preview = if sa.thinking.is_empty() {
                                truncate_str(&sa.text, 60)
                            } else {
                                truncate_str(&sa.thinking, 60)
                            };
                            if !matches!(sa.status, SubAgentStatus::Succeeded) {
                                self.ctx.display.render_error(&format!(
                                    "[sub-agent {}] failed: {}",
                                    session_id, preview
                                ));
                            }
                            self.ctx.display.render_sub_agent_status_meta(
                                &session_id,
                                sa.status.as_str(),
                                launch.label.as_deref(),
                                Some(sa.stop_reason.as_str()),
                                sa.usage.total_input_tokens,
                                sa.usage.total_output_tokens,
                            );
                            self.log_sub_agent_event(
                                &session_id,
                                sa.status.as_str(),
                                None,
                                launch.label,
                                Some(launch.mode),
                                Some(launch.depth),
                                Some(sa.stop_reason.as_str()),
                                Some(sa.elapsed_ms),
                                Some(sa.usage.total_input_tokens),
                                Some(sa.usage.total_output_tokens),
                            );
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
            self.mark_pending_incomplete(&mut processed_results, &batch, end);
            self.drain_pending(&mut batch).await;
        }

        processed_results
    }

    /// Record one catalog event with the additive v2 fields.
    #[allow(clippy::too_many_arguments)]
    fn log_sub_agent_event(
        &self,
        session_id: &str,
        status: &str,
        phase: Option<&str>,
        label: Option<String>,
        mode: Option<String>,
        depth: Option<u32>,
        stop_reason: Option<&str>,
        duration_ms: Option<u64>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) {
        self.ctx.log_event(crate::events::EventLog::SubAgent {
            version: Some(2),
            session_id: session_id.to_string(),
            status: status.to_string(),
            input_tokens,
            output_tokens,
            label,
            mode,
            parent_session_id: Some(self.ctx.config.session_id.clone()),
            depth,
            stop_reason: stop_reason.map(str::to_string),
            duration_ms,
            phase: phase.map(str::to_string),
        });
    }

    /// Mark entries that never reported with the batch-end vocabulary.
    fn mark_pending_incomplete(
        &self,
        processed_results: &mut [ToolExecution],
        batch: &SubAgentBatch,
        end: BatchEnd,
    ) {
        let reason = end.as_str();
        for (idx, launch) in &batch.pending {
            launch.cancel.cancel();
            self.ctx
                .display
                .render_sub_agent_status(&launch.session_id, reason, 0, 0);
            self.log_sub_agent_event(
                &launch.session_id,
                reason,
                None,
                launch.label.clone(),
                Some(launch.mode.clone()),
                Some(launch.depth),
                Some(if end == BatchEnd::Interrupted {
                    "aborted"
                } else {
                    "error"
                }),
                None,
                None,
                None,
            );
            if let Some(pr) = processed_results.get_mut(*idx) {
                pr.status = match end {
                    BatchEnd::Interrupted => ToolStatus::Interrupted,
                    BatchEnd::ChannelClosed | BatchEnd::ProtocolError => {
                        ToolStatus::Failed(ToolFailureKind::Unknown)
                    }
                };
                if pr.content.is_empty() {
                    pr.content = match end {
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

    /// Cancel/abort remaining tasks after a short cooperative grace.
    ///
    /// Handles stay in `batch.pending` during the grace period so dropping
    /// this future still transfers ownership to `SubAgentBatch::drop`.
    async fn drain_pending(&self, batch: &mut SubAgentBatch) {
        let deadline = Instant::now() + Duration::from_millis(SUB_AGENT_ABORT_GRACE_MS);
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
    // The failure is already classified; boxing keeps the Err variant cheap
    // (clippy::result_large_err) without adding a second result type.
) -> Result<tokio::sync::OwnedSemaphorePermit, Box<SubAgentResult>> {
    tokio::select! {
        permit = sub_semaphore.acquire_owned() => match permit {
            Ok(permit) => Ok(permit),
            Err(_) => Err(Box::new(SubAgentResult::terminal(
                SubAgentStatus::Failed,
                SubAgentStopReason::Error,
                String::new(),
                Some("sub_agent_error: semaphore closed".into()),
            ))),
        },
        _ = cancel.cancelled() => Err(Box::new(SubAgentResult::terminal(
            SubAgentStatus::Interrupted,
            SubAgentStopReason::Aborted,
            String::new(),
            Some("sub_agent_interrupted before execution".into()),
        ))),
    }
}

async fn run_sub_agent_runner(
    runner: SubAgentRunner,
    ctx: Arc<AgentSharedContext>,
    spec: SubAgentRunSpec,
) -> SubAgentResult {
    let future = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runner(ctx, spec)))
    {
        Ok(future) => future,
        Err(panic_info) => {
            return SubAgentResult::terminal(
                SubAgentStatus::Failed,
                SubAgentStopReason::Error,
                String::new(),
                Some(format!(
                    "sub_agent_error: task panicked: {}",
                    panic_message(panic_info)
                )),
            );
        }
    };
    match std::panic::AssertUnwindSafe(future).catch_unwind().await {
        Ok(sa) => sa,
        Err(panic_info) => SubAgentResult::terminal(
            SubAgentStatus::Failed,
            SubAgentStopReason::Error,
            String::new(),
            Some(format!(
                "sub_agent_error: task panicked: {}",
                panic_message(panic_info)
            )),
        ),
    }
}

fn default_sub_agent_runner(config: Config) -> SubAgentRunner {
    Arc::new(move |ctx, spec| {
        let config = config.clone();
        Box::pin(async move {
            let cancel = spec.cancel.clone();
            match SubAgentExecutor::new_child(
                ctx,
                spec.session_id.clone(),
                spec.fork,
                config,
                spec.cancel.clone(),
                spec.label.clone(),
                spec.model.clone(),
                SubAgentOrigin::SubAgent,
            )
            .await
            {
                Ok(executor) => {
                    let _ = spec.child_stats.set(executor.context().stats.clone());
                    spec.phase.store(PHASE_RUN, Ordering::SeqCst);
                    executor.execute(spec.prompt).await
                }
                Err(_error) if cancel.is_cancelled() => SubAgentResult::terminal(
                    SubAgentStatus::Interrupted,
                    SubAgentStopReason::Aborted,
                    String::new(),
                    Some("sub_agent_interrupted during setup".into()),
                ),
                Err(error) => SubAgentResult::terminal(
                    SubAgentStatus::Failed,
                    SubAgentStopReason::Error,
                    String::new(),
                    Some(format!("sub_agent_setup_error: {error:#}")),
                ),
            }
        })
    })
}

/// Reject an invalid child model route before creating a child session.
fn resolve_delegation_model(
    ctx: &AgentSharedContext,
    requested: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(raw) = requested.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let resolver = crate::config::model_resolver(&ctx.config);
    let current = ctx
        .active_model
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .actual
        .clone();
    if resolver.aliases().contains_key(raw) || raw == current {
        return Ok(Some(raw.to_string()));
    }
    let allowed = resolver
        .aliases()
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "sub_agent_model_not_allowed: '{raw}' is neither a configured model alias nor the current model; allowed aliases: {allowed}"
    ))
}

fn fail_request(result: &mut ToolExecution, status: ToolStatus, content: String) {
    result.status = status;
    result.content = content;
    result.spawns_sub_agent = false;
    result.needs_finalization = false;
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
