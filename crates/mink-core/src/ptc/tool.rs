//! `RunCode` tool definition and async execution coordinator.

use super::bridge::PtcDispatchBridge;
use super::runtime::{PtcFailureKind, PtcOutputItem, PtcRunRequest};
use crate::context::ToolContext;
use crate::protocol::ToolCallEvent;
use crate::tools::metadata::{
    ApprovalTier, ToolFailureKind, ToolMetadata, ToolResultKind, ToolStatus,
};
use crate::tools::runner::{ToolExec, ToolExecution, ToolOutcome};
use crate::ui::{PtcRunDisplay, ToolPresentation};
use anyhow::{Result, bail};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunCodeArgs {
    pub code: String,
    pub description: String,
    #[serde(default)]
    pub timeout: Option<u64>,
}

impl RunCodeArgs {
    pub fn parse(input: &serde_json::Value) -> Result<Self> {
        let args: Self = crate::tools::args::decode_args(input)?;
        if args.description.trim().is_empty() {
            return Err(crate::tools::args::invalid_input(
                "invalid tool arguments: description must not be empty",
            ));
        }
        if args.code.trim().is_empty() {
            return Err(crate::tools::args::invalid_input(
                "invalid tool arguments: code must not be empty",
            ));
        }
        Ok(args)
    }
}

pub struct RunCodeTool;

impl ToolExec for RunCodeTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new("RunCode", ApprovalTier::Exec, ToolResultKind::Command)
            .mutating()
            .storm_exempt()
            .explicit_only()
    }

    fn execute(&self, _input: &serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutcome> {
        bail!("RunCode must be executed through the async ToolRunner path")
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PtcImageBudgetWindow {
    pub remaining_slots: usize,
    pub remaining_bytes: u64,
    pub max_images: usize,
    pub max_bytes: u64,
    pub used_images: usize,
    pub used_bytes: u64,
}

/// Async entry point called by `ToolRunner::dispatch_sequential` when `call.name == "RunCode"`.
pub(crate) async fn execute_run_code(
    ctx: &Arc<ToolContext>,
    call: ToolCallEvent,
    budget: PtcImageBudgetWindow,
) -> (ToolExecution, usize, u64) {
    let args = match RunCodeArgs::parse(&call.input_json) {
        Ok(args) => args,
        Err(error) => {
            return (
                crate::tools::runner::model_format_failed_result(call, format!("{error:#}")),
                0,
                0,
            );
        }
    };

    let Some(runtime) = &ctx.ptc_runtime else {
        return (
            crate::tools::runner::failed_tool_result(
                call.id,
                call.name,
                call.fields,
                "PTC runtime is not configured for this session".to_string(),
            ),
            0,
            0,
        );
    };

    let tool_names: Vec<String> = ctx
        .tool_surface
        .programmatic_tools()
        .iter()
        .map(|t| t.metadata.name.to_string())
        .collect();

    let run_cancel = ctx.cancel.linked_child_token();
    let req = PtcRunRequest {
        code: args.code,
        cwd: ctx.cwd.clone(),
        tool_names,
        timeout_secs: args.timeout,
        cancel: run_cancel.clone(),
        interrupt: ctx.interrupt.clone(),
        sandbox: Some(ctx.tool_config.sandbox.clone()),
    };

    let spec = match runtime.resolve(req) {
        Ok(spec) => spec,
        Err(error) => {
            let msg = format!("{error:#}");
            if msg.contains("timeout") {
                return (
                    crate::tools::runner::model_format_failed_result(call, msg),
                    0,
                    0,
                );
            }
            return (
                crate::tools::runner::failed_tool_result(call.id, call.name, call.fields, msg),
                0,
                0,
            );
        }
    };

    let max_sub_calls = ctx.tool_config.ptc.sub_call_limit(spec.timeout);
    let bridge = Arc::new(PtcDispatchBridge::new(
        ctx.clone(),
        call.id.clone(),
        run_cancel.clone(),
        max_sub_calls,
        ctx.tool_config.ptc.max_parallel_sub_calls,
        format!(
            "{} per minute",
            ctx.tool_config.ptc.max_sub_calls_per_minute
        ),
    ));

    let before_todo_rev = ctx.todo_store.snapshot().revision;
    let run_result = runtime.run(spec, bridge.clone()).await;
    bridge.drain().await;

    let sub_calls = bridge.sub_calls();
    let edited_paths = bridge.edited_paths();
    let todo_advanced = bridge.todo_advanced();
    let unknown_effect = bridge.has_unknown_effect();
    let quota_msg = bridge.quota_exceeded_message();

    let result = match run_result {
        Ok(res) => res,
        Err(error) => {
            return (
                crate::tools::runner::failed_tool_result(
                    call.id,
                    call.name,
                    call.fields,
                    format!("PTC runtime error: {error:#}"),
                ),
                0,
                0,
            );
        }
    };

    let mut text_out = String::new();
    let mut selected_images = Vec::new();
    let mut added_images = 0usize;
    let mut added_bytes = 0u64;
    let mut image_budget_notes = Vec::new();

    for item in result.output {
        match item {
            PtcOutputItem::Text(t) => text_out.push_str(&t),
            PtcOutputItem::Image { image_id } => {
                if let Some(attachment) = bridge.take_image(&image_id) {
                    if added_images < budget.remaining_slots
                        && added_bytes.saturating_add(attachment.bytes) <= budget.remaining_bytes
                    {
                        added_images += 1;
                        added_bytes = added_bytes.saturating_add(attachment.bytes);
                        ctx.this_turn_image_ids
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(attachment.image_id.clone());
                        selected_images.push(attachment);
                    } else {
                        image_budget_notes.push(format!(
                            "Image {} omitted: image attachment batch limit exceeded (usage {}/{} images, {}/{} bytes).",
                            image_id,
                            budget.used_images + added_images,
                            budget.max_images,
                            budget.used_bytes + added_bytes,
                            budget.max_bytes
                        ));
                    }
                }
            }
        }
    }

    if !image_budget_notes.is_empty() {
        if !text_out.is_empty() && !text_out.ends_with('\n') {
            text_out.push('\n');
        }
        text_out.push_str(&image_budget_notes.join("\n"));
    }

    if result.output_truncated {
        if !text_out.is_empty() && !text_out.ends_with('\n') {
            text_out.push('\n');
        }
        text_out.push_str(
            "[Output truncated at PTC output limit; inspect remaining state without repeating completed actions.]",
        );
    }

    let after_todo = ctx.todo_store.snapshot();
    if after_todo.revision > before_todo_rev {
        if !text_out.is_empty() && !text_out.ends_with('\n') {
            text_out.push('\n');
        }
        let read_provider = ctx
            .tool_capabilities
            .primary_provider(
                crate::tools::semantic_capabilities::ToolSemanticCapability::TodoInspect,
            )
            .map(|p| p.tool.as_str())
            .unwrap_or("TodoRead");
        text_out.push_str(&crate::session::todo::render_current_todos(
            &after_todo,
            read_provider,
        ));
    }

    let outer_interrupted = ctx.cancel.is_cancelled() || ctx.interrupt.is_set();
    let status = match &result.error {
        None => ToolStatus::Succeeded,
        Some(err) => match err.kind {
            PtcFailureKind::Abort if outer_interrupted && quota_msg.is_none() => {
                ToolStatus::Interrupted
            }
            PtcFailureKind::Abort => ToolStatus::Failed(ToolFailureKind::Aborted),
            PtcFailureKind::Timeout => ToolStatus::Failed(ToolFailureKind::Timeout),
            PtcFailureKind::SandboxUnavailable => {
                ToolStatus::Failed(ToolFailureKind::SafetyBlocked)
            }
            PtcFailureKind::Exception | PtcFailureKind::WorkerExit | PtcFailureKind::Protocol => {
                ToolStatus::Failed(ToolFailureKind::ProcessFailed)
            }
        },
    };

    let mut composed = String::new();
    if let Some(err) = &result.error {
        let reason = match (&quota_msg, err.kind) {
            (Some(q), _) if q == &err.message => q.clone(),
            (Some(q), _) => format!("{q}\n{}", err.message),
            (None, _) => err.message.clone(),
        };
        composed.push_str(&format!(
            "Error: RunCode failed ({}): {reason}",
            err.kind.as_str()
        ));
        if !text_out.trim().is_empty() {
            composed.push_str("\nCaptured output:\n");
            composed.push_str(&text_out);
        }
    } else if text_out.is_empty() && selected_images.is_empty() {
        composed.push_str("(RunCode completed with no output)");
    } else {
        composed = text_out;
    }

    let formatted = crate::tools::runner::format_tool_result_with_artifact_pub(
        "RunCode",
        &composed,
        ctx.tool_config.tool_result_max_bytes,
        ctx,
    );

    let mut images_iter = selected_images.into_iter();
    let primary_image = images_iter.next();
    let extra_images: Vec<_> = images_iter.collect();

    let presentation = ToolPresentation::PtcRun(PtcRunDisplay {
        description: args.description,
        sub_calls,
        output_truncated: result.output_truncated,
    });

    let exec = ToolExecution {
        tool_use_id: call.id,
        tool_name: call.name,
        tool_args: call.fields,
        content: formatted.content,
        conv_content: String::new(),
        spawns_sub_agent: false,
        sub_agent_prompt: None,
        sub_agent_fork: false,
        sub_agent_label: None,
        sub_agent_model: None,
        exit_code: if status.is_success() {
            Some(0)
        } else {
            Some(1)
        },
        status,
        result_kind: ToolResultKind::Command,
        presentation: Some(presentation),
        artifacts: formatted.artifacts,
        signals: Vec::new(),
        failure_source: None,
        plan_command: None,
        needs_finalization: false,
        state_metadata: unknown_effect.then(|| serde_json::json!({"execution_outcome": "unknown"})),
        image_attachment: primary_image,
        extra_image_attachments: extra_images,
        todo_advanced,
        edited_paths,
    };
    (exec, added_images, added_bytes)
}
