use anyhow::{Result, anyhow};
use mink::runtime::{
    AgentEvent, AgentEventStream, AgentOptions, AgentRuntime, GoalSnapshot, SessionInfo,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

const TURN_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
const TURN_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(1);
/// Snapshot tail policy: at most this many recent user turns, and a soft byte
/// ceiling that drops older whole turns first. The newest turn is always kept
/// even when it alone exceeds the ceiling, so a tool exchange is never split.
const SNAPSHOT_RECENT_TURNS: usize = 4;
const SNAPSHOT_MAX_BYTES: usize = 256 * 1024;
const OWNED_SHUTDOWN_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(12);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePhase {
    Idle,
    Running,
    Cancelling,
    Closing,
    Closed,
}

enum GuardedTurnResult {
    Finished(Result<()>),
    ForceClose {
        reason: String,
        saw_stop: bool,
        saw_final: bool,
        turn_id: String,
        session: Box<SessionInfo>,
    },
}

struct RuntimeState {
    runtime: Option<AgentRuntime>,
    phase: RuntimePhase,
    turn_task: Option<JoinHandle<Result<()>>>,
    last_idle: Instant,
    forced_terminal: Option<ForcedTerminal>,
}

struct ForcedTerminal {
    reason: String,
    saw_stop: bool,
    saw_final: bool,
    turn_id: String,
    session: Box<SessionInfo>,
}

struct Projection {
    generation: String,
    watermark: u64,
    rows: Vec<serde_json::Value>,
    /// Serialized row sizes, aligned with `rows`, so trimming does not
    /// re-serialize the whole window on every commit.
    row_bytes: Vec<usize>,
    /// Older rows were dropped from the projection; the reader can page them
    /// back through the conversation endpoint.
    has_older: bool,
    progress: Vec<serde_json::Value>,
    current_turn: Option<String>,
    phase: &'static str,
    last_final: Option<serde_json::Value>,
    resources: serde_json::Value,
    diagnostics: serde_json::Value,
    activity: serde_json::Value,
    /// Goal mirror rendered into snapshots. It lives here, next to the
    /// watermark and the `goal_updated` payload, so a snapshot never has to
    /// reach for the state lock while holding the projection: the lock order
    /// state -> projection stays one-way and cannot invert.
    goal: Option<mink::runtime::GoalSnapshot>,
}
impl Projection {
    /// Seed a newly opened projection. Only the retained tail is serialized:
    /// computing row sizes for the whole 17 MB conversation just to drop most
    /// of it made opening a long session slower than the snapshot itself.
    fn trim_initial(&mut self) {
        let boundaries: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row["role"] == "user"
                    && row["content"].is_string()
                    && row["internal"] != true
                    && row.pointer("/_mink/guidance") != Some(&serde_json::Value::Bool(true))
            })
            .map(|(i, _)| i)
            .collect();
        if boundaries.len() > SNAPSHOT_RECENT_TURNS {
            let start = boundaries[boundaries.len() - SNAPSHOT_RECENT_TURNS];
            self.rows.drain(..start);
            self.has_older = start > 0;
        }
        self.row_bytes = self
            .rows
            .iter()
            .map(|row| {
                serde_json::to_vec(row)
                    .map(|bytes| bytes.len())
                    .unwrap_or(0)
            })
            .collect();
        self.trim();
    }

    fn trim(&mut self) {
        let boundaries: Vec<_> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row["role"] == "user"
                    && row["content"].is_string()
                    && row["internal"] != true
                    && row.pointer("/_mink/guidance") != Some(&serde_json::Value::Bool(true))
            })
            .map(|(i, _)| i)
            .collect();
        if boundaries.is_empty() {
            return;
        }
        // Start at the oldest of the last N turns; when those still exceed the
        // byte ceiling, move forward one whole turn at a time. The final turn
        // is never dropped, so a tool call/result exchange stays intact.
        let mut first = boundaries.len().saturating_sub(SNAPSHOT_RECENT_TURNS);
        let mut start = if boundaries.len() > SNAPSHOT_RECENT_TURNS {
            boundaries[first]
        } else {
            0
        };
        let bytes_from = |from: usize| self.row_bytes[from..].iter().sum::<usize>();
        while first + 1 < boundaries.len() && bytes_from(start) > SNAPSHOT_MAX_BYTES {
            first += 1;
            start = boundaries[first];
        }
        if start > 0 {
            self.rows.drain(..start);
            self.row_bytes.drain(..start);
            self.has_older = true;
        }
    }
}

pub struct SessionRuntime {
    projection: Arc<Mutex<Projection>>,
    inbox: Arc<mink::runtime::InputInbox>,
    image_input: mink::runtime::ImageInputCapability,
    attachments: Arc<mink::runtime::AttachmentStore>,
    goal_dir: std::path::PathBuf,
    state: Arc<Mutex<RuntimeState>>,
    event_tx: broadcast::Sender<String>,
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
            let reader = mink::runtime::session::SessionReader::new(read_dir);
            Ok((
                reader.conversation()?,
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
        let goal = runtime.handle().goal_snapshot().filter(|goal| !goal.hidden);
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
            has_older: false,
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
        let mut goal_updates = runtime.handle().subscribe_goal();
        let mut goal_turns = runtime.handle().subscribe_goal_turns();
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
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.phase == RuntimePhase::Idle).then(|| state.last_idle.elapsed())
    }
    pub fn event_receiver(&self) -> broadcast::Receiver<String> {
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
        if let Some(goal) = self.goal_handle().and_then(|handle| handle.goal_snapshot()) {
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
    pub fn detail(&self) -> serde_json::Value {
        let (generation, current_turn, phase, last_final, goal) = {
            let p = self.projection.lock().unwrap_or_else(|e| e.into_inner());
            (
                p.generation.clone(),
                p.current_turn.clone(),
                p.phase,
                p.last_final.clone(),
                p.goal.clone(),
            )
        };
        serde_json::json!({"generation":generation,"current_turn":current_turn,"phase":phase,"capabilities":{"image_input":self.image_input},"last_final":last_final,"goal":goal.as_ref().filter(|g| !g.hidden).map(goal_summary)})
    }
    pub fn snapshot_subscription(&self) -> (serde_json::Value, broadcast::Receiver<String>) {
        // Clone the projection under its lock, then serialize outside it: a
        // multi-megabyte snapshot must not hold off the live publishers (and
        // the turn task) while serde walks every row.
        let (
            generation,
            watermark,
            rows,
            has_older,
            progress,
            current_turn,
            phase,
            goal,
            last_final,
            resources,
            diagnostics,
            activity,
            inputs,
            rx,
        ) = {
            let p = self.projection.lock().unwrap_or_else(|e| e.into_inner());
            // Subscribing inside the same critical section is what makes the
            // snapshot watermark and the first delivered event contiguous.
            let rx = self.event_tx.subscribe();
            let inputs = self.inbox.outstanding();
            (
                p.generation.clone(),
                p.watermark,
                p.rows.clone(),
                p.has_older,
                p.progress.clone(),
                p.current_turn.clone(),
                p.phase,
                p.goal.clone(),
                p.last_final.clone(),
                p.resources.clone(),
                p.diagnostics.clone(),
                p.activity.clone(),
                inputs,
                rx,
            )
        };
        (
            serde_json::json!({"type":"session_snapshot","generation":generation,"stream_sequence":watermark,"conversation":rows,"has_older":has_older,"progress":progress,"current_turn":current_turn,"phase":phase,"running":matches!(phase,"running"|"cancelling"|"closing"),"inputs":inputs,"goal":goal.as_ref().filter(|g| !g.hidden).map(goal_summary),"capabilities":{"image_input":self.image_input},"last_final":last_final,"resources":resources,"diagnostics":diagnostics,"activity":activity}),
            rx,
        )
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
    tx: &broadcast::Sender<String>,
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

fn agent_event_to_json(event: &AgentEvent, stream_sequence: u64) -> serde_json::Value {
    let mut value = serde_json::to_value(event)
        .unwrap_or_else(|_| serde_json::json!({"type":"serialization_error"}));
    value["stream_sequence"] = stream_sequence.into();
    if value.get("type").and_then(serde_json::Value::as_str) == Some("final") {
        value["type"] = "turn_final".into();
    }
    // Core emits its own GoalUpdated/GoalFinished events during a turn; the
    // server also mirrors the goal watch channel. Both carry the full contract,
    // which can be hundreds of kilobytes of reviews and usage records. The
    // stream carries the same summary on both paths, and the detail tab asks
    // for the full snapshot explicitly.
    if matches!(
        value.get("type").and_then(serde_json::Value::as_str),
        Some("goal_updated" | "goal_finished")
    ) && let Some(goal) = value.get("goal").cloned()
    {
        value["goal"] = goal_summary_value(&goal);
    }
    let kind = value_type(&value).map(str::to_string);
    if let Some(object) = value.as_object_mut() {
        match kind.as_deref() {
            Some("tool_result") => {
                // The workbench renders the complete tool row from the formal
                // history; the live event only needs the content field.
                object.remove("content_preview");
            }
            Some("sub_agent_output") => {
                // A sub-agent's final text is displayed as a summary card; the
                // full body already lives in the child session. Bound both
                // streams so one child cannot dominate the live SSE payload.
                for key in ["text", "thinking"] {
                    if let Some(text) = object.get(key).and_then(serde_json::Value::as_str) {
                        object.insert(key.into(), bounded_text(text, 32 * 1024).into());
                    }
                }
            }
            _ => {}
        }
    }
    value
}

fn value_type(value: &serde_json::Value) -> Option<&str> {
    value.get("type").and_then(serde_json::Value::as_str)
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated {} bytes]", &text[..end], text.len() - end)
}

/// Goal fields the live bar and status wording consume. Heavy history stays on
/// the explicit detail read; the stream and the session snapshot remain small.
fn goal_summary(goal: &GoalSnapshot) -> serde_json::Value {
    serde_json::to_value(goal)
        .map(|value| goal_summary_value(&value))
        .unwrap_or(serde_json::Value::Null)
}

pub(crate) fn goal_summary_value(goal: &serde_json::Value) -> serde_json::Value {
    let Some(mut object) = goal.as_object().cloned() else {
        return goal.clone();
    };
    let iterations = object
        .remove("iterations")
        .and_then(|value| value.as_array().map(Vec::len))
        .unwrap_or(0);
    object.insert("iterations_count".into(), iterations.into());
    for key in [
        "usage_records",
        "checker_records",
        "checker_evidence_versions",
        "evidence_versions",
        "checker_passed_candidate",
        "review_report",
        "report",
        "environment",
    ] {
        object.remove(key);
    }
    for key in ["reviews", "continuations"] {
        if let Some(array) = object
            .get_mut(key)
            .and_then(serde_json::Value::as_array_mut)
        {
            let last = array.last().cloned();
            array.clear();
            if let Some(last) = last {
                array.push(last);
            }
        }
    }
    serde_json::Value::Object(object)
}

fn publish(
    tx: &broadcast::Sender<String>,
    sequence: &AtomicU64,
    projection: &Arc<Mutex<Projection>>,
    value: serde_json::Value,
) {
    let mut p = projection.lock().unwrap_or_else(|e| e.into_inner());
    publish_locked(tx, sequence, &mut p, value);
}

/// Publish a goal update and advance the mirrored goal in the same critical
/// section, so `snapshot_subscription` and `detail` can render the goal
/// without touching session state.
///
/// A cleared (`hidden`) goal is normalized to null here: the mirror, the
/// session snapshot/detail projections and the `goal_updated` event must all
/// agree that a cleared goal is gone, matching the REST read.
fn publish_goal(
    tx: &broadcast::Sender<String>,
    sequence: &AtomicU64,
    projection: &Arc<Mutex<Projection>>,
    goal: Option<mink::runtime::GoalSnapshot>,
) {
    let goal = goal.filter(|goal| !goal.hidden);
    let mut p = projection.lock().unwrap_or_else(|e| e.into_inner());
    p.goal = goal.clone();
    publish_locked(
        tx,
        sequence,
        &mut p,
        serde_json::json!({"type":"goal_updated","goal":goal.as_ref().map(goal_summary)}),
    );
}

fn publish_locked(
    tx: &broadcast::Sender<String>,
    sequence: &AtomicU64,
    p: &mut Projection,
    mut value: serde_json::Value,
) {
    value["generation"] = p.generation.clone().into();
    let seq = sequence.fetch_add(1, Ordering::Relaxed);
    value["stream_sequence"] = seq.into();
    p.watermark = seq;
    value["after_conversation_seq"] = p
        .rows
        .last()
        .and_then(|row| row.get("seq"))
        .cloned()
        .unwrap_or(serde_json::json!(0));
    update_activity(&mut p.activity, &value);
    match value["type"].as_str().unwrap_or_default() {
        "title_update" => {
            p.diagnostics = value.clone();
        }
        "conversation_committed" => {
            let mut row = value["message"].clone();
            row["seq"] = value["conversation_seq"].clone();
            if row["role"] == "assistant" {
                p.progress
                    .retain(|e| !matches!(e["type"].as_str(), Some("text" | "thinking")));
            }
            if let Some(blocks) = row["content"].as_array() {
                for block in blocks {
                    if let Some(artifacts) = block
                        .pointer("/_mink/artifacts")
                        .and_then(serde_json::Value::as_array)
                    {
                        let existing = p.resources["artifacts"]
                            .as_array_mut()
                            .expect("artifact snapshot is an array");
                        for artifact in artifacts {
                            if !existing.iter().any(|row| row["id"] == artifact["id"]) {
                                existing.push(artifact.clone());
                            }
                        }
                    }
                    if let Some(presentation) = block.pointer("/_mink/presentation") {
                        apply_presentation(&mut p.resources, presentation);
                    }
                }
            }
            let row_bytes = serde_json::to_vec(&row)
                .map(|bytes| bytes.len())
                .unwrap_or(0);
            p.rows.push(row);
            p.row_bytes.push(row_bytes);
            p.trim();
        }
        "phase_updated" => {
            p.phase = match value["phase"].as_str() {
                Some("cancelling") => "cancelling",
                Some("closing") => "closing",
                Some("closed") => "closed",
                Some("running") => "running",
                Some("idle") => "idle",
                _ => p.phase,
            };
        }
        "turn_started" => {
            p.phase = "running";
            p.current_turn = value["turn_id"].as_str().map(str::to_string);
        }
        "turn_final" => {
            if !matches!(p.phase, "closing" | "closed") {
                p.phase = "idle";
            }
            p.current_turn = None;
            p.progress.clear();
            p.last_final = Some(value.clone());
        }
        "text" | "thinking" => {
            if let Some(last) = p.progress.last_mut().filter(|e| e["type"] == value["type"]) {
                let text = format!(
                    "{}{}",
                    last["content"].as_str().unwrap_or_default(),
                    value["content"].as_str().unwrap_or_default()
                );
                last["content"] = text.into();
            } else {
                p.progress.push(value.clone());
            }
        }
        "signal" | "error" | "sub_agent_status" | "sub_agent_output" | "retry"
        | "context_compact" => {
            p.progress.push(value.clone());
        }
        _ => {}
    }
    let _ = tx.send(value.to_string());
}

fn update_activity(activity: &mut serde_json::Value, event: &serde_json::Value) {
    if matches!(event["type"].as_str(), Some("turn_started" | "turn_final")) {
        activity["active_sub_agents"] = serde_json::json!([]);
    }
    let work = match event["type"].as_str().unwrap_or_default() {
        "turn_started" | "retry" | "tool_result" => Some("waiting"),
        "thinking" => Some("thinking"),
        "text" => Some("generating"),
        "tool_call" => Some("tool"),
        "sub_agent_status" | "sub_agent_output" => {
            if !activity["active_sub_agents"].is_array() {
                activity["active_sub_agents"] = serde_json::json!([]);
            }
            let agents = activity["active_sub_agents"].as_array_mut().unwrap();
            let id = &event["session_id"];
            if matches!(event["status"].as_str(), Some("launched" | "running")) {
                if id.is_string() && !agents.contains(id) {
                    agents.push(id.clone());
                }
            } else if event["type"] == "sub_agent_output"
                || matches!(
                    event["status"].as_str(),
                    Some("ok" | "failed" | "timed_out" | "cancelled" | "channel_closed")
                )
            {
                agents.retain(|agent| agent != id);
            }
            Some(if agents.is_empty() {
                "waiting"
            } else {
                "sub-agent"
            })
        }
        "context_compact" => Some("compacting"),
        "turn_final" => Some(
            if event["outcome"]["status"] != "interrupted"
                && event
                    .pointer("/outcome/error")
                    .is_some_and(serde_json::Value::is_string)
            {
                "error"
            } else {
                "idle"
            },
        ),
        "info" => {
            let message = event["message"].as_str().unwrap_or_default();
            if let Some(elapsed) = mink::runtime::parse_llm_wait_heartbeat_elapsed(message) {
                activity["wait_elapsed_secs"] = elapsed.into();
            }
            (message == "Compressing...").then_some("compacting")
        }
        _ => None,
    };
    if let Some(work) = work {
        activity["work_state"] = work.into();
        activity["wait_elapsed_secs"] = serde_json::Value::Null;
    }
}

fn apply_presentation(resources: &mut serde_json::Value, presentation: &serde_json::Value) {
    let data = &presentation["data"];
    match presentation["kind"].as_str() {
        Some("plan") => match data["transition"].as_str() {
            Some("confirmed") => {
                resources["plan"] = serde_json::json!({"plan":data["content"],"draft":null})
            }
            Some("cleared") => resources["plan"] = serde_json::json!({"plan":null,"draft":null}),
            Some("draft_saved") => resources["plan"]["draft"] = data["content"].clone(),
            Some("draft_cancelled") => resources["plan"]["draft"] = serde_json::Value::Null,
            _ => {}
        },
        Some("todo") if data["revision"].as_u64() > resources["todo"]["revision"].as_u64() => {
            let mut items = resources["todo"]["items"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for change in data["changes"].as_array().into_iter().flatten() {
                let id = change["id"]
                    .as_str()
                    .or_else(|| change["item"]["id"].as_str());
                match change["change"].as_str() {
                    Some("removed") => items.retain(|item| item["id"].as_str() != id),
                    Some("added") => items.push(change["item"].clone()),
                    Some(action) => {
                        if let Some(item) = items.iter_mut().find(|item| item["id"].as_str() == id)
                        {
                            match action {
                                "updated" => item["content"] = change["content"].clone(),
                                "completed" => item["status"] = "completed".into(),
                                "activated" => item["status"] = "in_progress".into(),
                                "paused" | "reopened" => item["status"] = "pending".into(),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            for item in data["items"].as_array().into_iter().flatten() {
                if let Some(existing) = items
                    .iter_mut()
                    .find(|existing| existing["id"] == item["id"])
                {
                    *existing = item.clone();
                } else {
                    items.push(item.clone());
                }
            }
            resources["todo"]["items"] = items.into();
            resources["todo"]["revision"] = data["revision"].clone();
        }
        _ => {}
    }
}

async fn run_turn_guarded(
    tx: &broadcast::Sender<String>,
    stream_sequence: &AtomicU64,
    projection: &Arc<Mutex<Projection>>,
    state: &Arc<Mutex<RuntimeState>>,
    mut stream: AgentEventStream,
    session: SessionInfo,
) -> GuardedTurnResult {
    let timeout_secs = std::env::var("MINK_SERVER_TURN_TIMEOUT")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1200);
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
    tokio::pin!(deadline);
    let mut saw_stop = false;
    let mut saw_final = false;
    let turn_id = stream.turn_id().to_string();
    loop {
        tokio::select! {
            event = stream.recv() => match event {
                Some(event) => {
                    saw_stop |= matches!(&event.kind, mink::runtime::AgentEventKind::Stop { .. });
                    saw_final |= matches!(&event.kind, mink::runtime::AgentEventKind::Final { .. });
                    publish(tx,stream_sequence,projection,agent_event_to_json(&event,0));
                }
                None => {
                    return GuardedTurnResult::Finished(stream.outcome().await.map(|_| ()).map_err(Into::into));
                },
            },
            _ = &mut deadline => {
                {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.phase == RuntimePhase::Running {
                        state.phase = RuntimePhase::Cancelling;
                        publish(tx,stream_sequence,projection,serde_json::json!({"type":"phase_updated","phase":"cancelling"}));
                    }
                }
                stream.cancel();
                publish(tx,stream_sequence,projection,serde_json::json!({
                    "type":"turn_error",
                    "error":"turn timed out"
                }));
                break;
            }
        }
    }

    let grace = async {
        while let Some(event) = stream.recv().await {
            saw_stop |= matches!(&event.kind, mink::runtime::AgentEventKind::Stop { .. });
            saw_final |= matches!(&event.kind, mink::runtime::AgentEventKind::Final { .. });
            publish(
                tx,
                stream_sequence,
                projection,
                agent_event_to_json(&event, 0),
            );
        }
        stream.outcome().await
    };
    match tokio::time::timeout(TURN_SHUTDOWN_GRACE, grace).await {
        Ok(Ok(outcome)) => GuardedTurnResult::Finished(Err(anyhow!(
            "turn timed out after {timeout_secs}s (ended as {:?})",
            outcome.status
        ))),
        Ok(Err(error)) => GuardedTurnResult::Finished(Err(anyhow!(
            "turn timed out after {timeout_secs}s and cancellation failed: {error}"
        ))),
        Err(_) => GuardedTurnResult::ForceClose {
            reason: format!(
                "turn timed out after {timeout_secs}s and did not stop within {}s",
                TURN_SHUTDOWN_GRACE.as_secs()
            ),
            saw_stop,
            saw_final,
            turn_id,
            session: Box::new(session),
        },
    }
}

fn publish_forced_timeout_final(
    tx: &broadcast::Sender<String>,
    stream_sequence: &AtomicU64,
    turn_id: &str,
    session: &SessionInfo,
    error: &str,
    observed: (bool, bool),
    projection: Option<&Arc<Mutex<Projection>>>,
) {
    let (saw_stop, saw_final) = observed;
    let deliver = |mut value: serde_json::Value| {
        if let Some(projection) = projection {
            publish(tx, stream_sequence, projection, value);
        } else {
            value["stream_sequence"] = stream_sequence.fetch_add(1, Ordering::Relaxed).into();
            let _ = tx.send(value.to_string());
        }
    };
    if !saw_stop {
        let sequence = 0;
        deliver(serde_json::json!({
            "type": "stop",
            "turn_id": turn_id,
            "reason": "timeout",
            "stream_sequence": sequence,
        }));
    }
    if !saw_final {
        let sequence = 0;
        deliver(serde_json::json!({
        "type": "turn_final",
        "turn_id": turn_id,
        "stream_sequence": sequence,
        "outcome": {
            "turn_id": turn_id,
            "billing_turn_id": turn_id,
            "status": "failed",
            "session": session,
            "text": "",
            "thinking": "",
            "tool_call_count": 0,
            "tool_error_count": 1,
            "error": error,
            "usage_records": [],
            "usage": {
                "request_count": 0,
                "reported_request_count": 0,
                "unreported_request_count": 0,
                "attempt_count": 0,
                "tokens": {
                    "input_tokens": 0,
                    "cache_read_tokens": 0,
                    "cache_creation_tokens": 0,
                    "output_tokens": 0
                }
            }
        }
        }));
    }
}

/// Forwards goal-driven turn events into the same ordered SSE projection that
/// ordinary turns use.
///
/// It deliberately owns no lifecycle state: phase and idle time come from the
/// core goal-turn signal through the session's own `RuntimeState`, so a second
/// source of truth cannot drift from execution control.
struct GoalTurnSink {
    tx: broadcast::Sender<String>,
    sequence: Arc<AtomicU64>,
    projection: Arc<OnceLock<Arc<Mutex<Projection>>>>,
}

#[async_trait::async_trait]
impl mink::runtime::EventSink for GoalTurnSink {
    async fn on_event(&self, event: mink::runtime::AgentEvent) -> Result<(), String> {
        let mut value = serde_json::to_value(&event)
            .unwrap_or_else(|_| serde_json::json!({"type":"serialization_error"}));
        if value["type"] == "final" {
            value["type"] = "turn_final".into();
        }
        match self.projection.get() {
            Some(projection) => publish(&self.tx, &self.sequence, projection, value),
            None => {
                let mut payload = value;
                payload["stream_sequence"] = self.sequence.fetch_add(1, Ordering::SeqCst).into();
                let _ = self.tx.send(payload.to_string());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_text_keeps_utf8_boundaries_and_marks_the_cut() {
        let text = "汉".repeat(20);
        let bounded = bounded_text(&text, 4);
        assert!(bounded.starts_with("汉"));
        assert!(bounded.contains("truncated"));
        assert!(bounded.len() < text.len());
        assert_eq!(bounded_text("short", 100), "short");
    }

    fn projection(rows: Vec<serde_json::Value>) -> Projection {
        let row_bytes = rows
            .iter()
            .map(|row| {
                serde_json::to_vec(row)
                    .map(|bytes| bytes.len())
                    .unwrap_or(0)
            })
            .collect();
        Projection {
            generation: "g".into(),
            watermark: 0,
            rows,
            row_bytes,
            has_older: false,
            progress: vec![],
            current_turn: None,
            phase: "idle",
            last_final: None,
            resources: serde_json::json!({}),
            diagnostics: serde_json::json!({}),
            activity: serde_json::json!({}),
            goal: None,
        }
    }

    fn turn(n: usize, body_bytes: usize) -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({ "seq": n, "role": "user", "content": format!("prompt {n}") }),
            serde_json::json!({
                "seq": n + 1,
                "role": "assistant",
                "content": [{ "type": "text", "text": "x".repeat(body_bytes) }]
            }),
        ]
    }

    #[test]
    fn snapshot_tail_keeps_recent_turns_and_never_splits_the_latest() {
        let small: Vec<_> = (1..=6).flat_map(|n| turn(n * 2, 10)).collect();
        let mut p = projection(small);
        p.trim();
        assert!(p.has_older);
        assert_eq!(p.rows.first().unwrap()["seq"], 6);
        assert_eq!(p.rows.iter().filter(|row| row["role"] == "user").count(), 4);

        let large: Vec<_> = (1..=6).flat_map(|n| turn(n * 2, 100 * 1024)).collect();
        let mut p = projection(large);
        p.trim();
        assert!(p.has_older);
        assert!(p.rows.first().unwrap()["seq"].as_u64().unwrap() > 1);
        assert_eq!(p.rows.last().unwrap()["role"], "assistant");

        let mut rows = turn(2, 300 * 1024);
        rows.extend(turn(4, 300 * 1024));
        let mut p = projection(rows);
        p.trim();
        assert!(p.has_older);
        assert_eq!(
            p.rows.first().unwrap()["seq"],
            4,
            "the newest turn stays whole"
        );
        assert_eq!(p.rows.last().unwrap()["role"], "assistant");
    }

    #[test]
    fn opening_a_long_projection_serializes_only_the_retained_tail() {
        let rows: Vec<_> = (1..=8).flat_map(|n| turn(n * 2, 10)).collect();
        let mut p = projection(rows);
        p.trim_initial();
        assert!(p.has_older);
        assert_eq!(p.rows.iter().filter(|row| row["role"] == "user").count(), 4);
        assert_eq!(p.row_bytes.len(), p.rows.len());
    }

    #[test]
    fn goal_summary_drops_heavy_history_and_keeps_bar_fields() {
        let goal = serde_json::json!({
            "id": "g1",
            "revision": 7,
            "status": "active",
            "phase": "idle",
            "spec": {"objective": "ship it"},
            "iterations": [{"n": 1}, {"n": 2}],
            "reviews": [{"verdict": "partial"}, {"verdict": "satisfied"}],
            "continuations": [{"n": 1}, {"n": 2}],
            "usage_records": [{"tokens": 1}],
            "checker_records": [{"ok": true}],
            "review_report": {"long": "x"},
            "report": {"long": "y"},
            "review_candidate": {"iteration": 1},
            "environment": "host",
            "pending_effects": []
        });
        let summary = goal_summary_value(&goal);
        assert_eq!(summary["id"], "g1");
        assert_eq!(summary["iterations_count"], 2);
        assert_eq!(summary["reviews"].as_array().unwrap().len(), 1);
        assert_eq!(summary["reviews"][0]["verdict"], "satisfied");
        assert_eq!(summary["continuations"].as_array().unwrap().len(), 1);
        for key in [
            "iterations",
            "usage_records",
            "checker_records",
            "review_report",
            "report",
            "environment",
        ] {
            assert!(
                summary.get(key).is_none(),
                "{key} must not be streamed: {summary}"
            );
        }
        assert_eq!(summary["review_candidate"]["iteration"], 1);
    }

    #[test]
    fn activity_tracks_reliable_progress_and_clears_wait_at_boundaries() {
        let mut activity = serde_json::json!({"work_state":"idle","wait_elapsed_secs":null});
        update_activity(&mut activity, &serde_json::json!({"type":"turn_started"}));
        update_activity(
            &mut activity,
            &serde_json::json!({"type":"info","message":"Waiting for model response... elapsed=23s idle=5s"}),
        );
        assert_eq!(activity["work_state"], "waiting");
        assert_eq!(activity["wait_elapsed_secs"], 23);
        // Formal history commits and stats must not erase current activity.
        update_activity(
            &mut activity,
            &serde_json::json!({"type":"conversation_committed"}),
        );
        assert_eq!(activity["wait_elapsed_secs"], 23);
        for (event, work) in [
            (serde_json::json!({"type":"thinking"}), "thinking"),
            (serde_json::json!({"type":"text"}), "generating"),
            (serde_json::json!({"type":"tool_call"}), "tool"),
            (serde_json::json!({"type":"tool_result"}), "waiting"),
            (
                serde_json::json!({"type":"sub_agent_status","session_id":"child","status":"running"}),
                "sub-agent",
            ),
            (
                serde_json::json!({"type":"sub_agent_output","session_id":"child"}),
                "waiting",
            ),
            (
                serde_json::json!({"type":"info","message":"Compressing..."}),
                "compacting",
            ),
            (
                serde_json::json!({"type":"turn_final","outcome":{"error":"cancelled"}}),
                "error",
            ),
            (
                serde_json::json!({"type":"turn_final","outcome":{"error":null}}),
                "idle",
            ),
        ] {
            update_activity(&mut activity, &event);
            assert_eq!(activity["work_state"], work);
            assert!(activity["wait_elapsed_secs"].is_null());
        }
    }

    #[test]
    fn activity_keeps_remaining_children_running_and_interruption_idle() {
        let mut activity = serde_json::json!({"work_state":"idle","wait_elapsed_secs":null});
        for id in ["first", "second"] {
            update_activity(
                &mut activity,
                &serde_json::json!({"type":"sub_agent_status","session_id":id,"status":"launched"}),
            );
        }
        update_activity(
            &mut activity,
            &serde_json::json!({"type":"sub_agent_output","session_id":"first","status":"ok"}),
        );
        assert_eq!(activity["work_state"], "sub-agent");
        assert_eq!(activity["active_sub_agents"], serde_json::json!(["second"]));
        update_activity(
            &mut activity,
            &serde_json::json!({"type":"turn_final","outcome":{"status":"interrupted","error":"interrupted"}}),
        );
        assert_eq!(activity["work_state"], "idle");
        assert_eq!(activity["active_sub_agents"], serde_json::json!([]));
    }

    fn session_info() -> SessionInfo {
        let path = std::path::PathBuf::from("/tmp/mink-server-runtime-test");
        SessionInfo {
            session_id: "session".into(),
            session_ref: "session".into(),
            is_new: false,
            home: path.clone(),
            cwd: path.clone(),
            events_path: path.join("events.jsonl"),
            conversation_path: path.join("conversation.jsonl"),
            artifacts_dir: path.join("artifacts"),
            summary_path: path.join("summary.md"),
            usage_path: path.join("usage.jsonl"),
            plan_path: path.join("plan.md"),
            plan_draft_path: path.join("plan.draft"),
            todos_path: path.join("todos.json"),
        }
    }

    #[test]
    fn forced_terminal_is_published_once_after_closed() {
        let (tx, mut rx) = broadcast::channel(8);
        let sequence = AtomicU64::new(1);
        let state = Arc::new(Mutex::new(RuntimeState {
            runtime: None,
            phase: RuntimePhase::Closing,
            turn_task: None,
            last_idle: Instant::now(),
            forced_terminal: Some(ForcedTerminal {
                reason: "turn timed out".into(),
                saw_stop: false,
                saw_final: false,
                turn_id: "turn".into(),
                session: Box::new(session_info()),
            }),
        }));

        finalize_shutdown(&state, &tx, &sequence, None, Some("shutdown error"));
        assert_eq!(state.lock().unwrap().phase, RuntimePhase::Closed);
        let stop: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        let final_event: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(stop["type"], "stop");
        assert_eq!(final_event["type"], "turn_final");
        assert_eq!(final_event["outcome"]["status"], "failed");
        assert!(
            final_event["outcome"]["error"]
                .as_str()
                .unwrap()
                .contains("shutdown error")
        );

        finalize_shutdown(&state, &tx, &sequence, None, None);
        assert!(rx.try_recv().is_err());
    }
    /// Regression for the audit's lock-order inversion: interrupt/shutdown
    /// hold the state lock and then publish (projection), so snapshot/detail
    /// must never hold the projection while waiting for state. The goal is
    /// rendered from the mirrored projection instead of a live state read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_and_detail_do_not_wait_for_the_state_lock() {
        let dir =
            std::env::temp_dir().join(format!("mink-server-runtime-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = Arc::new(
            SessionRuntime::open(AgentOptions::new(&dir, &dir).with_api_key("synthetic"))
                .await
                .unwrap(),
        );
        // Simulate interrupt/shutdown owning the state lock.
        let state = runtime.state.lock().unwrap();
        let worker_runtime = runtime.clone();
        let worker = std::thread::spawn(move || {
            let (snapshot, _rx) = worker_runtime.snapshot_subscription();
            let detail = worker_runtime.detail();
            (snapshot, detail)
        });
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !worker.is_finished() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let finished = worker.is_finished();
        drop(state);
        assert!(
            finished,
            "snapshot/detail must not wait on the state lock held by interrupt/shutdown"
        );
        let (snapshot, detail) = worker.join().unwrap();
        assert_eq!(snapshot["type"], "session_snapshot");
        assert!(detail.get("goal").is_some());
        runtime.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn goal_mirror_rides_the_snapshot_watermark() {
        let dir = std::env::temp_dir().join(format!(
            "mink-server-runtime-goal-mirror-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = SessionRuntime::open(AgentOptions::new(&dir, &dir).with_api_key("synthetic"))
            .await
            .unwrap();
        let goal = mink::runtime::GoalSnapshot {
            version: 1,
            draft_duration_secs: None,
            needs_confirmation: false,
            review_candidate: None,
            review_generation: 0,
            checker_evidence_versions: Vec::new(),
            reviews: Vec::new(),
            checker_passed_candidate: None,
            checker_records: Vec::new(),
            confirmed_after_iteration: 0,
            draft_ready: true,
            id: "goal-mirror".into(),
            revision: 1,
            contract_revision: 1,
            spec: mink::runtime::GoalSpec::new("mirror the goal"),
            status: mink::runtime::GoalStatus::Paused,
            phase: mink::runtime::GoalPhase::Idle,
            reason: None,
            review_failure_reason: None,
            review_failure_stage: None,
            review_stage: None,
            review_requires_full: false,
            review_escalation_reason: None,
            review_report: None,
            continuations: Vec::new(),
            continuation_fingerprint: None,
            continuation_rounds: 0,
            completion_basis: None,
            iterations: vec![],
            report: None,
            report_repair_used: false,
            usage_records: vec![],
            usage: Default::default(),
            pending_effects: vec![],
            failure_fingerprint: None,
            repeated_failures: 0,
            blocked_rounds: 0,
            blocker_rounds: 0,
            blocker_fingerprint: None,
            evidence_versions: vec![],
            acceptance_conversation_seq: 0,
            environment: "test".into(),
            hidden: false,
        };
        publish_goal(
            &runtime.event_tx,
            &runtime.stream_sequence,
            &runtime.projection,
            Some(goal.clone()),
        );
        let (snapshot, _rx) = runtime.snapshot_subscription();
        let watermark = runtime.projection.lock().unwrap().watermark;
        assert_eq!(snapshot["stream_sequence"], watermark);
        assert_eq!(snapshot["goal"]["id"], "goal-mirror");
        assert_eq!(runtime.detail()["goal"]["id"], "goal-mirror");
        // A cleared goal is normalized to null in the mirror, the snapshot and
        // the published event alike.
        let mut events = runtime.event_tx.subscribe();
        let mut hidden = goal.clone();
        hidden.hidden = true;
        publish_goal(
            &runtime.event_tx,
            &runtime.stream_sequence,
            &runtime.projection,
            Some(hidden),
        );
        let mut goal_event = None;
        while let Ok(message) = events.try_recv() {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&message)
                && value["type"] == "goal_updated"
            {
                goal_event = Some(value);
            }
        }
        let goal_event = goal_event.expect("goal_updated event");
        assert!(
            goal_event["goal"].is_null(),
            "a cleared goal leaked into the event: {goal_event}"
        );
        let (snapshot, _rx) = runtime.snapshot_subscription();
        assert!(snapshot["goal"].is_null());
        assert!(runtime.detail()["goal"].is_null());
        runtime.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The audit's second direction: interrupt takes state and then waits for
    /// the projection, so a snapshot must be able to finish under the same
    /// contention without ever waiting for state. Both operations are bounded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshot_and_interrupt_finish_under_projection_contention() {
        let dir = std::env::temp_dir().join(format!(
            "mink-server-runtime-contention-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = Arc::new(
            SessionRuntime::open(AgentOptions::new(&dir, &dir).with_api_key("synthetic"))
                .await
                .unwrap(),
        );
        // The interrupt path must actually publish, so mark the session as
        // running before holding the projection it will wait for. No state
        // lock may be taken while the projection is held, or this test would
        // recreate the inversion it guards against.
        runtime.state.lock().unwrap().phase = RuntimePhase::Running;
        let projection = runtime.projection.lock().unwrap();
        let snapshot_runtime = runtime.clone();
        let snapshot = std::thread::spawn(move || snapshot_runtime.snapshot_subscription().0);
        let interrupt_runtime = runtime.clone();
        let interrupt = std::thread::spawn(move || interrupt_runtime.interrupt());
        // Both are serialised behind this release; the old snapshot path would
        // deadlock here, holding the projection while waiting for the state
        // lock interrupt took first.
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(projection);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !(snapshot.is_finished() && interrupt.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            snapshot.is_finished(),
            "snapshot must finish with the interrupt"
        );
        assert!(
            interrupt.is_finished(),
            "interrupt must finish with the snapshot"
        );
        let snapshot = snapshot.join().unwrap();
        interrupt.join().unwrap();
        assert_eq!(snapshot["type"], "session_snapshot");
        assert_eq!(runtime.phase(), RuntimePhase::Cancelling);
        runtime.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resource_projection_applies_todo_deltas_without_losing_completed_items() {
        let mut resources = serde_json::json!({"plan":{"plan":null,"draft":null},"todo":{"revision":1,"items":[{"id":"done","content":"old","status":"completed"},{"id":"work","content":"new","status":"in_progress"}]}});
        apply_presentation(
            &mut resources,
            &serde_json::json!({"kind":"todo","data":{"revision":2,"items":[],"changes":[{"change":"completed","id":"work"}]}}),
        );
        assert_eq!(resources["todo"]["items"].as_array().unwrap().len(), 2);
        assert_eq!(resources["todo"]["items"][1]["status"], "completed");
        apply_presentation(
            &mut resources,
            &serde_json::json!({"kind":"todo","data":{"revision":1,"items":[],"changes":[{"change":"removed","id":"done"}]}}),
        );
        assert_eq!(resources["todo"]["items"].as_array().unwrap().len(), 2);
        apply_presentation(
            &mut resources,
            &serde_json::json!({"kind":"plan","data":{"transition":"confirmed","content":"plan"}}),
        );
        assert_eq!(resources["plan"]["plan"], "plan");
        assert!(resources["plan"]["draft"].is_null());
    }
}
