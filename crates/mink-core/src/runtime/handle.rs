use crate::agent::orchestrator::{OrchCmd, TurnRunResult, TurnStatus};
use crate::context::AgentSharedContext;
use crate::runtime::config::SessionInfo;
use crate::runtime::events::{EventDisplay, TurnEventEmitter};
use crate::runtime::{AgentEvent, AgentEventKind, AgentOptions};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(String);

impl TurnId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Busy { active_turn_id: TurnId },
    Closed,
    Command(String),
    Join(String),
    ShutdownTimeout,
    Configuration(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy { active_turn_id } => {
                write!(f, "runtime is busy with turn {active_turn_id}")
            }
            Self::Closed => f.write_str("runtime is closed"),
            Self::Command(message) => write!(f, "runtime command failed: {message}"),
            Self::Join(message) => write!(f, "runtime task failed: {message}"),
            Self::ShutdownTimeout => f.write_str("runtime shutdown timed out"),
            Self::Configuration(message) => write!(f, "runtime configuration failed: {message}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

pub type RuntimeResult<T> = std::result::Result<T, RuntimeError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CompactOutcome {
    Compacted { reason: String },
    Skipped { reason: String },
}

/// Result of a successful model switch: the resolved human-facing label and
/// the title snapshot captured after the switch. Control callers (CLI/TUI)
/// render these directly because they have no turn emitter to carry the
/// title update.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSwitchOutcome {
    pub label: String,
    pub stats: crate::ui::StatsSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnOutcome {
    pub turn_id: TurnId,
    pub billing_turn_id: String,
    pub status: TurnStatus,
    pub session: SessionInfo,
    pub text: String,
    pub thinking: String,
    pub tool_call_count: u32,
    pub tool_error_count: u32,
    pub error: Option<String>,
    pub usage_records: Vec<crate::session::usage::UsageRecord>,
    pub usage: crate::session::usage::UsageSummary,
}

impl TurnOutcome {
    pub(crate) fn from_run_result(
        result: TurnRunResult,
        session: SessionInfo,
        turn_id: TurnId,
    ) -> Self {
        Self {
            turn_id,
            billing_turn_id: result.billing_turn_id,
            status: result.status,
            session,
            text: result.text,
            thinking: result.thinking,
            tool_call_count: result.tool_call_count,
            tool_error_count: result.tool_error_count,
            error: result.error,
            usage_records: result.usage_records,
            usage: result.usage,
        }
    }
}

pub(crate) struct TurnGate {
    state: Mutex<TurnGateState>,
    interrupt: Arc<AtomicBool>,
    idle: tokio::sync::Notify,
}

#[derive(Default)]
struct TurnGateState {
    closed: bool,
    active: Option<TurnId>,
}

impl TurnGate {
    fn acquire(self: &Arc<Self>, turn_id: TurnId) -> RuntimeResult<TurnPermit> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        if let Some(active_turn_id) = state.active.clone() {
            return Err(RuntimeError::Busy { active_turn_id });
        }
        self.interrupt.store(false, Ordering::SeqCst);
        state.active = Some(turn_id.clone());
        Ok(TurnPermit {
            gate: self.clone(),
            turn_id,
        })
    }

    fn active(&self) -> Option<TurnId> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .clone()
    }

    fn interrupt(&self) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.is_some() {
            self.interrupt.store(true, Ordering::SeqCst);
        }
    }

    fn interrupt_if_owner(&self, turn_id: &TurnId) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.as_ref() == Some(turn_id) {
            self.interrupt.store(true, Ordering::SeqCst);
        }
    }

    fn close_and_interrupt(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        let had_active = state.active.is_some();
        if had_active {
            self.interrupt.store(true, Ordering::SeqCst);
        }
        had_active
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.active().is_none() {
                return;
            }
            notified.await;
        }
    }
}

struct TurnPermit {
    gate: Arc<TurnGate>,
    turn_id: TurnId,
}

impl Drop for TurnPermit {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.as_ref() == Some(&self.turn_id) {
            state.active = None;
            self.gate.idle.notify_waiters();
        }
    }
}

pub struct AgentEventStream {
    turn_id: TurnId,
    rx: mpsc::UnboundedReceiver<AgentEvent>,
    handle: Option<JoinHandle<RuntimeResult<TurnOutcome>>>,
    gate: Arc<TurnGate>,
    finished: bool,
    /// Shared with the emitter: pending progress bytes are bounded, and
    /// consumption (recv/outcome drain) releases them.
    progress_budget: Arc<crate::runtime::events::ProgressBudget>,
}

impl AgentEventStream {
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        let event = self.rx.recv().await;
        if let Some(event) = &event
            && let Some(bytes) = event.kind.progress_len()
        {
            self.progress_budget.release(bytes);
        }
        event
    }

    pub fn cancel(&self) {
        self.gate.interrupt_if_owner(&self.turn_id);
    }

    /// Diagnostics for the live progress budget.
    ///
    /// Returns `(pending_progress_bytes, dropped_progress_deltas)`. Reliable
    /// events are never counted or dropped; the bound applies to coalescible
    /// Text/Thinking progress only.
    pub fn progress_backlog(&self) -> (usize, u64) {
        (
            self.progress_budget.pending(),
            self.progress_budget.dropped(),
        )
    }

    /// Wait for the final outcome, draining events meanwhile.
    ///
    /// Consumers that only need the result never accumulate an unbounded
    /// backlog: pending progress bytes are released as events are drained.
    pub async fn outcome(mut self) -> RuntimeResult<TurnOutcome> {
        let mut handle = self
            .handle
            .take()
            .ok_or_else(|| RuntimeError::Join("missing turn task".into()))?;
        let result = loop {
            tokio::select! {
                joined = &mut handle => {
                    break joined.map_err(|e| RuntimeError::Join(e.to_string()))?;
                }
                event = self.rx.recv() => {
                    if let Some(event) = &event
                        && let Some(bytes) = event.kind.progress_len()
                    {
                        self.progress_budget.release(bytes);
                    }
                }
            }
        };
        self.finished = true;
        result
    }
}

impl Drop for AgentEventStream {
    fn drop(&mut self) {
        if !self.finished {
            self.cancel();
        }
    }
}

pub struct AgentRuntime {
    pub(crate) ctx: Arc<AgentSharedContext>,
    pub(crate) orch_handle: JoinHandle<anyhow::Result<()>>,
    pub(crate) event_display: Arc<EventDisplay>,
    pub(crate) handle: AgentRuntimeHandle,
}

#[derive(Clone)]
pub struct AgentRuntimeHandle {
    pub(crate) cmd_tx: mpsc::UnboundedSender<OrchCmd>,
    pub(crate) session: SessionInfo,
    pub(crate) event_display: Arc<EventDisplay>,
    pub(crate) turn_gate: Arc<TurnGate>,
    pub(crate) turn_counter: Arc<AtomicU64>,
}

impl AgentRuntimeHandle {
    fn next_turn_id(&self) -> TurnId {
        let counter = self.turn_counter.fetch_add(1, Ordering::Relaxed) + 1;
        TurnId(format!("{}:{counter}", self.session.session_id))
    }

    pub async fn run_turn(&self, input: impl Into<String>) -> RuntimeResult<TurnOutcome> {
        let mut stream = self.stream_turn(input)?;
        while stream.recv().await.is_some() {}
        stream.outcome().await
    }

    pub fn stream_turn(&self, input: impl Into<String>) -> RuntimeResult<AgentEventStream> {
        let turn_id = self.next_turn_id();
        let permit = self.turn_gate.acquire(turn_id.clone())?;
        let stream_turn_id = turn_id.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        let progress_budget = crate::runtime::events::ProgressBudget::new(
            crate::runtime::events::PROGRESS_PENDING_BYTES_LIMIT,
        );
        let emitter = Arc::new(
            TurnEventEmitter::new(turn_id.clone(), Some(tx), self.event_display.dispatcher())
                .with_progress_budget(progress_budget.clone()),
        );
        self.event_display.begin_turn(emitter.clone());
        let (done_tx, done_rx) = oneshot::channel();
        if let Err(error) = self.cmd_tx.send(OrchCmd::UserInput {
            input: input.into(),
            turn_id: turn_id.clone(),
            emitter: emitter.clone(),
            done: done_tx,
        }) {
            self.event_display.end_turn(&turn_id);
            return Err(RuntimeError::Command(error.to_string()));
        }
        let session = self.session.clone();
        let event_display = self.event_display.clone();
        let handle = tokio::spawn(async move {
            let _permit = permit;
            let result = done_rx.await.map_err(|e| {
                RuntimeError::Command(format!("orchestrator dropped turn result: {e}"))
            });
            let outcome = result.map(|result| {
                let outcome = TurnOutcome::from_run_result(result, session, turn_id.clone());
                emitter.emit(AgentEventKind::Final {
                    outcome: Box::new(outcome.clone()),
                });
                outcome
            });
            event_display.end_turn(&turn_id);
            outcome
        });
        Ok(AgentEventStream {
            turn_id: stream_turn_id,
            rx,
            handle: Some(handle),
            gate: self.turn_gate.clone(),
            finished: false,
            progress_budget,
        })
    }

    /// Run a manual compaction.
    ///
    /// The busy permit is owned by the spawned operation task until the
    /// orchestrator reports completion: dropping this future only abandons
    /// waiting, it does not cancel the operation or release the gate early.
    pub async fn compact(&self) -> RuntimeResult<CompactOutcome> {
        let operation_id = self.next_turn_id();
        let permit = self.turn_gate.acquire(operation_id)?;
        // No `.await` between acquiring the permit, sending the command and
        // handing the permit to the task: cancellation cannot interleave
        // while the operation is still owned by this future. A send failure
        // returns before the task exists, so the local permit drops here.
        let (done_tx, done_rx) = oneshot::channel();
        self.cmd_tx
            .send(OrchCmd::Compact { done: done_tx })
            .map_err(|e| RuntimeError::Command(e.to_string()))?;
        let task = tokio::spawn(async move {
            let _permit = permit;
            done_rx
                .await
                .map_err(|e| RuntimeError::Command(e.to_string()))?
                .map_err(|e| RuntimeError::Command(format!("{e:#}")))
        });
        task.await.map_err(|e| RuntimeError::Join(e.to_string()))?
    }

    /// Switch the active model.
    ///
    /// Same permit ownership contract as [`Self::compact`]: the permit lives
    /// in the spawned operation task until the orchestrator reports
    /// completion, so dropping the caller future only abandons waiting.
    pub async fn set_model(&self, model: impl Into<String>) -> RuntimeResult<()> {
        self.set_model_with_outcome(model).await.map(|_| ())
    }

    /// Switch the active model and return the resolved label plus the title
    /// snapshot captured after the switch.
    ///
    /// Additive companion to [`Self::set_model`] for control UIs that must
    /// update the displayed model label without re-resolving aliases.
    pub async fn set_model_with_outcome(
        &self,
        model: impl Into<String>,
    ) -> RuntimeResult<ModelSwitchOutcome> {
        let operation_id = self.next_turn_id();
        let permit = self.turn_gate.acquire(operation_id)?;
        let (done_tx, done_rx) = oneshot::channel();
        self.cmd_tx
            .send(OrchCmd::SetModel {
                model: model.into(),
                done: done_tx,
            })
            .map_err(|e| RuntimeError::Command(e.to_string()))?;
        let task = tokio::spawn(async move {
            let _permit = permit;
            done_rx
                .await
                .map_err(|e| RuntimeError::Command(e.to_string()))?
                .map_err(|e| RuntimeError::Command(format!("{e:#}")))
        });
        task.await.map_err(|e| RuntimeError::Join(e.to_string()))?
    }

    pub fn interrupt_current_turn(&self) {
        self.turn_gate.interrupt();
    }

    pub fn session_info(&self) -> &SessionInfo {
        &self.session
    }
}

/// Per-stage budget for shutdown flush/teardown work.
#[cfg(not(test))]
const SHUTDOWN_STAGE_BUDGET: Duration = Duration::from_secs(5);
#[cfg(test)]
const SHUTDOWN_STAGE_BUDGET: Duration = Duration::from_millis(200);

/// Run one shutdown stage under a deadline, recording failures/timeouts.
///
/// "Call returned" and "background writes finished" are different promises:
/// a timed-out stage is reported, never silently claimed complete.
/// Run one blocking shutdown stage on the blocking pool under a deadline.
///
/// Wrapping a sync `flush` in `async {}` would not make it cancellable: the
/// blocking work must own a blocking-pool thread so the async worker stays
/// responsive and the deadline bounds only the caller's wait.
async fn flush_stage_blocking<F>(
    budget: Duration,
    stage: &str,
    work: F,
    failures: &mut Vec<String>,
    timed_out: &mut bool,
) where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    flush_stage(
        budget,
        stage,
        async move {
            tokio::task::spawn_blocking(work)
                .await
                .map_err(|error| anyhow::anyhow!("blocking stage task failed: {error}"))?
        },
        failures,
        timed_out,
    )
    .await;
}

async fn flush_stage<T, E: std::fmt::Display>(
    budget: Duration,
    stage: &str,
    future: impl std::future::Future<Output = Result<T, E>>,
    failures: &mut Vec<String>,
    timed_out: &mut bool,
) {
    match tokio::time::timeout(budget, future).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => failures.push(format!("{stage} failed: {error}")),
        Err(_) => {
            *timed_out = true;
            failures.push(format!("{stage} timed out after {budget:?}"));
        }
    }
}

impl AgentRuntime {
    pub async fn start(options: AgentOptions) -> RuntimeResult<Self> {
        crate::runtime::build_runtime(options.into_runtime_config())
            .await
            .map_err(|e| RuntimeError::Configuration(format!("{e:#}")))
    }

    pub fn handle(&self) -> AgentRuntimeHandle {
        self.handle.clone()
    }

    pub async fn run_turn(&self, input: impl Into<String>) -> RuntimeResult<TurnOutcome> {
        self.handle.run_turn(input).await
    }

    pub fn stream_turn(&self, input: impl Into<String>) -> RuntimeResult<AgentEventStream> {
        self.handle.stream_turn(input)
    }

    pub async fn compact(&self) -> RuntimeResult<CompactOutcome> {
        self.handle.compact().await
    }

    pub async fn set_model(&self, model: impl Into<String>) -> RuntimeResult<()> {
        self.handle.set_model(model).await
    }

    /// Switch the active model and return the resolved label plus the title
    /// snapshot captured after the switch (see
    /// [`AgentRuntimeHandle::set_model_with_outcome`]).
    pub async fn set_model_with_outcome(
        &self,
        model: impl Into<String>,
    ) -> RuntimeResult<ModelSwitchOutcome> {
        self.handle.set_model_with_outcome(model).await
    }

    pub fn interrupt_current_turn(&self) {
        self.handle.interrupt_current_turn();
    }

    pub async fn shutdown(self) -> RuntimeResult<()> {
        let had_active = self.handle.turn_gate.close_and_interrupt();
        let turn_timed_out = had_active
            && tokio::time::timeout(Duration::from_secs(5), self.handle.turn_gate.wait_idle())
                .await
                .is_err();
        self.ctx.cancel.cancel();
        let mut failures = Vec::new();
        let mut shutdown_timed_out = turn_timed_out;
        let mut orchestrator = self.orch_handle;
        if turn_timed_out {
            orchestrator.abort();
            if let Err(error) = orchestrator.await
                && !error.is_cancelled()
            {
                failures.push(format!("orchestrator abort failed: {error}"));
            }
        } else {
            match tokio::time::timeout(Duration::from_secs(5), &mut orchestrator).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => {
                    failures.push(format!("orchestrator shutdown failed: {error:#}"));
                }
                Ok(Err(error)) => failures.push(format!("runtime task failed: {error}")),
                Err(_) => {
                    shutdown_timed_out = true;
                    orchestrator.abort();
                    if let Err(error) = orchestrator.await
                        && !error.is_cancelled()
                    {
                        failures.push(format!("orchestrator abort failed: {error}"));
                    }
                }
            }
        }
        if let Some(dispatcher) = self.event_display.dispatcher()
            && let Err(error) = dispatcher.shutdown().await
        {
            failures.push(format!("event dispatcher shutdown failed: {error}"));
        }
        flush_stage(
            SHUTDOWN_STAGE_BUDGET,
            "event log flush",
            self.ctx.flush_event_log(),
            &mut failures,
            &mut shutdown_timed_out,
        )
        .await;
        let usage = self.ctx.usage.clone();
        flush_stage_blocking(
            SHUTDOWN_STAGE_BUDGET,
            "usage flush",
            move || usage.flush(),
            &mut failures,
            &mut shutdown_timed_out,
        )
        .await;
        flush_stage(
            SHUTDOWN_STAGE_BUDGET,
            "stats flush",
            self.ctx.stats.flush(),
            &mut failures,
            &mut shutdown_timed_out,
        )
        .await;
        flush_stage(
            SHUTDOWN_STAGE_BUDGET,
            "compaction projection flush",
            self.ctx.compaction.flush_projection(),
            &mut failures,
            &mut shutdown_timed_out,
        )
        .await;
        if failures.is_empty() && !shutdown_timed_out {
            Ok(())
        } else if failures.is_empty() {
            Err(RuntimeError::ShutdownTimeout)
        } else {
            if shutdown_timed_out {
                failures.insert(0, RuntimeError::ShutdownTimeout.to_string());
            }
            Err(RuntimeError::Command(failures.join("; ")))
        }
    }

    pub fn session_info(&self) -> &SessionInfo {
        self.handle.session_info()
    }
}

pub(crate) fn new_turn_gate(interrupt: Arc<AtomicBool>) -> Arc<TurnGate> {
    Arc::new(TurnGate {
        state: Mutex::new(TurnGateState::default()),
        interrupt,
        idle: tokio::sync::Notify::new(),
    })
}

#[cfg(test)]
mod shutdown_stage_tests {
    use super::*;

    #[tokio::test]
    async fn blocking_stage_timeout_bounds_caller() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let release = Arc::new(AtomicBool::new(false));
        let worker_release = release.clone();
        let mut failures = Vec::new();
        let mut timed_out = false;
        let started = std::time::Instant::now();
        flush_stage_blocking(
            Duration::from_millis(50),
            "blocking unit stage",
            move || {
                while !worker_release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(())
            },
            &mut failures,
            &mut timed_out,
        )
        .await;
        assert!(timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "caller must not wait for the blocking work: {:?}",
            started.elapsed()
        );
        assert!(
            failures.iter().any(|line| line.contains("timed out")),
            "{failures:?}"
        );
        release.store(true, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn stage_timeout_is_bounded_and_reported() {
        let mut failures = Vec::new();
        let mut timed_out = false;
        let started = std::time::Instant::now();
        flush_stage(
            Duration::from_millis(50),
            "unit stage",
            async { futures::future::pending::<anyhow::Result<()>>().await },
            &mut failures,
            &mut timed_out,
        )
        .await;
        assert!(timed_out);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            failures.iter().any(|line| line.contains("timed out")),
            "{failures:?}"
        );
    }

    #[tokio::test]
    async fn stage_errors_are_collected_without_timeout_flag() {
        let mut failures = Vec::new();
        let mut timed_out = false;
        flush_stage(
            Duration::from_millis(50),
            "unit stage",
            async { Err::<(), _>(anyhow::anyhow!("injected failure")) },
            &mut failures,
            &mut timed_out,
        )
        .await;
        assert!(!timed_out);
        assert!(
            failures
                .iter()
                .any(|line| line.contains("injected failure")),
            "{failures:?}"
        );
    }
}
