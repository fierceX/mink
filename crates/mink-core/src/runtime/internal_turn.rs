//! Generic runtime extension primitives for external controllers, internal
//! turns, ephemeral child sessions, artifacts, and scoped tool execution.

use crate::agent::failure::TurnFailureReason;
use crate::agent::orchestrator::TurnStatus;
use crate::runtime::{AgentTool, RuntimeResult, TurnId};
use crate::session::usage::UsageRecord;
use crate::tools::policy_hook::{ToolExecutionMask, ToolExecutionObserver};
use serde_json::Value;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::watch;

/// Distinguishes a user-initiated turn interrupt from a runtime shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
    UserInterrupt,
    RuntimeShutdown,
}

/// Lifecycle bridge implemented by an external session controller (e.g. a
/// cross-turn scheduler) attached to [`crate::runtime::AgentRuntimeHandle`].
pub trait ExternalController: Send + Sync {
    /// If ordinary human turns, `compact`, and `set_model` should be rejected
    /// right now, return `Some(owner_label)`.
    fn blocks_ordinary_control(&self) -> Option<&'static str> {
        None
    }

    /// Invoked under the runtime admission lock during
    /// `interrupt_current_turn()` before interrupting the turn gate.
    fn on_user_interrupt(&self) {}

    /// Invoked at the start of `shutdown()` / `Drop` before cancelling the
    /// runtime cancellation token.
    fn on_shutdown_disable(&self) {}

    /// Whether an external run is currently active and should be awaited during
    /// `shutdown()`.
    fn is_active(&self) -> bool {
        false
    }

    /// Wait until any active external run exits during `shutdown()`.
    fn wait_exit(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }

    /// Invoked after waiting during `shutdown()` / `Drop` to mark the
    /// controller permanently closed.
    fn on_shutdown_close(&self) {}
}

#[derive(Default)]
pub(crate) struct ExternalAdmissionState {
    pub(crate) active_owner: Option<&'static str>,
    pub(crate) controller: Option<Arc<dyn ExternalController>>,
}

impl ExternalAdmissionState {
    pub(crate) fn blocking_owner(&self) -> Option<&'static str> {
        self.active_owner.or_else(|| {
            self.controller
                .as_ref()
                .and_then(|controller| controller.blocks_ordinary_control())
        })
    }
}

pub(crate) struct ExternalControlState {
    pub(crate) admission: Mutex<ExternalAdmissionState>,
    pub(crate) stop_cause: watch::Sender<Option<StopCause>>,
}

impl ExternalControlState {
    pub(crate) fn new() -> Self {
        let (stop_cause, _) = watch::channel(None);
        Self {
            admission: Mutex::new(ExternalAdmissionState::default()),
            stop_cause,
        }
    }
}

/// Exclusive external control lease for a session; clears ownership on drop.
pub struct ExternalOwnerGuard {
    pub(crate) control: Arc<ExternalControlState>,
    pub(crate) owner: &'static str,
}

impl ExternalOwnerGuard {
    pub fn owner(&self) -> &'static str {
        self.owner
    }
}

impl Drop for ExternalOwnerGuard {
    fn drop(&mut self) {
        let mut state = self
            .control
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.active_owner == Some(self.owner) {
            state.active_owner = None;
        }
    }
}

/// Outcome of preparing an [`InternalTurnRequest`] while holding the admission
/// lock and a `TurnPermit`.
pub struct PreparedInternalTurn {
    pub content: String,
    pub metadata: Value,
    pub on_rollback: Option<TurnRollbackHook>,
}

/// Settlement facts handed to [`InternalTurnRequest::on_before_final`] before
/// `TurnFinal` is emitted.
#[derive(Debug, Clone)]
pub struct TurnSettlementContext {
    pub billing_turn_id: String,
    pub status: TurnStatus,
    pub failure: Option<TurnFailureReason>,
    pub error: Option<String>,
    pub text: String,
    pub usage_records: Vec<UsageRecord>,
    pub interrupted_without_runtime_cancel: bool,
    pub start_seq: usize,
    pub end_seq: usize,
}

pub type TurnPreAdmissionHook =
    Arc<dyn Fn(&TurnId) -> RuntimeResult<PreparedInternalTurn> + Send + Sync>;
pub type TurnRollbackHook = Arc<dyn Fn() + Send + Sync>;
pub type TurnBillingHook = Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>;
pub type TurnSettlementHook = Arc<
    dyn Fn(TurnSettlementContext) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>
        + Send
        + Sync,
>;

/// Request parameters for executing an internal runtime turn without disguising
/// it as ordinary human input.
#[derive(Clone)]
pub struct InternalTurnRequest {
    pub content: String,
    pub source_tag: String,
    pub metadata: Value,
    pub turn_id_prefix: Option<String>,
    pub accept_guidance: bool,
    pub count_as_user_turn: bool,
    pub consume_inbox: bool,
    pub enable_belief: bool,
    pub disable_native_web: bool,
    pub billing_turn_id_override: Option<String>,
    pub tool_mask: Option<ToolExecutionMask>,
    pub tool_observer: Option<Arc<dyn ToolExecutionObserver>>,
    pub budget_check: Option<Arc<dyn Fn() -> anyhow::Result<()> + Send + Sync>>,
    pub stop_when: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    pub continue_prompt_when_not_stopped: Option<String>,
    pub pre_admission_check: Option<Arc<dyn Fn() -> RuntimeResult<()> + Send + Sync>>,
    pub prepare_on_admit: Option<TurnPreAdmissionHook>,
    pub on_billing_started: Option<TurnBillingHook>,
    pub on_before_final: Option<TurnSettlementHook>,
}

impl InternalTurnRequest {
    pub fn new(source_tag: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            source_tag: source_tag.into(),
            metadata: Value::Object(Default::default()),
            turn_id_prefix: None,
            accept_guidance: false,
            count_as_user_turn: true,
            consume_inbox: false,
            enable_belief: true,
            disable_native_web: false,
            billing_turn_id_override: None,
            tool_mask: None,
            tool_observer: None,
            budget_check: None,
            stop_when: None,
            continue_prompt_when_not_stopped: None,
            pre_admission_check: None,
            prepare_on_admit: None,
            on_billing_started: None,
            on_before_final: None,
        }
    }
}

/// Configuration for spawning a controlled ephemeral child session.
#[derive(Clone)]
pub struct EphemeralChildOptions {
    pub origin_tag: String,
    pub label: Option<String>,
    pub allowed_tools: Vec<String>,
    pub fork_parent_session: bool,
    pub isolate_extensions: bool,
    pub timeout_secs: Option<u64>,
    pub billing_turn_id: Option<String>,
    pub tool_mask: Option<ToolExecutionMask>,
    pub tool_observer: Option<Arc<dyn ToolExecutionObserver>>,
    pub budget_check: Option<Arc<dyn Fn() -> anyhow::Result<()> + Send + Sync>>,
    pub extra_tools: Vec<Arc<dyn AgentTool>>,
    pub deadline: Option<Instant>,
}

impl EphemeralChildOptions {
    pub fn new(origin_tag: impl Into<String>) -> Self {
        Self {
            origin_tag: origin_tag.into(),
            label: None,
            allowed_tools: Vec::new(),
            fork_parent_session: true,
            isolate_extensions: true,
            timeout_secs: None,
            billing_turn_id: None,
            tool_mask: None,
            tool_observer: None,
            budget_check: None,
            extra_tools: Vec::new(),
            deadline: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EphemeralChildStatus {
    Succeeded,
    Failed,
    Interrupted,
    TimedOut,
}

#[derive(Debug, Clone)]
pub struct EphemeralChildOutcome {
    pub status: EphemeralChildStatus,
    pub stop_reason: String,
    pub text: String,
    pub thinking: String,
    pub diagnostic: Option<String>,
    pub elapsed_ms: u64,
}

/// Reference to an artifact stored in the session's `ArtifactManager`.
#[derive(Debug, Clone)]
pub struct ArtifactRef {
    pub id: String,
    pub tool: String,
    pub url: String,
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

/// Result of executing a single scoped tool call via
/// [`crate::runtime::AgentRuntimeHandle::execute_scoped_tool`].
#[derive(Debug, Clone)]
pub struct ScopedToolOutcome {
    pub content: String,
    pub succeeded: bool,
    pub exit_code: Option<i32>,
    pub artifacts: Vec<String>,
}

/// Cycle-free reader handle for session artifacts, safe for custom tools and
/// external controllers without holding an [`crate::runtime::AgentRuntimeHandle`].
#[derive(Clone)]
pub struct ArtifactReader {
    pub(crate) artifacts: Arc<crate::session::artifacts::ArtifactManager>,
    pub(crate) artifacts_dir: PathBuf,
}

impl ArtifactReader {
    pub fn artifacts_dir(&self) -> &std::path::Path {
        &self.artifacts_dir
    }

    pub fn find(&self, id: &str) -> anyhow::Result<Option<ArtifactRef>> {
        let Some(record) = self.artifacts.find(id)? else {
            return Ok(None);
        };
        let path = self.artifacts_dir.join(&record.path);
        let bytes = std::fs::read(&path)?;
        Ok(Some(ArtifactRef {
            url: format!("artifact://{}", record.id),
            id: record.id,
            tool: record.tool,
            path,
            sha256: sha256_hex(&bytes),
            bytes: record.bytes,
        }))
    }

    pub fn get(&self, id: &str) -> anyhow::Result<ArtifactRef> {
        self.find(id)?
            .ok_or_else(|| anyhow::anyhow!("Error: artifact not found: {id}"))
    }

    pub fn read_text(&self, id: &str) -> anyhow::Result<String> {
        self.artifacts.read_text(id)
    }
}

/// Cycle-free extension event and display emitter, safe for external
/// controllers to hold without keeping an [`crate::runtime::AgentRuntimeHandle`].
#[derive(Clone)]
pub struct ExtensionEmitter {
    pub(crate) event_display: Arc<crate::runtime::events::EventDisplay>,
    pub(crate) ctx: Arc<crate::context::AgentSharedContext>,
}

impl ExtensionEmitter {
    pub fn emit_extension_event(&self, kind: &str, payload: Value) {
        self.event_display
            .emit(crate::runtime::AgentEventKind::Extension {
                kind: kind.to_string(),
                payload: payload.clone(),
            });
        self.ctx.log_event(crate::events::EventLog::Extension {
            kind: kind.to_string(),
            payload,
        });
    }

    /// Emit an extension event on the live event stream without duplicating a
    /// durable `events.jsonl` row.
    pub fn emit_live_extension_event(&self, kind: &str, payload: Value) {
        self.event_display
            .emit(crate::runtime::AgentEventKind::Extension {
                kind: kind.to_string(),
                payload,
            });
    }

    pub fn render_info(&self, message: &str) {
        self.ctx.io.display.render_info(message);
    }

    pub fn render_error(&self, message: &str) {
        self.ctx.io.display.render_error(message);
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
