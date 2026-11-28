//! Host supplied tools and the execution boundary they receive.

use crate::tools::metadata::{ApprovalTier, ToolResultKind};
use crate::tools::semantic_capabilities::{
    CapabilityAvailability, CapabilityUseScope, ProviderTier, ToolSemanticCapability,
};
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    ParallelReadOnly,
    Sequential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolActivation {
    Enabled,
    ExplicitOnly,
}

/// Controls where a custom tool is exposed when PTC (`RunCode`) is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolPtcExposure {
    /// Follow the active `PtcMode` (`Tool` exposes on both wire and `<ptc-sdk>`; `Exclusive` exposes in `<ptc-sdk>`).
    #[default]
    Default,
    /// Always expose on wire only (direct tool calls), even in `PtcMode::Exclusive`; never callable inside `RunCode`.
    DirectOnly,
    /// Expose inside `<ptc-sdk>` (`RunCode`) only when PTC is active (falls back to wire when `PtcMode::Off`).
    ProgrammaticOnly,
}

/// Controls which agent roles include a custom tool in their resolved surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolRoleScope {
    /// Available to primary and delegated sub-agents, excluded from scoped child sessions.
    #[default]
    PrimaryAndSubAgent,
    /// Available only to the primary agent.
    PrimaryOnly,
    /// Available to all roles (primary, sub-agent, and scoped child sessions).
    All,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub summary: String,
    pub input_schema: Value,
    pub approval: ApprovalTier,
    pub result_kind: ToolResultKind,
    pub execution: ToolExecutionMode,
    pub mutating: bool,
    pub discoverable: bool,
    pub storm_exempt: bool,
    pub side_effect_exempt: bool,
    pub role_scope: ToolRoleScope,
    pub activation: ToolActivation,
    pub ptc_exposure: ToolPtcExposure,
    pub hard_dependencies: Vec<String>,
    pub semantic_capabilities: Vec<ToolCapabilityOffer>,
}

#[derive(Debug, Clone)]
pub struct ToolCapabilityOffer {
    pub capability: ToolSemanticCapability,
    pub tier: ProviderTier,
    pub priority: u16,
    pub available_if: CapabilityAvailability,
    pub use_scope: CapabilityUseScope,
}

impl ToolDefinition {
    pub fn new(name: impl Into<String>, summary: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            summary: summary.into(),
            input_schema,
            approval: ApprovalTier::Read,
            result_kind: ToolResultKind::Text,
            execution: ToolExecutionMode::ParallelReadOnly,
            mutating: false,
            discoverable: false,
            storm_exempt: false,
            side_effect_exempt: false,
            role_scope: ToolRoleScope::PrimaryAndSubAgent,
            activation: ToolActivation::Enabled,
            ptc_exposure: ToolPtcExposure::Default,
            hard_dependencies: Vec::new(),
            semantic_capabilities: Vec::new(),
        }
    }

    pub fn with_ptc_exposure(mut self, exposure: ToolPtcExposure) -> Self {
        self.ptc_exposure = exposure;
        self
    }

    pub fn with_role_scope(mut self, role_scope: ToolRoleScope) -> Self {
        self.role_scope = role_scope;
        self
    }

    pub fn with_side_effect_exempt(mut self, exempt: bool) -> Self {
        self.side_effect_exempt = exempt;
        self
    }

    pub fn with_approval(mut self, approval: ApprovalTier) -> Self {
        self.approval = approval;
        self
    }

    pub fn with_result_kind(mut self, result_kind: ToolResultKind) -> Self {
        self.result_kind = result_kind;
        self
    }

    pub fn with_execution(mut self, execution: ToolExecutionMode) -> Self {
        self.execution = execution;
        self
    }

    pub fn with_mutating(mut self, mutating: bool) -> Self {
        self.mutating = mutating;
        self
    }

    pub fn with_storm_exempt(mut self, storm_exempt: bool) -> Self {
        self.storm_exempt = storm_exempt;
        self
    }
}

#[derive(Clone)]
pub struct ToolExecutionContext {
    cwd: PathBuf,
    interrupt: Arc<AtomicBool>,
}

impl ToolExecutionContext {
    pub fn new(cwd: PathBuf, interrupt: Arc<AtomicBool>) -> Self {
        Self { cwd, interrupt }
    }
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    pub fn is_cancelled(&self) -> bool {
        self.interrupt.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub conversation_content: Option<String>,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub structured_value: Option<serde_json::Value>,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            conversation_content: None,
            success: true,
            exit_code: None,
            structured_value: None,
        }
    }

    pub fn with_structured_value(mut self, value: serde_json::Value) -> Self {
        self.structured_value = Some(value);
        self
    }
}

#[derive(Debug, Clone)]
pub struct ToolError {
    message: String,
    /// Set by [`ToolError::argument`]: pure argument decoding failure with no
    /// side effect. The runtime reports it as model-format feedback.
    argument: bool,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            argument: false,
        }
    }

    /// Construct a pure argument-decoding failure. Callers must guarantee the
    /// error is returned before any side effect occurs.
    pub fn argument(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            argument: true,
        }
    }

    pub(crate) fn is_argument(&self) -> bool {
        self.argument
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

// async_trait expands a bare `#[must_use]` into generated code; the lint
// cannot be satisfied there (macro output cannot carry a reason message).
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(
        &self,
        input: Value,
        ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError>;
}

#[derive(Clone)]
pub(crate) struct RegisteredCustomTool {
    pub definition: ToolDefinition,
    pub executor: Arc<dyn AgentTool>,
}

pub(crate) fn freeze_custom_tools(tools: Vec<Arc<dyn AgentTool>>) -> Vec<RegisteredCustomTool> {
    tools
        .into_iter()
        .map(|executor| RegisteredCustomTool {
            definition: executor.definition(),
            executor,
        })
        .collect()
}
