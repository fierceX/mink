use anyhow::{Result, anyhow};
use mink::runtime::{
    AgentEvent, AgentEventStream, AgentOptions, AgentRuntime, GoalSnapshot, SessionInfo,
};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

const MAX_PROGRESS_ITEMS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BroadcastEventKind {
    Standard,
    Control,
}

#[derive(Debug, Clone)]
pub(crate) struct BroadcastEvent {
    pub(crate) kind: BroadcastEventKind,
    pub(crate) payload: Arc<str>,
}

pub(crate) struct ActiveOpGuard {
    in_flight: Arc<AtomicUsize>,
    state: Arc<Mutex<RuntimeState>>,
}

impl Drop for ActiveOpGuard {
    fn drop(&mut self) {
        if self.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.last_idle = Instant::now();
        }
    }
}

mod events;
mod projection;
#[cfg(test)]
mod tests;
mod turn_guard;

pub(crate) use events::goal_summary_value;
use events::{GoalTurnSink, goal_summary, make_broadcast_event, publish, publish_goal};
use projection::Projection;
pub(crate) use turn_guard::draft_generation_timeout_secs;
use turn_guard::publish_forced_timeout_final;
use turn_guard::run_turn_guarded;

pub(super) const TURN_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
const TURN_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
/// Snapshot tail policy: at most this many recent user turns, and a soft byte
/// ceiling that drops older whole turns first. The newest turn is always kept
/// even when it alone exceeds the ceiling, so a tool exchange is never split.
pub(super) const SNAPSHOT_RECENT_TURNS: usize = 4;
pub(super) const SNAPSHOT_MAX_BYTES: usize = 256 * 1024;
const OWNED_SHUTDOWN_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(12);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePhase {
    Idle,
    Running,
    Cancelling,
    Closing,
    Closed,
}

pub(super) enum GuardedTurnResult {
    Finished(Result<()>),
    ForceClose {
        reason: String,
        saw_stop: bool,
        saw_final: bool,
        turn_id: String,
        session: Box<SessionInfo>,
    },
}

pub(super) struct RuntimeState {
    pub(super) runtime: Option<AgentRuntime>,
    pub(super) phase: RuntimePhase,
    pub(super) turn_task: Option<JoinHandle<Result<()>>>,
    pub(super) last_idle: Instant,
    pub(super) forced_terminal: Option<ForcedTerminal>,
}

pub(super) struct ForcedTerminal {
    pub(super) reason: String,
    pub(super) saw_stop: bool,
    pub(super) saw_final: bool,
    pub(super) turn_id: String,
    pub(super) session: Box<SessionInfo>,
}

pub struct SessionRuntime {
    projection: Arc<Mutex<Projection>>,
    inbox: Arc<mink::runtime::InputInbox>,
    image_input: mink::runtime::ImageInputCapability,
    attachments: Arc<mink::runtime::AttachmentStore>,
    goal_dir: std::path::PathBuf,
    state: Arc<Mutex<RuntimeState>>,
    in_flight_ops: Arc<AtomicUsize>,
    event_tx: broadcast::Sender<Arc<BroadcastEvent>>,
    stream_sequence: Arc<AtomicU64>,
}

impl SessionRuntime {
    pub async fn open(options: AgentOptions) -> Result<Self> {
        // Goal turns bypass `submit_input`, so without a sink their body, tool
        // calls, formal history and final never reach the mirror. The
        // projection is built below, so the sink reaches it through a slot that
        // is filled before the driver can run.
        let goal_projection: Arc<OnceLock<Arc<Mutex<Projection>>>> = Arc::new(OnceLock::new());
        let (event_tx, _) = broadcast::channel(1024);
        let stream_sequence = Arc::new(AtomicU64::new(1));
        let options = options.with_goal_event_sink(Arc::new(GoalTurnSink {
            tx: event_tx.clone(),
            sequence: Arc::clone(&stream_sequence),
            projection: goal_projection.clone(),
        }));
        let runtime = AgentRuntime::start(options).await?;
        let info = runtime.session_info();
        let directory = info
            .conversation_path
            .parent()
            .expect("session directory")
            .to_path_buf();
        let read_dir = directory.clone();
        let read_result = tokio::task::spawn_blocking(move || -> Result<_> {
            let reader = mink::runtime::session::SessionReader::new(read_dir.clone());
            let rows = if read_dir.join("conversation.jsonl").exists() {
                reader.conversation_turns_bounded(0, SNAPSHOT_RECENT_TURNS + 1, true, None, None)?
            } else {
                Vec::new()
            };
            Ok((
                rows,
                serde_json::json!({"plan":reader.plan()?,"todo":reader.todo()?,"artifacts":reader.artifacts()?}),
            ))
        })
        .await.map_err(anyhow::Error::from).and_then(|result| result);
        let (rows, resources) = match read_result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return match runtime.shutdown().await {
                    Ok(()) => Err(error.context("cannot initialize session projection")),
                    Err(shutdown) => Err(anyhow!(
                        "cannot initialize session projection: {error:#}; cleanup failed: {shutdown}"
                    )),
                };
            }
        };
        let (model, stats) = runtime.presentation_snapshot().await;
        // Seed the goal mirror before the projection is shared: a restored
        // goal must be visible in the first snapshot without a state read. A
        // cleared (hidden) goal is seeded as null, exactly like every later
        // publication normalizes it.
        let goal = runtime.goals().snapshot().filter(|goal| !goal.hidden);
        let has_older = rows
            .first()
            .and_then(|r| r.get("seq"))
            .and_then(serde_json::Value::as_u64)
            .is_some_and(|seq| seq > 1);
        let mut projection = Projection {
            generation: format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
            ),
            watermark: 0,
            rows,
            row_bytes: vec![],
            has_older,
            progress: vec![],
            current_turn: None,
            phase: "idle",
            last_final: None,
            resources,
            diagnostics: serde_json::json!({"type":"title_update","model":model,"stats":stats}),
            activity: serde_json::json!({"work_state":"idle","wait_elapsed_secs":null,"active_sub_agents":[]}),
            goal,
        };
        projection.trim_initial();
        let inbox = runtime.handle().input_inbox().clone();
        let image_input = runtime.handle().image_input().clone();
        let attachments = Arc::new(mink::runtime::AttachmentStore::new(
            directory.join("attachments"),
        ));
        // Subscribe before the runtime is moved into state: the watch channel
        // carries every later lifecycle change and replays the current value.
        let mut goal_updates = runtime.goals().subscribe();
        let mut goal_turns = runtime.goals().subscribe_turns();
        let projection = Arc::new(Mutex::new(projection));
        let _ = goal_projection.set(projection.clone());
        let state = Arc::new(Mutex::new(RuntimeState {
            runtime: Some(runtime),
            phase: RuntimePhase::Idle,
            turn_task: None,
            last_idle: Instant::now(),
            forced_terminal: None,
        }));
        let runtime_this = Self {
            projection: projection.clone(),
            inbox,
            image_input,
            attachments,
            goal_dir: directory,
            state: state.clone(),
            in_flight_ops: Arc::new(AtomicUsize::new(0)),
            event_tx: event_tx.clone(),
            stream_sequence: stream_sequence.clone(),
        };
        // Goal lifecycle is not part of the turn event stream, so the workbench
        // would never hear about it. Mirror the authoritative watch channel into
        // the same ordered SSE channel the browser already subscribes to; the
        // runtime stays the single writer of goal state.
        let goal_tx = event_tx.clone();
        let goal_seq = Arc::clone(&runtime_this.stream_sequence);
        let goal_projection = Arc::clone(&projection);
        let turn_tx = goal_tx.clone();
        let turn_seq = Arc::clone(&goal_seq);
        let turn_projection = Arc::clone(&goal_projection);
        let turn_state = state.clone();
        tokio::spawn(async move {
            loop {
                if goal_turns.changed().await.is_err() {
                    return;
                }
                let active = *goal_turns.borrow_and_update();
                // A goal turn is a turn. Writing it into the session's own state
                // is what makes interrupt, idle_for and the idle reaper see it:
                // a parallel display flag left those three blind, and the
                // reaper would close a session mid-goal.
                {
                    let mut guard = turn_state.lock().unwrap_or_else(|e| e.into_inner());
                    if !matches!(guard.phase, RuntimePhase::Closed | RuntimePhase::Closing) {
                        guard.phase = if active {
                            RuntimePhase::Running
                        } else {
                            // An interrupted goal turn settles as Cancelling, not
                            // Running. Leaving it there kept `running` true
                            // forever: the session held its concurrency slot
                            // and `idle_for` never reclaimed it.
                            match guard.phase {
                                RuntimePhase::Running | RuntimePhase::Cancelling => {
                                    RuntimePhase::Idle
                                }
                                other => other,
                            }
                        };
                        if !active {
                            guard.last_idle = Instant::now();
                        }
                    }
                }
                publish(
                    &turn_tx,
                    &turn_seq,
                    &turn_projection,
                    serde_json::json!({
                        "type": "phase_updated",
                        "phase": if active { "running" } else { "idle" }
                    }),
                );
            }
        });

        tokio::spawn(async move {
            loop {
                if goal_updates.changed().await.is_err() {
                    return;
                }
                let current = goal_updates.borrow_and_update().clone();
                // Mirror and event share one critical section: the snapshot
                // watermark and the goal it carries can never disagree.
                publish_goal(&goal_tx, &goal_seq, &goal_projection, current);
            }
        });
        Ok(runtime_this)
    }

    pub fn phase(&self) -> RuntimePhase {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).phase
    }
    pub fn running(&self) -> bool {
        matches!(
            self.phase(),
            RuntimePhase::Running | RuntimePhase::Cancelling | RuntimePhase::Closing
        )
    }
    pub fn closed(&self) -> bool {
        self.phase() == RuntimePhase::Closed
    }
    pub fn idle_for(&self) -> Option<std::time::Duration> {
        if self.in_flight_ops.load(Ordering::Acquire) > 0 {
            return None;
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.phase == RuntimePhase::Idle).then(|| state.last_idle.elapsed())
    }
    pub(crate) fn begin_operation(&self) -> ActiveOpGuard {
        self.in_flight_ops.fetch_add(1, Ordering::AcqRel);
        ActiveOpGuard {
            in_flight: Arc::clone(&self.in_flight_ops),
            state: Arc::clone(&self.state),
        }
    }
    pub(crate) fn event_receiver(&self) -> broadcast::Receiver<Arc<BroadcastEvent>> {
        self.event_tx.subscribe()
    }

    /// Cloneable handle to the live runtime, or `None` once it is closed.
    /// Goal control goes through this so the core goal contract — host-owned
    /// limits, lease ownership and acceptance checks — stays authoritative.
    /// Core handle for host controls that are not goal commands.
    ///
    /// Named after what it is rather than `goal_handle` so model switching and
    /// manual compaction do not read as goal operations.
    pub fn handle(&self) -> Option<mink::runtime::AgentRuntimeHandle> {
        self.goal_handle()
    }

    pub fn goal_handle(&self) -> Option<mink::runtime::AgentRuntimeHandle> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtime
            .as_ref()
            .map(mink::runtime::AgentRuntime::handle)
    }

    /// Live goal snapshot, falling back to the persisted file when no runtime
    /// is open so a closed session still reports what it left behind.
    pub fn goal_snapshot(&self) -> Option<mink::runtime::GoalSnapshot> {
        if let Some(goal) = self
            .goal_handle()
            .and_then(|handle| handle.goals().snapshot())
        {
            return Some(goal);
        }
        self.persisted_goal()
    }

    pub fn persisted_goal(&self) -> Option<mink::runtime::GoalSnapshot> {
        mink::runtime::session::read_goal_snapshot(&self.goal_path())
    }

    fn goal_path(&self) -> std::path::PathBuf {
        let directory = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtime
            .as_ref()
            .map(|runtime| runtime.session_info().goal_path());
        directory
            .or_else(|| Some(self.goal_fallback_path()))
            .unwrap_or_default()
    }

    fn goal_fallback_path(&self) -> std::path::PathBuf {
        // Only reached once the runtime is dropped; the session directory is
        // recovered from the projection's stored artifact root.
        self.goal_dir.join("goal.json")
    }

    pub fn inbox(&self) -> &Arc<mink::runtime::InputInbox> {
        &self.inbox
    }
    pub fn image_input(&self) -> &mink::runtime::ImageInputCapability {
        &self.image_input
    }
    pub fn attachments(&self) -> &Arc<mink::runtime::AttachmentStore> {
        &self.attachments
    }
    pub fn publish_inputs(&self) {
        publish(
            &self.event_tx,
            &self.stream_sequence,
            &self.projection,
            serde_json::json!({"type":"inputs_updated","inputs":self.inbox.outstanding()}),
        );
    }
    /// A retry that reuses an admitted request id must not take a second
    /// capacity slot: resolve it before reserving. `None` means this is a new
    /// submission and the normal turn path owns it.
    pub(crate) fn existing_turn_input(
        &self,
        request_id: &str,
        text: &str,
    ) -> Result<Option<mink::runtime::InputReceipt>> {
        self.inbox.existing(&mink::runtime::HumanInput {
            request_id: request_id.to_string(),
            text: text.to_string(),
            target_turn_id: None,
            attachment_ids: vec![],
        })
    }

    /// Start a turn that owns a capacity permit for its whole execution.
    pub(crate) fn start_turn_with_permit(
        &self,
        input: String,
        request_id: Option<String>,
        permit: Option<crate::session::admission::ExecutionPermit>,
    ) -> Result<()> {
        let request_id = request_id
            .filter(|id| !id.trim().is_empty())
            .unwrap_or_else(|| {
                format!(
                    "legacy-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_nanos())
                        .unwrap_or_default()
                )
            });
        self.submit(
            mink::runtime::HumanInput {
                request_id,
                text: input,
                target_turn_id: None,
                attachment_ids: vec![],
            },
            None,
            permit,
        )
        .map(|_| ())
    }
    pub fn submit(
        &self,
        input: mink::runtime::HumanInput,
        resume: Option<(&str, u64)>,
        permit: Option<crate::session::admission::ExecutionPermit>,
    ) -> Result<mink::runtime::InputReceipt> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if resume.is_none()
            && let Some(receipt) = self.inbox.existing(&input)?
        {
            return Ok(receipt);
        }
        if input.target_turn_id.is_some() && resume.is_none() {
            if state.phase != RuntimePhase::Running {
                anyhow::bail!("turn no longer accepts guidance");
            }
            let runtime = state
                .runtime
                .as_ref()
                .ok_or_else(|| anyhow!("session runtime is closed"))?;
            let (receipt, _) = runtime.handle().stream_input(input)?;
            drop(state);
            self.publish_inputs();
            return Ok(receipt);
        }
        if state.phase != RuntimePhase::Idle {
            anyhow::bail!("session runtime is {:?}", state.phase);
        }
        if state
            .turn_task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            anyhow::bail!("session turn cleanup is still running");
        }
        state.turn_task.take();
        let runtime = state
            .runtime
            .as_ref()
            .ok_or_else(|| anyhow!("session runtime is closed"))?;
        let session = runtime.session_info().clone();
        let (receipt, stream) = match resume {
            Some((id, revision)) => runtime.handle().resume_input(id, revision)?,
            None => runtime.handle().stream_input(input)?,
        };
        let Some(stream) = stream else {
            return Ok(receipt);
        };
        {
            let mut p = self.projection.lock().unwrap_or_else(|e| e.into_inner());
            p.current_turn = Some(receipt.turn_id.clone());
            p.phase = "running";
            p.progress.clear();
        }
        state.phase = RuntimePhase::Running;
        let shared = self.state.clone();
        let tx = self.event_tx.clone();
        let stream_sequence = self.stream_sequence.clone();
        let projection = self.projection.clone();
        let inbox = self.inbox.clone();
        state.turn_task = Some(tokio::spawn(async move {
            // The permit is the turn's capacity claim: it is released exactly
            // when this task ends, on every exit path.
            if let Some(permit) = &permit {
                permit.mark_running();
            }
            let _permit = permit;
            let guarded =
                run_turn_guarded(&tx, &stream_sequence, &projection, &shared, stream, session)
                    .await;
            let result = match guarded {
                GuardedTurnResult::Finished(result) => {
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    if !matches!(state.phase, RuntimePhase::Closing | RuntimePhase::Closed) {
                        state.phase = RuntimePhase::Idle;
                        state.last_idle = Instant::now();
                    }
                    drop(state);
                    publish(
                        &tx,
                        &stream_sequence,
                        &projection,
                        serde_json::json!({"type":"inputs_updated","inputs":inbox.outstanding()}),
                    );
                    result
                }
                GuardedTurnResult::ForceClose {
                    reason,
                    saw_stop,
                    saw_final,
                    turn_id,
                    session,
                } => {
                    let runtime = {
                        let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                        state.forced_terminal = Some(ForcedTerminal {
                            reason: reason.clone(),
                            saw_stop,
                            saw_final,
                            turn_id,
                            session,
                        });
                        state.phase = RuntimePhase::Closing;
                        publish(
                            &tx,
                            &stream_sequence,
                            &projection,
                            serde_json::json!({"type":"phase_updated","phase":"closing"}),
                        );
                        state.runtime.take()
                    };
                    let owns_runtime_shutdown = runtime.is_some();
                    let shutdown_error = if let Some(runtime) = runtime {
                        runtime
                            .shutdown()
                            .await
                            .err()
                            .map(|error| error.to_string())
                    } else {
                        None
                    };
                    if owns_runtime_shutdown {
                        finalize_shutdown(
                            &shared,
                            &tx,
                            &stream_sequence,
                            Some(&projection),
                            shutdown_error.as_deref(),
                        );
                    }
                    match shutdown_error {
                        Some(error) => {
                            Err(anyhow!("{reason}; forced runtime shutdown failed: {error}"))
                        }
                        None => Err(anyhow!(reason)),
                    }
                }
            };
            if let Err(error) = &result {
                eprintln!("[mink-server] turn task ended abnormally: {error:#}");
            }
            result
        }));
        drop(state);
        self.publish_inputs();
        Ok(receipt)
    }

    pub fn session_id(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtime
            .as_ref()
            .map(|runtime| runtime.session_info().session_id.clone())
            .unwrap_or_default()
    }

    pub fn interrupt(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(
            state.phase,
            RuntimePhase::Running | RuntimePhase::Cancelling
        ) {
            state.phase = RuntimePhase::Cancelling;
            publish(
                &self.event_tx,
                &self.stream_sequence,
                &self.projection,
                serde_json::json!({"type":"phase_updated","phase":"cancelling"}),
            );
            if let Some(runtime) = &state.runtime {
                runtime.interrupt_current_turn();
            }
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        let (mut task, runtime) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.phase == RuntimePhase::Closed {
                return Ok(());
            }
            state.phase = RuntimePhase::Closing;
            publish(
                &self.event_tx,
                &self.stream_sequence,
                &self.projection,
                serde_json::json!({"type":"phase_updated","phase":"closing"}),
            );
            if let Some(runtime) = &state.runtime {
                runtime.interrupt_current_turn();
            }
            (state.turn_task.take(), state.runtime.take())
        };
        let turn_join_grace = if runtime.is_none() {
            // The timeout cleanup task may already own AgentRuntime::shutdown,
            // whose own bounded turn/actor cleanup can exceed five seconds.
            OWNED_SHUTDOWN_JOIN_GRACE
        } else {
            TURN_SHUTDOWN_GRACE
        };
        let mut errors = Vec::new();
        if let Some(handle) = task.as_mut() {
            match tokio::time::timeout(turn_join_grace, handle).await {
                Ok(result) => {
                    task = None;
                    record_turn_join(result, &mut errors);
                }
                Err(_) => {
                    errors.push(format!(
                        "turn did not stop within {}s",
                        turn_join_grace.as_secs()
                    ));
                }
            }
        }
        let shutdown_error = if let Some(runtime) = runtime {
            runtime
                .shutdown()
                .await
                .err()
                .map(|error| error.to_string())
        } else {
            None
        };
        if let Some(error) = &shutdown_error {
            errors.push(format!("runtime shutdown failed: {error}"));
        }
        if let Some(mut handle) = task {
            match tokio::time::timeout(TURN_JOIN_GRACE, &mut handle).await {
                Ok(result) => record_turn_join(result, &mut errors),
                Err(_) => {
                    handle.abort();
                    match handle.await {
                        Err(error) if error.is_cancelled() => {}
                        result => record_turn_join(result, &mut errors),
                    }
                    errors.push("turn task required forced abort after runtime shutdown".into());
                }
            }
        }
        finalize_shutdown(
            &self.state,
            &self.event_tx,
            &self.stream_sequence,
            Some(&self.projection),
            shutdown_error.as_deref(),
        );
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(errors.join("; ")))
        }
    }
}

fn finalize_shutdown(
    state: &Arc<Mutex<RuntimeState>>,
    tx: &broadcast::Sender<Arc<BroadcastEvent>>,
    stream_sequence: &AtomicU64,
    projection: Option<&Arc<Mutex<Projection>>>,
    shutdown_error: Option<&str>,
) {
    let terminal = {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        state.phase = RuntimePhase::Closed;
        state.forced_terminal.take()
    };
    let Some(terminal) = terminal else {
        if let Some(projection) = projection {
            publish(
                tx,
                stream_sequence,
                projection,
                serde_json::json!({"type":"phase_updated","phase":"closed"}),
            );
        }
        return;
    };
    let error = match shutdown_error {
        Some(shutdown_error) => format!(
            "{}; forced runtime shutdown failed: {shutdown_error}",
            terminal.reason
        ),
        None => terminal.reason,
    };
    publish_forced_timeout_final(
        tx,
        stream_sequence,
        &terminal.turn_id,
        &terminal.session,
        &error,
        (terminal.saw_stop, terminal.saw_final),
        projection,
    );
    if let Some(projection) = projection {
        publish(
            tx,
            stream_sequence,
            projection,
            serde_json::json!({"type":"phase_updated","phase":"closed"}),
        );
    }
}

fn record_turn_join(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
    errors: &mut Vec<String>,
) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => errors.push(format!("turn task failed: {error:#}")),
        Err(error) => errors.push(format!("turn task panicked: {error}")),
    }
}
