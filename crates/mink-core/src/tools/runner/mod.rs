use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::{Arc, Mutex};

use super::bash;
use super::file;
use super::metadata::{
    ApprovalTier, ToolBlocker, ToolFailureKind, ToolFailureSource, ToolMetadata, ToolResultKind,
    ToolStatus,
};
use super::plan::{PlanClearTool, PlanCommand, PlanConfirmTool, PlanDraftTool};
use super::python;
#[cfg(feature = "python-sandbox")]
use super::sandbox_python;
use super::search;
use super::todo::{TodoAdvanceTool, TodoReadTool, TodoWriteTool};
use crate::context::ToolContext;
use crate::guard::storm::{StormBreaker, StormDecision};
use crate::protocol::ToolCallEvent;
use crate::ui::{ArtifactDisplay, ToolPresentation};

#[derive(Debug)]
/// A read whose memo entry should be recorded by the runner *after* the
/// complete composed output (including the summary line) is known to fit the
/// budget. The executor cannot know the final size, so it only offers the
/// candidate; the runner decides.
pub struct MemoCandidate {
    pub path: std::path::PathBuf,
    pub raw: bool,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
}

#[derive(Debug)]
pub struct ToolOutcome {
    pub content: String,
    pub conversation_content: String,
    pub is_bash: bool,
    pub exit_code: Option<i32>,
    pub success: bool,
    /// True when the call changed nothing on disk (e.g. an idempotent edit);
    /// used to skip mutation-epoch bumps so read memos stay valid.
    pub no_mutation: bool,
    /// Set by Read when a successful read may be memoized; the runner records
    /// it only when the final composed output fits `tool_result_max_bytes`.
    pub memo_candidate: Option<MemoCandidate>,
    pub diagnostics: Vec<String>,
    pub plan_command: Option<PlanCommand>,
    pub state_metadata: Option<serde_json::Value>,
    pub presentation: Option<ToolPresentation>,
    /// Optional structured JSON result for PTC Python bindings.
    pub structured_value: Option<serde_json::Value>,
    /// Structured process termination, set by command tools (Bash/Python).
    /// Runner failure classification prefers this over output text.
    pub(crate) termination: Option<crate::tools::process::ProcessTermination>,
}

impl ToolOutcome {
    pub fn text(content: String) -> Self {
        Self {
            content,
            conversation_content: String::new(),
            is_bash: false,
            exit_code: None,
            success: true,
            no_mutation: false,
            memo_candidate: None,
            diagnostics: Vec::new(),
            plan_command: None,
            state_metadata: None,
            presentation: None,
            structured_value: None,
            termination: None,
        }
    }

    pub fn plan(command: PlanCommand, content: &str) -> Self {
        let mut outcome = Self::text(content.to_string());
        outcome.plan_command = Some(command);
        outcome
    }
}

/// ToolExec defines the execution contract for a tool.
/// Each tool registers itself via `tool_registry()` and is dispatched
/// without a central match block.
pub trait ToolExec: Send + Sync {
    fn metadata(&self) -> ToolMetadata;

    fn execute(&self, input: &serde_json::Value, ctx: &ToolContext) -> anyhow::Result<ToolOutcome>;
}

/// Built-in tool table, initialized once at first access.
static TOOL_REGISTRY: LazyLock<Vec<Box<dyn ToolExec>>> = LazyLock::new(|| {
    vec![
        Box::new(crate::tools::goal::GoalReadTool),
        Box::new(crate::tools::goal::GoalDraftTool),
        Box::new(crate::tools::goal::GoalReviewTool),
        Box::new(file::ReadTool),
        Box::new(file::WriteTool),
        Box::new(file::EditTool),
        Box::new(bash::BashTool),
        Box::new(search::GlobTool),
        Box::new(search::GrepTool),
        Box::new(super::web::WebSearchTool),
        Box::new(super::web::WebFetchTool),
        Box::new(TodoReadTool),
        Box::new(TodoWriteTool),
        Box::new(TodoAdvanceTool),
        Box::new(PlanDraftTool),
        Box::new(PlanConfirmTool),
        Box::new(PlanClearTool),
        Box::new(python::PythonTool),
        #[cfg(feature = "python-sandbox")]
        Box::new(sandbox_python::PythonSandboxTool),
        Box::new(crate::ptc::tool::RunCodeTool),
        Box::new(SubAgentTool),
    ]
});

/// Return a reference to the global tool registry.
pub fn tool_registry() -> &'static [Box<dyn ToolExec>] {
    &TOOL_REGISTRY
}

// --- Runner ---

/// ToolRunner dispatches tool calls to their implementations.
pub struct ToolRunner {
    pub(super) ctx: Arc<ToolContext>,
    pub(super) storm: Arc<Mutex<StormBreaker>>,
    pub(super) tools: &'static [Box<dyn ToolExec>],
}

/// Result of executing a single tool call.
pub struct ToolExecution {
    pub tool_use_id: String,
    pub tool_name: String,
    pub tool_args: BTreeMap<String, String>,
    pub content: String,
    pub conv_content: String,
    pub spawns_sub_agent: bool,
    pub sub_agent_prompt: Option<String>,
    pub sub_agent_fork: bool,
    pub sub_agent_label: Option<String>,
    pub sub_agent_model: Option<String>,
    pub exit_code: Option<i32>,
    pub status: ToolStatus,
    pub result_kind: ToolResultKind,
    pub presentation: Option<ToolPresentation>,
    pub artifacts: Vec<ArtifactDisplay>,
    pub signals: Vec<crate::guard::collector::Signal>,
    /// Internal provenance of a failure produced by model output formatting.
    /// Never used to recover authoritative execution state.
    pub failure_source: Option<ToolFailureSource>,
    pub(crate) plan_command: Option<PlanCommand>,
    pub(crate) needs_finalization: bool,
    pub(crate) state_metadata: Option<serde_json::Value>,
    pub image_attachment: Option<crate::tools::image::ImageAttachment>,
    /// Additional images returned from a single execution (e.g. multiple `inspect(img)` in `RunCode`).
    pub extra_image_attachments: Vec<crate::tools::image::ImageAttachment>,
    pub(crate) todo_advanced: bool,
    pub(crate) edited_paths: Vec<String>,
}

pub struct SubAgentRequest {
    pub prompt: String,
    pub fork: bool,
    pub label: Option<String>,
    pub model: Option<String>,
}

impl ToolExecution {
    pub fn succeeded(&self) -> bool {
        self.status.is_success()
    }

    pub(crate) fn take_sub_agent_request(&mut self) -> Option<SubAgentRequest> {
        if !self.spawns_sub_agent {
            return None;
        }
        let prompt = self.sub_agent_prompt.take()?;
        Some(SubAgentRequest {
            prompt,
            fork: self.sub_agent_fork,
            label: self.sub_agent_label.take(),
            model: self.sub_agent_model.take(),
        })
    }

    #[cfg(test)]
    pub(crate) fn test_result(
        tool_use_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            tool_name: tool_name.into(),
            tool_args: BTreeMap::new(),
            content: content.into(),
            conv_content: String::new(),
            spawns_sub_agent: false,
            sub_agent_prompt: None,
            sub_agent_fork: false,
            sub_agent_label: None,
            sub_agent_model: None,
            exit_code: None,
            status: ToolStatus::Succeeded,
            result_kind: ToolResultKind::Text,
            presentation: None,
            artifacts: Vec::new(),
            signals: Vec::new(),
            failure_source: None,
            plan_command: None,
            needs_finalization: false,
            state_metadata: None,
            image_attachment: None,
            extra_image_attachments: Vec::new(),
            todo_advanced: false,
            edited_paths: Vec::new(),
        }
    }
}

pub(crate) struct NestedToolOutcome {
    pub rendered_text: String,
    pub structured_value: Option<serde_json::Value>,
    pub status: ToolStatus,
    pub exit_code: Option<i32>,
    pub result_kind: ToolResultKind,
    pub unknown_effect: bool,
    pub todo_advanced: bool,
    pub edited_paths: Vec<String>,
    pub image: Option<(crate::tools::image::ImageAttachment, String)>,
}

/// Infrastructure failure while executing a tool batch.
///
/// `completed` carries every real `ToolExecution` produced before the fatal
/// error, so callers can persist them instead of replacing side effects with
/// a synthetic failure.
pub(crate) struct FatalBatchError {
    pub error: anyhow::Error,
    pub completed: Vec<ToolExecution>,
}

impl std::fmt::Debug for FatalBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FatalBatchError")
            .field("error", &self.error)
            .field("completed", &self.completed.len())
            .finish()
    }
}

impl std::fmt::Display for FatalBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for FatalBatchError {}

/// Goal side-effect ledger: `GoalDraft` / `GoalReview` is a control record rather than a side
/// effect, read-only builtins are safe, and custom tools are recorded
/// conservatively because their declared metadata is host-supplied.
///
/// The goal-control exclusion is checked first so a custom tool registered under
/// the same name cannot slip past it through the custom-tool branch.
pub(crate) fn records_goal_effect(
    name: &str,
    metadata: Option<&ToolMetadata>,
    is_custom: bool,
) -> bool {
    if matches!(name, "GoalDraft" | "GoalReview") {
        return false;
    }
    is_custom || metadata.is_none_or(|m| m.approval != ApprovalTier::Read || m.mutating)
}

impl ToolRunner {
    pub fn new(ctx: Arc<ToolContext>) -> Self {
        Self {
            ctx,
            storm: Arc::new(Mutex::new(StormBreaker::new(6, 3))),
            tools: tool_registry(),
        }
    }

    /// Find a tool by name.
    fn find_tool(&self, name: &str) -> Option<&dyn ToolExec> {
        self.tools
            .iter()
            .find(|t| t.metadata().name == name)
            .map(|t| t.as_ref())
    }

    fn find_custom_tool(
        &self,
        name: &str,
    ) -> Option<&crate::tools::extension::RegisteredCustomTool> {
        self.ctx
            .custom_tools
            .iter()
            .find(|tool| tool.definition.name == name)
    }

    fn metadata_for(&self, name: &str) -> Option<ToolMetadata> {
        if let Some(tool) = self.find_tool(name) {
            return Some(tool.metadata());
        }
        self.ctx
            .tool_surface
            .get(name)
            .map(|tool| tool.metadata.clone())
    }

    /// Reset storm breaker window — call at the start of each user turn.
    pub fn reset_storm(&self) {
        self.storm.lock().unwrap_or_else(|e| e.into_inner()).reset();
    }

    pub(crate) fn goal_read_only(&self) -> bool {
        self.ctx.goal_drafting || self.ctx.review_evidence().is_some()
    }
}

use policy::unknown_tool_result;

mod batch;
mod format;
mod policy;
mod sub_agent;

#[allow(unused_imports)]
pub use format::{
    TruncationPolicy, approx_token_count, format_tool_result, format_tool_result_policy,
};
/// Shared helper for extracting edited/written file paths from a tool call's arguments.
pub(crate) fn extract_edited_paths(
    tool_name: &str,
    args: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut paths = Vec::new();
    match tool_name {
        "Write" => {
            if let Some(path) = args.get("path") {
                paths.push(path.clone());
            }
        }
        "Edit" => {
            if let Some(path) = args.get("path") {
                paths.push(path.clone());
            } else if let Some(input) = args.get("input") {
                for line in input.lines() {
                    if let Some(path) = crate::tools::hashline::section_header_path(line) {
                        paths.push(path);
                    }
                }
            }
        }
        _ => {}
    }
    paths
}

pub(crate) use batch::execute_nested_tool_call;
pub(crate) use format::format_tool_result_with_artifact as format_tool_result_with_artifact_pub;
#[cfg(test)]
use format::{filter_bash_noise, format_dispatched_result, format_tool_result_with_artifact};
pub(crate) use policy::requires_sequential_execution;
#[cfg_attr(not(test), allow(unused_imports))]
use policy::{
    ImageBatchBudget, PreparedCall, ToolExecOutput, ToolPolicyGate, mark_format_confirmed,
    not_executed_tool_result,
};
#[allow(unused_imports)]
pub(crate) use policy::{blocked_tool_result, failed_tool_result, model_format_failed_result};
pub use sub_agent::SubAgentTool;

#[cfg(test)]
#[path = "../runner_tests.rs"]
mod tests;
