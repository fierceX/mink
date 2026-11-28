use super::*;

// Immediate results are short-lived by construction and already owned by the
// caller; boxing them only to make the enum smaller added an unnecessary
// allocation on the policy-block hot path.
#[allow(clippy::large_enum_variant)]
pub(super) enum PreparedCall {
    Execute(ToolCallEvent),
    Immediate(ToolExecution),
}

pub(super) struct ToolPolicyGate<'a> {
    pub(super) human_web_command: bool,
    pub(super) surface: &'a crate::tools::surface::ModelToolSurface,
    pub(super) storm: &'a Mutex<StormBreaker>,
}

/// Per-`execute_all` image budget counters (v7 §7.3). Created once per
/// `execute_all` and shared across every sequential-tool flushes of the read
/// batch — the budget resets only at the next `execute_all`, so history
/// never locks out new reads while one tool-call group stays bounded
/// (memory and keepable promises).
#[derive(Debug, Default)]
pub(super) struct ImageBatchBudget {
    pub(super) used_images: usize,
    pub(super) used_bytes: u64,
}

pub(super) struct ToolExecOutput {
    pub(super) content: String,
    pub(super) is_bash: bool,
    pub(super) conv_content: String,
    pub(super) exit_code: Option<i32>,
    pub(super) wall_ms: Option<u128>,
    pub(super) no_mutation: bool,
    pub(super) memo_candidate: Option<MemoCandidate>,
    pub(super) spawns_sub_agent: bool,
    pub(super) status: ToolStatus,
    pub(super) diagnostics: Vec<String>,
    pub(super) plan_command: Option<PlanCommand>,
    pub(super) state_metadata: Option<serde_json::Value>,
    pub(super) result_kind: ToolResultKind,
    pub(super) presentation: Option<ToolPresentation>,
    pub(super) structured_value: Option<serde_json::Value>,
    pub(super) failure_source: Option<ToolFailureSource>,
    pub(super) file_summary_metrics: Option<(usize, usize)>,
}

impl ToolExecOutput {
    /// Single constructor for failed executions: success paths keep their own
    /// explicit field mapping, failures always share these defaults.
    pub(super) fn failed(
        content: String,
        status: ToolStatus,
        result_kind: ToolResultKind,
        wall_ms: Option<u128>,
    ) -> Self {
        Self {
            content,
            is_bash: false,
            conv_content: String::new(),
            exit_code: None,
            wall_ms,
            no_mutation: false,
            memo_candidate: None,
            spawns_sub_agent: false,
            status,
            diagnostics: Vec::new(),
            plan_command: None,
            state_metadata: None,
            result_kind,
            presentation: None,
            structured_value: None,
            failure_source: None,
            file_summary_metrics: None,
        }
    }
}

impl ToolRunner {
    pub(super) fn prepare_call(&self, call: ToolCallEvent) -> PreparedCall {
        // Surface membership and storm accounting run first for every call
        // (including known model-format ones), so a crafted bad call cannot
        // bypass them; only then is the known-format failure reported.
        let tool_metadata = self.metadata_for(&call.name);
        let policy = ToolPolicyGate {
            human_web_command: self.ctx.human_web_command,
            surface: &self.ctx.tool_surface,
            storm: &self.storm,
        };
        if let Some(blocked) = policy.evaluate(&call, tool_metadata) {
            return PreparedCall::Immediate(blocked);
        }

        if let Some(mask) = &self.ctx.tool_mask {
            let exclusive_ptc = self.ctx.tool_config.ptc.mode == crate::ptc::PtcMode::Exclusive;
            let allowed = mask.allowed_tools.contains(&call.name)
                || (mask.allow_exclusive_ptc && exclusive_ptc && call.name == "RunCode");
            if !allowed {
                return PreparedCall::Immediate(blocked_tool_result(
                    call.id,
                    call.name,
                    call.fields,
                    mask.block_reason.clone(),
                    ToolBlocker::ToolSurface,
                ));
            }
            if mask.block_custom_resource_schemes
                && call.name == "Read"
                && call.input_json["path"].as_str().is_some_and(|p| {
                    p.contains("://")
                        && !["artifact://", "skill://", "rule://", "session://"]
                            .iter()
                            .any(|scheme| p.starts_with(scheme))
                })
            {
                return PreparedCall::Immediate(blocked_tool_result(
                    call.id,
                    call.name,
                    call.fields,
                    "draft exploration cannot invoke custom resource handlers".into(),
                    ToolBlocker::ToolSurface,
                ));
            }
        }
        // Model-generated arguments that could not be parsed become an
        // ordinary failed tool result (ModelFormat provenance) instead of
        // failing the turn: the model sees the decode diagnostic and can
        // resend corrected arguments. Repaired payloads are never executed.
        if let Some(error) = call.parse_error.clone() {
            if let Some(observer) = &self.ctx.tool_observer {
                observer.on_parse_error(&call.name, &error);
            }
            return PreparedCall::Immediate(model_format_failed_result(
                call,
                format!("invalid tool arguments: {error}"),
            ));
        }

        if let Some(observer) = &self.ctx.tool_observer
            && let Err(error) = observer.before_prepare_execute(&call.name)
        {
            return PreparedCall::Immediate(failed_tool_result(
                call.id,
                call.name,
                call.fields,
                format!("{error:#}"),
            ));
        }
        PreparedCall::Execute(call)
    }
}

impl ToolPolicyGate<'_> {
    pub(super) fn evaluate(
        &self,
        call: &ToolCallEvent,
        metadata: Option<ToolMetadata>,
    ) -> Option<ToolExecution> {
        let Some(metadata) = metadata else {
            // Unknown/disabled names are surface rejections too: they must
            // not bypass the gate and reach dispatch as an "unknown tool"
            // failure with no surface/storm accounting.
            return Some(blocked_tool_result(
                call.id.clone(),
                call.name.clone(),
                call.fields.clone(),
                format!(
                    "Tool '{}' is unavailable in the resolved model tool surface.",
                    call.name
                ),
                ToolBlocker::ToolSurface,
            ));
        };
        let can_run = self.surface.has(&call.name)
            || (self.human_web_command
                && matches!(call.name.as_str(), "WebSearch" | "WebFetch")
                && self.surface.has_programmatic(&call.name));
        if !can_run {
            if self.surface.has_programmatic(&call.name) {
                return Some(blocked_tool_result(
                    call.id.clone(),
                    call.name.clone(),
                    call.fields.clone(),
                    format!(
                        "Tool '{}' is only callable inside RunCode (via await tools.{}(...)) in the current PTC mode.",
                        call.name, call.name
                    ),
                    ToolBlocker::ToolSurface,
                ));
            }
            let reason = self
                .surface
                .hidden()
                .get(&call.name)
                .map(|reason| format!(" ({reason:?})"))
                .unwrap_or_default();
            return Some(blocked_tool_result(
                call.id.clone(),
                call.name.clone(),
                call.fields.clone(),
                format!(
                    "Tool '{}' is unavailable in the resolved model tool surface{reason}.",
                    call.name
                ),
                ToolBlocker::ToolSurface,
            ));
        }
        if metadata.storm_exempt {
            return None;
        }
        // The storm identity uses the original raw-argument digest when the
        // payload did not parse, so distinct bad calls never collapse into
        // one identity through their `{}` placeholder.
        let identity = call.storm_identity();
        let (decision, format_confirmed) = {
            let mut storm = self.storm.lock().unwrap_or_else(|e| e.into_inner());
            let decision = storm.check(&call.name, &identity, metadata.mutating);
            let confirmed = storm.is_format_confirmed(&call.name, &identity, metadata.mutating);
            (decision, confirmed)
        };
        match decision {
            StormDecision::Allow => None,
            StormDecision::Suppress(reason) => {
                let mut blocked = blocked_tool_result(
                    call.id.clone(),
                    call.name.clone(),
                    call.fields.clone(),
                    reason,
                    ToolBlocker::StormBreaker,
                );
                // A repeated known-bad-arguments call keeps its format
                // provenance: suppression must not turn it into a hard
                // execution failure.
                if call.parse_error.is_some() || format_confirmed {
                    blocked.failure_source = Some(ToolFailureSource::ModelFormat);
                }
                Some(blocked)
            }
        }
    }
}

pub(crate) fn requires_sequential_execution(metadata: Option<&ToolMetadata>) -> bool {
    let Some(metadata) = metadata else {
        return false;
    };
    metadata.mutating
        || metadata.spawns_sub_agent
        || matches!(metadata.approval, ApprovalTier::Write | ApprovalTier::Exec)
        || matches!(
            metadata.result_kind,
            ToolResultKind::FileWrite
                | ToolResultKind::Edit
                | ToolResultKind::Command
                | ToolResultKind::Control
                | ToolResultKind::SubAgent
        )
}

pub(crate) fn blocked_tool_result(
    id: String,
    name: String,
    args: BTreeMap<String, String>,
    reason: String,
    blocker: ToolBlocker,
) -> ToolExecution {
    ToolExecution {
        tool_use_id: id,
        tool_name: name,
        tool_args: args,
        content: format!("Error: {reason}"),
        conv_content: String::new(),
        spawns_sub_agent: false,
        sub_agent_prompt: None,
        sub_agent_fork: false,
        sub_agent_label: None,
        sub_agent_model: None,
        exit_code: None,
        status: ToolStatus::Blocked(blocker),
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

pub(crate) fn failed_tool_result(
    id: String,
    name: String,
    args: BTreeMap<String, String>,
    reason: String,
) -> ToolExecution {
    let mut result = blocked_tool_result(id, name, args, reason.clone(), ToolBlocker::ToolSurface);
    result.status =
        ToolStatus::Failed(crate::tools::metadata::classify_failure_kind(&reason, None));
    result
}

/// Failed tool call produced by model-output formatting (bad arguments).
/// The signal layer routes these as format feedback, not execution failures.
pub(crate) fn model_format_failed_result(call: ToolCallEvent, reason: String) -> ToolExecution {
    let mut result = failed_tool_result(call.id, call.name, call.fields, reason);
    result.status = ToolStatus::Failed(ToolFailureKind::ArgumentInvalid);
    result.failure_source = Some(ToolFailureSource::ModelFormat);
    result
}

/// Flag the matching bounded storm entry after a call was confirmed as a pure
/// model-format decode failure. No second cache is introduced.
pub(super) fn mark_format_confirmed(
    storm: &Mutex<StormBreaker>,
    name: &str,
    identity: &str,
    result: &ToolExecution,
) {
    if result.failure_source != Some(ToolFailureSource::ModelFormat) {
        return;
    }
    let mut storm = storm.lock().unwrap_or_else(|error| error.into_inner());
    storm.mark_format(name, identity);
}

/// Explicit "this call was never dispatched" result, used when a latched
/// session halts a tool batch mid-way.
pub(super) fn not_executed_tool_result(call: ToolCallEvent, reason: &str) -> ToolExecution {
    failed_tool_result(call.id, call.name, call.fields, reason.to_string())
}

pub(super) fn unknown_tool_result(call: ToolCallEvent, error: &anyhow::Error) -> ToolExecution {
    let mut result = failed_tool_result(
        call.id,
        call.name,
        call.fields,
        format!("execution outcome unknown: {error:#}"),
    );
    result.state_metadata = Some(serde_json::json!({"execution_outcome": "unknown"}));
    result
}
