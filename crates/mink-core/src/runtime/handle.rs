use crate::agent::failure::TurnFailureReason;
use crate::agent::orchestrator::{OrchCmd, TurnRunResult, TurnStatus};
use crate::context::AgentSharedContext;
use crate::runtime::config::SessionInfo;
use crate::runtime::events::{EventDisplay, TurnEventEmitter};
use crate::runtime::{AgentEvent, AgentEventKind, AgentOptions};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
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
    ExternallyControlled { owner: &'static str },
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
            Self::ExternallyControlled { owner } => {
                write!(f, "{owner} owns session; pause before control commands")
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

fn usize_is_zero(value: &usize) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnOutcome {
    pub turn_id: TurnId,
    pub billing_turn_id: String,
    pub status: TurnStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<TurnFailureReason>,
    pub session: SessionInfo,
    pub text: String,
    pub thinking: String,
    pub tool_call_count: u32,
    pub tool_error_count: u32,
    pub error: Option<String>,
    pub usage_records: Vec<crate::session::usage::UsageRecord>,
    pub usage: crate::session::usage::UsageSummary,
    #[serde(default, skip_serializing_if = "usize_is_zero")]
    pub start_seq: usize,
    #[serde(default, skip_serializing_if = "usize_is_zero")]
    pub end_seq: usize,
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
            failure: result.failure,
            session,
            text: result.text,
            thinking: result.thinking,
            tool_call_count: result.tool_call_count,
            tool_error_count: result.tool_error_count,
            error: result.error,
            usage_records: result.usage_records,
            usage: result.usage,
            start_seq: result.start_seq,
            end_seq: result.end_seq,
        }
    }
}

pub(crate) struct TurnGate {
    state: Mutex<TurnGateState>,
    interrupt: crate::cancel::InterruptSignal,
    idle: tokio::sync::Notify,
}

#[derive(Default)]
struct TurnGateState {
    closed: bool,
    active: Option<TurnId>,
}

impl TurnGate {
    fn acquire(self: &Arc<Self>, turn_id: TurnId) -> RuntimeResult<TurnGatePermit> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        if let Some(active_turn_id) = state.active.clone() {
            return Err(RuntimeError::Busy { active_turn_id });
        }
        self.interrupt.reset();
        state.active = Some(turn_id.clone());
        Ok(TurnGatePermit {
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

    pub(crate) fn interrupt(&self) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.is_some() {
            self.interrupt.trigger();
        }
    }

    pub(super) fn interrupt_if_owner(&self, turn_id: &TurnId) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.as_ref() == Some(turn_id) {
            self.interrupt.trigger();
        }
    }

    fn close_and_interrupt(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        let had_active = state.active.is_some();
        if had_active {
            self.interrupt.trigger();
        }
        had_active
    }

    pub(crate) async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.active().is_none() {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) struct TurnGatePermit {
    gate: Arc<TurnGate>,
    pub(crate) turn_id: TurnId,
}

impl Drop for TurnGatePermit {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.as_ref() == Some(&self.turn_id) {
            state.active = None;
            self.gate.idle.notify_waiters();
        }
    }
}

/// Exclusive single-turn execution permit held across internal or verification
/// turns until the execution stream or closure releases it.
pub struct TurnPermit(pub(crate) TurnGatePermit);

impl TurnPermit {
    pub fn turn_id(&self) -> &TurnId {
        &self.0.turn_id
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
    pub(crate) _lease: Arc<crate::session::lease::SessionLease>,
    pub(crate) orch_handle: Option<JoinHandle<anyhow::Result<()>>>,
    pub(crate) event_display: Arc<EventDisplay>,
    pub(crate) handle: AgentRuntimeHandle,
    pub(crate) shutdown_called: bool,
}

impl Drop for AgentRuntime {
    fn drop(&mut self) {
        if self.shutdown_called {
            return;
        }
        let controller = self
            .handle
            .external_control
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .controller
            .take();
        if let Some(controller) = &controller {
            controller.on_shutdown_disable();
        }
        self.ctx.lifecycle.mark_closing();
        let _ = self
            .handle
            .external_control
            .stop_cause
            .send_replace(Some(crate::runtime::ext::StopCause::RuntimeShutdown));
        self.ctx.cancel.cancel();
        self.handle.turn_gate.close_and_interrupt();
        if let Some(controller) = &controller {
            controller.on_shutdown_close();
        }
        self.ctx.lifecycle.dispose();
        if let Some(orch_handle) = self.orch_handle.take() {
            orch_handle.abort();
        }
        self.ctx
            .stores
            .session_lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}

#[derive(Clone)]
pub struct AgentRuntimeHandle {
    pub(crate) input_inbox: Arc<crate::session::input::InputInbox>,
    pub(crate) ctx: Arc<AgentSharedContext>,
    pub(crate) external_control: Arc<crate::runtime::internal_turn::ExternalControlState>,
    pub(crate) image_input: crate::runtime::ImageInputCapability,
    pub(crate) cmd_tx: mpsc::UnboundedSender<OrchCmd>,
    pub(crate) session: SessionInfo,
    pub(crate) event_display: Arc<EventDisplay>,
    pub(crate) turn_gate: Arc<TurnGate>,
    pub(crate) turn_counter: Arc<AtomicU64>,
}

impl AgentRuntimeHandle {
    pub(crate) fn next_turn_id(&self) -> TurnId {
        let counter = self.turn_counter.fetch_add(1, Ordering::Relaxed) + 1;
        TurnId(format!(
            "{}:{}:{counter}",
            self.session.session_id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
    }

    pub async fn run_turn(&self, input: impl Into<String>) -> RuntimeResult<TurnOutcome> {
        let mut stream = self.stream_turn(input)?;
        while stream.recv().await.is_some() {}
        stream.outcome().await
    }

    pub fn stream_turn(&self, input: impl Into<String>) -> RuntimeResult<AgentEventStream> {
        let input = crate::runtime::HumanInput {
            request_id: format!(
                "legacy-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            text: input.into(),
            target_turn_id: None,
            attachment_ids: vec![],
        };
        self.stream_input_inner(input, None, false)
            .map(|(_, stream)| stream.expect("new legacy input"))
    }

    pub fn input_inbox(&self) -> &Arc<crate::runtime::InputInbox> {
        &self.input_inbox
    }
    pub fn image_input(&self) -> &crate::runtime::ImageInputCapability {
        &self.image_input
    }

    pub fn stream_input(
        &self,
        input: crate::runtime::HumanInput,
    ) -> RuntimeResult<(crate::runtime::InputReceipt, Option<AgentEventStream>)> {
        self.stream_input_inner(input, None, true)
    }
    pub fn resume_input(
        &self,
        id: &str,
        revision: u64,
    ) -> RuntimeResult<(crate::runtime::InputReceipt, Option<AgentEventStream>)> {
        let receipt = self
            .input_inbox
            .entries()
            .into_iter()
            .find(|e| e.input_id == id)
            .ok_or_else(|| RuntimeError::Command("input not found".into()))?;
        self.stream_input_inner(receipt.input, Some((id, revision)), true)
    }
    fn stream_input_inner(
        &self,
        input: crate::runtime::HumanInput,
        resume: Option<(&str, u64)>,
        register: bool,
    ) -> RuntimeResult<(crate::runtime::InputReceipt, Option<AgentEventStream>)> {
        let _admission = self
            .external_control
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.is_closed() {
            return Err(RuntimeError::Closed);
        }
        if input.target_turn_id.is_none()
            && let Some(owner) = _admission.blocking_owner()
        {
            return Err(RuntimeError::ExternallyControlled { owner });
        }
        if register
            && resume.is_none()
            && let Some(receipt) = self
                .input_inbox
                .existing(&input)
                .map_err(|e| RuntimeError::Command(e.to_string()))?
        {
            return Ok((receipt, None));
        }
        if input.target_turn_id.is_some() && resume.is_none() {
            if self.turn_gate.interrupt.is_set() {
                return Err(RuntimeError::Command("turn is stopping".into()));
            }
            let receipt = self
                .input_inbox
                .accept(input, None)
                .map_err(|e| RuntimeError::Command(e.to_string()))?;
            return Ok((receipt, None));
        }
        let turn_id = self.next_turn_id();
        let permit = self.turn_gate.acquire(turn_id.clone())?;
        let receipt = self.register_submission_receipt(&input, &turn_id, resume, register)?;
        self.dispatch_turn_stream(input.text, turn_id, permit, None, false, None, receipt)
    }

    fn register_submission_receipt(
        &self,
        input: &crate::runtime::HumanInput,
        turn_id: &TurnId,
        resume: Option<(&str, u64)>,
        register: bool,
    ) -> RuntimeResult<crate::runtime::InputReceipt> {
        if !register {
            return Ok(crate::runtime::InputReceipt::transient(
                input.clone(),
                turn_id.as_str(),
            ));
        }
        match resume {
            Some((id, revision)) => self.input_inbox.resume(id, revision, turn_id.as_str()),
            None => self
                .input_inbox
                .accept(input.clone(), Some(turn_id.as_str())),
        }
        .map_err(|e| RuntimeError::Command(e.to_string()))
    }

    fn rollback_unstarted_submission(
        &self,
        on_rollback: Option<&crate::runtime::ext::TurnRollbackHook>,
        inbox_started: bool,
    ) {
        if inbox_started {
            let _ = self.input_inbox.finish();
        }
        if let Some(hook) = on_rollback {
            hook();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_turn_stream(
        &self,
        input: String,
        turn_id: TurnId,
        permit: TurnGatePermit,
        internal: Option<crate::agent::orchestrator::InternalTurnExecution>,
        inbox_started: bool,
        on_rollback: Option<crate::runtime::ext::TurnRollbackHook>,
        receipt: crate::runtime::InputReceipt,
    ) -> RuntimeResult<(crate::runtime::InputReceipt, Option<AgentEventStream>)> {
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
            input,
            internal: internal.map(Box::new),
            turn_id: turn_id.clone(),
            emitter: emitter.clone(),
            done: done_tx,
        }) {
            self.event_display.end_turn(&turn_id);
            self.rollback_unstarted_submission(on_rollback.as_ref(), inbox_started);
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
        Ok((
            receipt,
            Some(AgentEventStream {
                turn_id: stream_turn_id,
                rx,
                handle: Some(handle),
                gate: self.turn_gate.clone(),
                finished: false,
                progress_budget,
            }),
        ))
    }

    /// Run a manual compaction.
    ///
    /// The busy permit is owned by the spawned operation task until the
    /// orchestrator reports completion: dropping this future only abandons
    /// waiting, it does not cancel the operation or release the gate early.
    pub async fn compact(&self) -> RuntimeResult<CompactOutcome> {
        let permit = {
            let guard = self
                .external_control
                .admission
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(owner) = guard.blocking_owner() {
                return Err(RuntimeError::ExternallyControlled { owner });
            }
            self.turn_gate.acquire(self.next_turn_id())?
        };
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

    /// The model in force right now, preserving the accepted alias when present.
    pub fn current_model(&self) -> String {
        self.ctx
            .active_model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .requested
            .clone()
    }

    /// The concrete model in force right now.
    pub fn resolved_model(&self) -> String {
        self.ctx
            .active_model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .actual
            .clone()
    }

    /// Alias choices this runtime honours: the built-in pair plus the configured
    /// aliases, each with the model it resolves to.
    pub fn model_choices(&self) -> Vec<(String, String)> {
        crate::config::model_resolver(&self.ctx.config)
            .aliases()
            .iter()
            .map(|(alias, model)| (alias.clone(), model.clone()))
            .collect()
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
        let permit = {
            let guard = self
                .external_control
                .admission
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(owner) = guard.blocking_owner() {
                return Err(RuntimeError::ExternallyControlled { owner });
            }
            self.turn_gate.acquire(self.next_turn_id())?
        };
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
        let controller = {
            let guard = self
                .external_control
                .admission
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if self.ext().is_closed() {
                None
            } else {
                guard.controller.clone()
            }
        };
        if let Some(controller) = controller {
            controller.on_user_interrupt();
        }
        let _ = self
            .external_control
            .stop_cause
            .send_replace(Some(crate::runtime::ext::StopCause::UserInterrupt));
        self.turn_gate.interrupt();
    }

    pub fn session_info(&self) -> &SessionInfo {
        &self.session
    }

    /// Access advanced/experimental orchestrator SPI methods.
    pub fn ext(&self) -> RuntimeExtHandle<'_> {
        RuntimeExtHandle(self)
    }

    fn is_closed(&self) -> bool {
        self.ext().is_closed()
    }
}

/// Extension SPI handle for external session orchestrators (such as `mink-goal`).
#[derive(Clone, Copy)]
pub struct RuntimeExtHandle<'a>(pub(crate) &'a AgentRuntimeHandle);

impl<'a> RuntimeExtHandle<'a> {
    pub fn attach_external_controller(
        &self,
        controller: Arc<dyn crate::runtime::ext::ExternalController>,
    ) -> RuntimeResult<()> {
        let mut guard = self
            .0
            .external_control
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.is_closed() {
            return Err(RuntimeError::Closed);
        }
        if let Some(existing) = &guard.controller
            && let Some(owner) = existing.blocks_ordinary_control()
        {
            return Err(RuntimeError::ExternallyControlled { owner });
        }
        guard.controller = Some(controller);
        Ok(())
    }

    /// Run `f` inside a short, non-reentrant critical section serialized
    /// against human turn admission (`stream_input`/`compact`/`set_model`).
    /// Callers must not invoke runtime admission methods or emit events inside `f`.
    pub fn with_admission_lock<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self
            .0
            .external_control
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        f()
    }

    pub fn acquire_turn_permit(&self, prefix: Option<&str>) -> RuntimeResult<TurnPermit> {
        self.acquire_turn_permit_when(prefix, || Ok(()))
    }

    pub fn acquire_turn_permit_when(
        &self,
        prefix: Option<&str>,
        check: impl FnOnce() -> RuntimeResult<()>,
    ) -> RuntimeResult<TurnPermit> {
        let _guard = self
            .0
            .external_control
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.is_closed() {
            return Err(RuntimeError::Closed);
        }
        self.0
            .ctx
            .stores
            .persistence_fault
            .check()
            .map_err(|e| RuntimeError::Command(format!("{e:#}")))?;
        check()?;
        let base = self.0.next_turn_id();
        let turn_id = match prefix {
            Some(prefix) => TurnId::new(format!("{prefix}:{base}")),
            None => base,
        };
        Ok(TurnPermit(self.0.turn_gate.acquire(turn_id)?))
    }

    pub async fn wait_turn_idle(&self) {
        self.0.turn_gate.wait_idle().await;
    }

    pub fn subscribe_stop_cause(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<crate::runtime::ext::StopCause>> {
        self.0.external_control.stop_cause.subscribe()
    }

    pub fn active_turn_id(&self) -> Option<TurnId> {
        self.0.turn_gate.active()
    }

    pub fn is_closed(&self) -> bool {
        self.0
            .turn_gate
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.ctx.cancel.is_cancelled()
    }

    pub fn interrupt_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.0.ctx.interrupt.as_atomic()
    }

    pub fn is_interrupted(&self) -> bool {
        self.0.ctx.interrupt.is_set()
    }

    pub fn interrupt_turn_gate(&self) {
        self.0.turn_gate.interrupt();
    }

    pub fn interrupt_turn_if_owner(&self, expected: &TurnId) {
        self.0.turn_gate.interrupt_if_owner(expected);
    }

    pub fn stream_internal_turn(
        &self,
        mut req: crate::runtime::ext::InternalTurnRequest,
    ) -> RuntimeResult<AgentEventStream> {
        let _guard = self
            .0
            .external_control
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.is_closed() {
            return Err(RuntimeError::Closed);
        }
        if let Some(check) = &req.hooks.pre_admission_check {
            check()?;
        }
        let base = self.0.next_turn_id();
        let turn_id = match req.turn_id_prefix.as_deref() {
            Some(prefix) => TurnId::new(format!("{prefix}:{base}")),
            None => base,
        };
        let permit = self.0.turn_gate.acquire(turn_id.clone())?;
        let on_rollback = if let Some(prepare) = &req.hooks.prepare_on_admit {
            let prepared = prepare(&turn_id)?;
            req.content = prepared.content;
            req.metadata = prepared.metadata;
            prepared.on_rollback
        } else {
            None
        };
        if let Err(err) = self.0.input_inbox.begin_internal_turn(turn_id.as_str()) {
            if let Some(hook) = &on_rollback {
                hook();
            }
            return Err(RuntimeError::Command(format!("{err:#}")));
        }
        let receipt = crate::runtime::InputReceipt::transient(
            crate::runtime::HumanInput {
                request_id: String::new(),
                text: req.content.clone(),
                target_turn_id: None,
                attachment_ids: Vec::new(),
            },
            turn_id.as_str(),
        );
        let input_text = req.content.clone();
        self.0
            .dispatch_turn_stream(
                input_text,
                turn_id,
                permit,
                Some(crate::agent::orchestrator::InternalTurnExecution { request: req }),
                true,
                on_rollback,
                receipt,
            )
            .map(|(_, stream)| stream.expect("internal turn stream"))
    }

    pub async fn run_internal_turn_with_permit(
        &self,
        permit: &TurnPermit,
        req: crate::runtime::ext::InternalTurnRequest,
    ) -> anyhow::Result<TurnOutcome> {
        let (done_tx, done_rx) = oneshot::channel();
        self.0
            .cmd_tx
            .send(OrchCmd::RunScopedInternalTurn {
                request: Box::new(req),
                done: done_tx,
            })
            .map_err(|e| anyhow::anyhow!("orchestrator command channel closed: {e}"))?;
        let run_result = done_rx.await.map_err(|e| {
            anyhow::anyhow!("orchestrator dropped scoped internal turn result: {e}")
        })??;
        Ok(TurnOutcome::from_run_result(
            run_result,
            self.0.session.clone(),
            permit.turn_id().clone(),
        ))
    }

    pub async fn append_internal_tail(
        &self,
        content: String,
        source_tag: &str,
        metadata: serde_json::Value,
    ) -> anyhow::Result<usize> {
        let mut meta = match metadata {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        meta.insert("internal".into(), serde_json::Value::Bool(true));
        if !source_tag.is_empty() {
            meta.insert(
                "source".into(),
                serde_json::Value::String(source_tag.to_string()),
            );
        }
        self.0
            .ctx
            .stores
            .store
            .append_runtime_message_durable(serde_json::json!({
                "role": "user",
                "content": content,
                "_mink": serde_json::Value::Object(meta),
            }))
            .await?;
        self.0.ctx.stores.store.committed_len().await
    }

    pub async fn committed_seq(&self) -> anyhow::Result<usize> {
        self.0.ctx.stores.store.committed_len().await
    }

    pub async fn sync_conversation(&self) -> anyhow::Result<()> {
        self.0.ctx.stores.store.sync().await
    }

    pub async fn conversation_lines(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        self.0.ctx.stores.store.lines().await
    }

    pub fn has_outstanding_guidance(&self) -> bool {
        self.0.input_inbox.outstanding().iter().any(|r| {
            r.guidance
                && matches!(
                    r.status,
                    crate::runtime::InputStatus::Pending
                        | crate::runtime::InputStatus::Applying
                        | crate::runtime::InputStatus::Unapplied
                )
        })
    }

    pub async fn wait_interrupted_or_cancelled(&self) {
        if self.is_interrupted() || self.is_cancelled() {
            return;
        }
        tokio::select! {
            _ = self.0.ctx.interrupt.notified() => {}
            _ = self.0.ctx.cancel.cancelled() => {}
        }
    }

    pub async fn spawn_ephemeral_child(
        &self,
        session_id: String,
        opts: crate::runtime::ext::EphemeralChildOptions,
    ) -> anyhow::Result<crate::runtime::ext::EphemeralChildSession> {
        let deadline = opts.deadline;
        let cancel = self.0.ctx.cancel.linked_child_token();
        let executor = crate::agent::sub_executor::SubAgentExecutor::new_ephemeral(
            self.0.ctx.clone(),
            session_id,
            cancel,
            opts,
        )
        .await?;
        Ok(crate::runtime::ext::EphemeralChildSession::new(
            executor, deadline,
        ))
    }

    pub async fn run_ephemeral_child(
        &self,
        session_id: String,
        opts: crate::runtime::ext::EphemeralChildOptions,
        prompt: String,
    ) -> anyhow::Result<crate::runtime::ext::EphemeralChildOutcome> {
        let mut child = self.spawn_ephemeral_child(session_id, opts).await?;
        Ok(child.run_turn(prompt).await)
    }

    pub fn write_artifact(
        &self,
        tool: &str,
        description: &str,
        content: &str,
    ) -> anyhow::Result<crate::runtime::ext::ArtifactRef> {
        let record = self
            .0
            .ctx
            .stores
            .artifacts
            .write_text(tool, description, content)?;
        let path = self.0.session.artifacts_dir.join(&record.path);
        let sha256 = crate::runtime::internal_turn::sha256_hex(content.as_bytes());
        Ok(crate::runtime::ext::ArtifactRef {
            url: format!("artifact://{}", record.id),
            id: record.id,
            tool: record.tool,
            path,
            sha256,
            bytes: record.bytes,
        })
    }

    pub fn import_artifact(
        &self,
        source_path: &std::path::Path,
        label: &str,
    ) -> anyhow::Result<crate::runtime::ext::ArtifactRef> {
        let content = std::fs::read_to_string(source_path)?;
        self.write_artifact(label, label, &content)
    }

    pub fn artifact_reader(&self) -> crate::runtime::ext::ArtifactReader {
        crate::runtime::ext::ArtifactReader {
            artifacts: self.0.ctx.stores.artifacts.clone(),
            artifacts_dir: self.0.session.artifacts_dir.clone(),
        }
    }

    pub fn extension_emitter(&self) -> crate::runtime::ext::ExtensionEmitter {
        crate::runtime::ext::ExtensionEmitter {
            event_display: self.0.event_display.clone(),
            ctx: self.0.ctx.clone(),
        }
    }

    pub fn find_artifact(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<crate::runtime::ext::ArtifactRef>> {
        self.artifact_reader().find(id)
    }

    pub fn read_artifact_text(&self, id: &str) -> anyhow::Result<String> {
        self.0.ctx.stores.artifacts.read_text(id)
    }

    pub fn format_tool_result(&self, text: &str, max_bytes: usize) -> String {
        crate::tools::runner::format_tool_result(text, max_bytes)
    }

    pub async fn execute_scoped_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> anyhow::Result<crate::runtime::ext::ScopedToolOutcome> {
        self.0.ctx.stores.persistence_fault.check()?;
        let fields = input
            .as_object()
            .map(|obj| {
                obj.iter()
                    .map(|(k, v)| {
                        let rendered = v
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string());
                        (k.clone(), rendered)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let call = crate::protocol::ToolCallEvent {
            id: format!("scoped-{}", name.to_ascii_lowercase()),
            name: name.to_string(),
            input_json: input,
            fields,
            parse_error: None,
            raw_arguments_digest: None,
        };
        let tool_ctx = Arc::new(crate::context::ToolContext::from_shared(&self.0.ctx));
        let result = crate::tools::runner::ToolRunner::new(tool_ctx)
            .execute_verifier_call(call)
            .await?;
        Ok(crate::runtime::ext::ScopedToolOutcome {
            succeeded: result.succeeded(),
            content: result.content,
            exit_code: result.exit_code,
            artifacts: result
                .artifacts
                .into_iter()
                .map(|a| format!("artifact://{}", a.id))
                .collect(),
        })
    }

    pub fn persistence_fault(&self) -> crate::session::persistence::PersistenceFault {
        self.0.ctx.stores.persistence_fault.clone()
    }

    pub fn check_persistence_fault(&self) -> anyhow::Result<()> {
        self.0.ctx.stores.persistence_fault.check()
    }

    pub fn latch_persistence_fault(&self, detail: &str) {
        let _ = self
            .0
            .ctx
            .stores
            .persistence_fault
            .raise(&self.0.session.conversation_path, detail.to_string());
    }

    pub fn tool_surface_fingerprint(&self) -> String {
        self.0.ctx.tools.tool_surface.fingerprint().to_string()
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.0.ctx.tools.tool_surface.has(name)
    }

    pub fn has_reachable_tool(&self, name: &str) -> bool {
        self.0.ctx.tools.tool_surface.has_any_reachable(name)
    }

    pub fn max_context_tokens(&self) -> usize {
        self.0.ctx.config.max_context_tokens
    }

    pub fn tool_result_max_bytes(&self) -> usize {
        self.0.ctx.tools.tool_config.tool_result_max_bytes
    }

    pub fn active_model_name(&self) -> String {
        self.0
            .ctx
            .active_model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .actual
            .clone()
    }

    pub fn usage_records(&self) -> anyhow::Result<Vec<crate::session::usage::UsageRecord>> {
        self.0.ctx.stores.usage.all_records()
    }

    pub fn session_dir(&self) -> std::path::PathBuf {
        self.0.session.session_dir()
    }

    pub fn emit_extension_event(&self, kind: &str, payload: serde_json::Value) {
        self.extension_emitter().emit_extension_event(kind, payload);
    }

    pub fn emit_live_extension_event(&self, kind: &str, payload: serde_json::Value) {
        self.extension_emitter()
            .emit_live_extension_event(kind, payload);
    }

    pub fn render_info(&self, message: &str) {
        self.0.ctx.io.display.render_info(message);
    }

    pub fn render_error(&self, message: &str) {
        self.0.ctx.io.display.render_error(message);
    }

    /// Dynamically attach an additional [`SessionEventSink`](crate::runtime::SessionEventSink)
    /// to this runtime; dropping the returned guard removes it.
    pub fn register_session_event_sink(
        &self,
        sink: Arc<dyn crate::runtime::SessionEventSink>,
    ) -> crate::tools::rim_hooks::RegistrationGuard {
        self.register_session_event_sink_scoped("", sink)
    }

    /// Dynamically attach a scope-filtered [`SessionEventSink`](crate::runtime::SessionEventSink).
    /// An empty `scope_origin` matches every scope in the session.
    pub fn register_session_event_sink_scoped(
        &self,
        scope_origin: &str,
        sink: Arc<dyn crate::runtime::SessionEventSink>,
    ) -> crate::tools::rim_hooks::RegistrationGuard {
        if self.0.ctx.lifecycle.is_disposed() {
            return crate::tools::rim_hooks::RegistrationGuard::noop();
        }
        let origin = crate::tools::rim_hooks::normalize_scope_origin(scope_origin);
        self.0
            .ctx
            .io
            .session_event_sinks
            .register_scoped(origin, sink)
    }

    /// Dynamically attach a [`ToolExecutionGate`](crate::tools::rim_hooks::ToolExecutionGate)
    /// to this runtime (inherited by sub-agents); dropping the returned guard removes it.
    pub fn register_tool_execution_gate(
        &self,
        gate: Arc<dyn crate::tools::rim_hooks::ToolExecutionGate>,
    ) -> crate::tools::rim_hooks::RegistrationGuard {
        self.register_tool_execution_gate_scoped("", gate)
    }

    /// Dynamically attach a scope-filtered [`ToolExecutionGate`](crate::tools::rim_hooks::ToolExecutionGate).
    /// An empty `scope_origin` matches every scope in the session.
    pub fn register_tool_execution_gate_scoped(
        &self,
        scope_origin: &str,
        gate: Arc<dyn crate::tools::rim_hooks::ToolExecutionGate>,
    ) -> crate::tools::rim_hooks::RegistrationGuard {
        if self.0.ctx.lifecycle.is_disposed() {
            return crate::tools::rim_hooks::RegistrationGuard::noop();
        }
        let origin = crate::tools::rim_hooks::normalize_scope_origin(scope_origin);
        self.0
            .ctx
            .tools
            .tool_hooks
            .register_gate_scoped(origin, gate)
    }

    /// Dynamically attach a [`ToolOutputFilter`](crate::tools::rim_hooks::ToolOutputFilter)
    /// to this runtime (inherited by sub-agents); dropping the returned guard removes it.
    pub fn register_tool_output_filter(
        &self,
        filter: Arc<dyn crate::tools::rim_hooks::ToolOutputFilter>,
    ) -> crate::tools::rim_hooks::RegistrationGuard {
        self.register_tool_output_filter_scoped("", filter)
    }

    /// Dynamically attach a scope-filtered [`ToolOutputFilter`](crate::tools::rim_hooks::ToolOutputFilter).
    /// An empty `scope_origin` matches every scope in the session.
    pub fn register_tool_output_filter_scoped(
        &self,
        scope_origin: &str,
        filter: Arc<dyn crate::tools::rim_hooks::ToolOutputFilter>,
    ) -> crate::tools::rim_hooks::RegistrationGuard {
        if self.0.ctx.lifecycle.is_disposed() {
            return crate::tools::rim_hooks::RegistrationGuard::noop();
        }
        // Newly attached output filters may redact previously read files: bump
        // memo_mutation so stale ReadMemo entries do not bypass redaction.
        self.0
            .ctx
            .tools
            .memo_mutation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let origin = crate::tools::rim_hooks::normalize_scope_origin(scope_origin);
        self.0
            .ctx
            .tools
            .tool_hooks
            .register_filter_scoped(origin, filter)
    }

    /// Dynamically attach a [`ModelRequestHook`](crate::tools::rim_hooks::ModelRequestHook)
    /// to this runtime (inherited by sub-agents); dropping the returned guard removes it.
    pub fn register_model_request_hook(
        &self,
        hook: Arc<dyn crate::tools::rim_hooks::ModelRequestHook>,
    ) -> crate::tools::rim_hooks::RegistrationGuard {
        self.register_model_request_hook_scoped("", hook)
    }

    /// Dynamically attach a scope-filtered [`ModelRequestHook`](crate::tools::rim_hooks::ModelRequestHook).
    /// An empty `scope_origin` matches every scope in the session.
    pub fn register_model_request_hook_scoped(
        &self,
        scope_origin: &str,
        hook: Arc<dyn crate::tools::rim_hooks::ModelRequestHook>,
    ) -> crate::tools::rim_hooks::RegistrationGuard {
        if self.0.ctx.lifecycle.is_disposed() {
            return crate::tools::rim_hooks::RegistrationGuard::noop();
        }
        let origin = crate::tools::rim_hooks::normalize_scope_origin(scope_origin);
        self.0
            .ctx
            .tools
            .tool_hooks
            .register_model_request_hook_scoped(origin, hook)
    }

    pub fn is_closing(&self) -> bool {
        self.0.ctx.lifecycle.is_closing()
    }

    pub fn scope_path(&self) -> &str {
        &self.0.ctx.scope_path
    }

    /// Register a LIFO closing callback executed once when the runtime enters
    /// the closing phase (or immediately if already closing).
    pub fn register_on_closing(&self, label: impl Into<String>, f: impl FnOnce() + Send + 'static) {
        self.0.ctx.lifecycle.register_on_closing(label, f);
    }

    /// Register a LIFO teardown callback executed on `AgentRuntime::shutdown()`
    /// or `AgentRuntime::drop()` (or immediately if already disposed).
    pub fn register_teardown(&self, label: impl Into<String>, f: impl FnOnce() + Send + 'static) {
        self.0.ctx.lifecycle.register_teardown(label, f);
    }

    pub fn session_event_sink_count(&self) -> usize {
        self.0.ctx.io.session_event_sinks.len()
    }

    pub fn tool_hook_stats(&self) -> crate::tools::rim_hooks::ToolHookStats {
        self.0.ctx.tools.tool_hooks.stats()
    }
}

/// Per-stage budget for shutdown flush/teardown work.
const SHUTDOWN_STAGE_BUDGET: Duration = Duration::from_secs(5);

/// Run one shutdown stage under a deadline, recording failures/timeouts.
///
/// "Call returned" and "background writes finished" are different promises:
/// a timed-out stage is reported, never silently claimed complete.
/// Run one blocking shutdown stage on the blocking pool under a deadline.
///
/// Wrapping a sync `flush` in `async {}` would not make it cancellable: the
/// blocking work must own a blocking-pool thread so the async worker stays
/// responsive and the deadline bounds only the caller's wait.
async fn flush_stage_blocking<F>(budget: Duration, stage: &str, work: F) -> (Option<String>, bool)
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    flush_stage(budget, stage, async move {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|error| anyhow::anyhow!("blocking stage task failed: {error}"))?
    })
    .await
}

async fn flush_stage<T, E: std::fmt::Display>(
    budget: Duration,
    stage: &str,
    future: impl std::future::Future<Output = Result<T, E>>,
) -> (Option<String>, bool) {
    match tokio::time::timeout(budget, future).await {
        Ok(Ok(_)) => (None, false),
        Ok(Err(error)) => (Some(format!("{stage} failed: {error}")), false),
        Err(_) => {
            // Pure timeouts must surface as `RuntimeError::ShutdownTimeout`
            // rather than being misclassified as `RuntimeError::Command`.
            (None, true)
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

    pub async fn shutdown(mut self) -> RuntimeResult<()> {
        self.shutdown_called = true;
        let controller = self
            .handle
            .external_control
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .controller
            .clone();
        if let Some(controller) = &controller {
            controller.on_shutdown_disable();
        }
        self.ctx.lifecycle.mark_closing();
        let _ = self
            .handle
            .external_control
            .stop_cause
            .send_replace(Some(crate::runtime::ext::StopCause::RuntimeShutdown));
        // Cancellation is the runtime's own stop signal and must land before the
        // turn-gate interrupt: an in-flight external round settles as a pause
        // only when the gate was interrupted without the runtime being cancelled,
        // and a normal shutdown keeps active intent for the next open.
        self.ctx.cancel.cancel();
        let had_active = self.handle.turn_gate.close_and_interrupt();
        let turn_timed_out = had_active
            && tokio::time::timeout(Duration::from_secs(5), self.handle.turn_gate.wait_idle())
                .await
                .is_err();
        // Wait for any active external run to exit (bounded).
        let ext_wait = if let Some(controller) = &controller
            && controller.is_active()
        {
            tokio::time::timeout(Duration::from_secs(5), controller.wait_exit())
                .await
                .is_err()
        } else {
            false
        };
        if let Some(controller) = &controller {
            controller.on_shutdown_close();
        }
        self.ctx.lifecycle.dispose();
        self.handle
            .external_control
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .controller = None;
        let mut failures = Vec::new();
        let mut shutdown_timed_out = turn_timed_out || ext_wait;
        if let Some(mut orchestrator) = self.orch_handle.take() {
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
        }
        if let Some(dispatcher) = self.event_display.dispatcher()
            && let Err(error) = dispatcher.shutdown().await
        {
            failures.push(format!("event dispatcher shutdown failed: {error}"));
        }
        let usage = self.ctx.stores.usage.clone();
        let (f_events, f_usage, f_stats, f_compaction) = tokio::join!(
            flush_stage(
                SHUTDOWN_STAGE_BUDGET,
                "event log flush",
                self.ctx.flush_event_log()
            ),
            flush_stage_blocking(SHUTDOWN_STAGE_BUDGET, "usage flush", move || usage.flush()),
            flush_stage(
                SHUTDOWN_STAGE_BUDGET,
                "stats flush",
                self.ctx.stores.stats.flush()
            ),
            flush_stage(
                SHUTDOWN_STAGE_BUDGET,
                "compaction projection flush",
                self.ctx.stores.compaction.flush_projection(),
            ),
        );
        for (error, timed_out) in [f_events, f_usage, f_stats, f_compaction] {
            if let Some(error) = error {
                failures.push(error);
            }
            shutdown_timed_out |= timed_out;
        }
        self.ctx
            .stores
            .session_lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
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

    /// Read the initial presentation without issuing a model request.
    /// Live title events supersede this snapshot after model changes.
    pub async fn presentation_snapshot(&self) -> (String, crate::ui::StatsSnapshot) {
        (
            self.ctx.model(),
            crate::ui::title_snapshot(&self.ctx, 0.0).await,
        )
    }
    pub fn session_info(&self) -> &SessionInfo {
        self.handle.session_info()
    }
}

pub(crate) fn new_turn_gate(interrupt: crate::cancel::InterruptSignal) -> Arc<TurnGate> {
    Arc::new(TurnGate {
        state: Mutex::new(TurnGateState::default()),
        interrupt,
        idle: tokio::sync::Notify::new(),
    })
}

#[cfg(test)]
mod shutdown_stage_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn default_stage_budget_allows_delayed_blocking_flush() {
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = finished.clone();
        let (failure, timed_out) =
            flush_stage_blocking(SHUTDOWN_STAGE_BUDGET, "delayed blocking flush", move || {
                std::thread::sleep(Duration::from_millis(300));
                worker_finished.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(!timed_out, "{failure:?}");
        assert!(failure.is_none(), "{failure:?}");
        assert!(finished.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn blocking_stage_timeout_bounds_caller() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct ReleaseGuard(Arc<AtomicBool>);
        impl Drop for ReleaseGuard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let release = Arc::new(AtomicBool::new(false));
        let _guard = ReleaseGuard(release.clone());
        let worker_release = release.clone();
        let started = std::time::Instant::now();
        let (failure, timed_out) = flush_stage_blocking(
            Duration::from_millis(50),
            "blocking unit stage",
            move || {
                while !worker_release.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(())
            },
        )
        .await;
        assert!(timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "caller must not wait for the blocking work: {:?}",
            started.elapsed()
        );
        assert!(
            failure.is_none(),
            "pure stage timeout must not produce failure (so shutdown returns ShutdownTimeout): {failure:?}"
        );
    }

    #[tokio::test]
    async fn stage_timeout_is_bounded_and_reported() {
        let started = std::time::Instant::now();
        let (failure, timed_out) = flush_stage(Duration::from_millis(50), "unit stage", async {
            futures::future::pending::<anyhow::Result<()>>().await
        })
        .await;
        assert!(timed_out);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            failure.is_none(),
            "pure stage timeout must not produce failure: {failure:?}"
        );
    }

    #[tokio::test]
    async fn stage_errors_are_collected_without_timeout_flag() {
        let (failure, timed_out) = flush_stage(Duration::from_millis(50), "unit stage", async {
            Err::<(), _>(anyhow::anyhow!("injected failure"))
        })
        .await;
        assert!(!timed_out);
        assert!(
            failure
                .as_deref()
                .is_some_and(|line| line.contains("injected failure")),
            "{failure:?}"
        );
    }
}
