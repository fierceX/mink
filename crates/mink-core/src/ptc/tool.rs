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
        if let Some(0) = args.timeout {
            return Err(crate::tools::args::invalid_input(
                "invalid tool arguments: timeout must be >= 1",
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
        .filter(|name| {
            let Some(mask) = &ctx.tool_mask else {
                return true;
            };
            mask.allow_exclusive_ptc
                && mask.ptc_allowed_tools.contains(name)
                && !ctx.custom_tools.iter().any(|c| c.definition.name == *name)
        })
        .collect();

    let code_bytes = args.code.len();
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

    let run_started = std::time::Instant::now();
    let max_sub_calls = ctx.tool_config.ptc.sub_call_limit(spec.timeout);
    let bridge = Arc::new(
        PtcDispatchBridge::new(
            ctx.clone(),
            call.id.clone(),
            run_cancel.clone(),
            max_sub_calls,
            ctx.tool_config.ptc.max_parallel_sub_calls,
            format!(
                "{} per minute",
                ctx.tool_config.ptc.max_sub_calls_per_minute
            ),
        )
        .with_deadline(run_started + spec.timeout),
    );

    let run_result = runtime.run(spec, bridge.clone()).await;
    bridge.drain().await;

    let sub_calls = bridge.sub_calls();
    let edited_paths = bridge.edited_paths();
    let todo_advanced = bridge.todo_advanced();
    let unknown_effect = bridge.has_unknown_effect();
    let quota_msg = bridge.quota_exceeded_message();
    let raw_sub_call_bytes = bridge.raw_sub_call_bytes();
    let failed_sub_calls = bridge.failed_sub_calls();

    let result = match run_result {
        Ok(res) => res,
        Err(error) => {
            emit_ptc_run_summary(
                ctx,
                &call.id,
                &sub_calls,
                failed_sub_calls,
                code_bytes,
                raw_sub_call_bytes,
                0,
                false,
                run_started.elapsed().as_millis() as u64,
                "runtime_error",
            );
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

    let inspected_output_bytes = text_out.len();
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
        if !sub_calls.is_empty() {
            composed.push('\n');
            composed.push_str(&format_sub_call_progress_summary(&sub_calls));
        }
        if !text_out.trim().is_empty() {
            composed.push_str("\nCaptured output:\n");
            composed.push_str(&text_out);
        }
    } else if text_out.is_empty() && selected_images.is_empty() {
        composed.push_str("(RunCode completed with no output)");
    } else {
        composed = text_out;
    }

    let outcome_label = result.error.as_ref().map_or("ok", |err| err.kind.as_str());
    emit_ptc_run_summary(
        ctx,
        &call.id,
        &sub_calls,
        failed_sub_calls,
        code_bytes,
        raw_sub_call_bytes,
        inspected_output_bytes,
        result.output_truncated,
        run_started.elapsed().as_millis() as u64,
        outcome_label,
    );

    let (filtered_composed, _, _, _, _, policy_effects) =
        crate::tools::runner::apply_tool_output_filters(
            ctx,
            crate::tools::rim_hooks::ToolOutputFilterContext {
                session_id: &ctx.session_id,
                call_id: &call.id,
                tool_name: &call.name,
                is_sub_agent: ctx.is_sub_agent,
                is_ptc_nested: false,
                succeeded: status.is_success(),
            },
            composed,
            String::new(),
            None,
        );

    let formatted = crate::tools::runner::format_tool_result_with_artifact_pub(
        "RunCode",
        &filtered_composed,
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

    let mut state_metadata =
        unknown_effect.then(|| serde_json::json!({"execution_outcome": "unknown"}));
    for effect in &policy_effects {
        crate::tools::runner::merge_policy_effect_metadata(&mut state_metadata, effect);
    }

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
        exit_code: match &result.error {
            None => Some(0),
            Some(err)
                if matches!(
                    err.kind,
                    PtcFailureKind::Exception | PtcFailureKind::WorkerExit
                ) =>
            {
                Some(1)
            }
            Some(_) => None,
        },
        status,
        result_kind: ToolResultKind::Command,
        presentation: Some(presentation),
        artifacts: formatted.artifacts,
        signals: Vec::new(),
        failure_source: None,
        plan_command: None,
        needs_finalization: false,
        state_metadata,
        image_attachment: primary_image,
        extra_image_attachments: extra_images,
        todo_advanced,
        edited_paths,
        policy_effects,
    };
    (exec, added_images, added_bytes)
}

fn format_sub_call_progress_summary(sub_calls: &[crate::ui::PtcSubCallDisplay]) -> String {
    const MAX_SHOWN: usize = 20;
    let items: Vec<String> = sub_calls
        .iter()
        .take(MAX_SHOWN)
        .map(|sc| {
            let label = match sc.status {
                ToolStatus::Succeeded => "ok",
                ToolStatus::Failed(_) => "failed",
                ToolStatus::Blocked(_) => "blocked",
                ToolStatus::Interrupted => "interrupted",
            };
            format!("{}[{label}]", sc.tool_name)
        })
        .collect();
    if sub_calls.len() > MAX_SHOWN {
        format!(
            "Sub-calls executed before failure ({}): {}, +{} more",
            sub_calls.len(),
            items.join(", "),
            sub_calls.len() - MAX_SHOWN
        )
    } else {
        format!(
            "Sub-calls executed before failure ({}): {}",
            sub_calls.len(),
            items.join(", ")
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_ptc_run_summary(
    ctx: &ToolContext,
    parent_tool_use_id: &str,
    sub_calls: &[crate::ui::PtcSubCallDisplay],
    failed_sub_calls: usize,
    code_bytes: usize,
    raw_sub_call_bytes: usize,
    inspected_output_bytes: usize,
    output_truncated: bool,
    duration_ms: u64,
    outcome: &str,
) {
    let Some(sink) = &ctx.ptc_event_sink else {
        return;
    };
    let mut unique_tools: Vec<String> = sub_calls.iter().map(|s| s.tool_name.clone()).collect();
    unique_tools.sort();
    unique_tools.dedup();
    sink(crate::events::EventLog::PtcRunSummary {
        parent_tool_use_id: parent_tool_use_id.to_string(),
        mode: ctx.tool_config.ptc.mode.as_str().to_string(),
        sub_call_count: sub_calls.len(),
        failed_sub_calls,
        unique_tools,
        code_bytes,
        raw_sub_call_bytes,
        inspected_output_bytes,
        output_truncated,
        duration_ms,
        outcome: outcome.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptc::runtime::{
        PtcBindingDispatcher, PtcRunFailure, PtcRunResult, PtcRunSpec, PtcRuntime,
    };
    use crate::ptc::{PtcBackendKind, PtcConfig, PtcMode};
    use crate::tools::metadata::ToolFailureSource;

    struct StubPtcRuntime {
        result: PtcRunResult,
    }

    #[async_trait::async_trait]
    impl PtcRuntime for StubPtcRuntime {
        fn kind(&self) -> PtcBackendKind {
            PtcBackendKind::SubprocessPython
        }
        fn resolve(&self, request: PtcRunRequest) -> Result<PtcRunSpec> {
            crate::ptc::protocol::resolve_timeout_and_cwd(&PtcConfig::default(), request)
        }
        async fn run(
            &self,
            _spec: PtcRunSpec,
            _dispatcher: Arc<dyn PtcBindingDispatcher>,
        ) -> Result<PtcRunResult> {
            Ok(self.result.clone())
        }
    }

    #[tokio::test]
    async fn zero_timeout_is_argument_invalid_and_exit_code_matches_failure_kind() {
        let shared =
            crate::regression::test_context_for_agent_with_config("ptc-tool-exit-code", |cfg| {
                cfg.ptc = PtcConfig {
                    mode: PtcMode::Exclusive,
                    ..PtcConfig::default()
                };
            })
            .await
            .unwrap();
        let mut ctx_val = ToolContext::from_shared(&shared);
        ctx_val.ptc_runtime = Some(Arc::new(StubPtcRuntime {
            result: PtcRunResult {
                output: Vec::new(),
                output_truncated: false,
                error: Some(PtcRunFailure {
                    kind: PtcFailureKind::Timeout,
                    message: "timed out".into(),
                }),
            },
        }));
        let ctx = Arc::new(ctx_val);

        let budget = PtcImageBudgetWindow {
            remaining_slots: 4,
            remaining_bytes: 1024 * 1024,
            max_images: 4,
            max_bytes: 1024 * 1024,
            used_images: 0,
            used_bytes: 0,
        };

        let (zero_timeout, _, _) = execute_run_code(
            &ctx,
            ToolCallEvent {
                name: "RunCode".into(),
                id: "call_0".into(),
                input_json: serde_json::json!({"code": "1", "description": "bad", "timeout": 0}),
                fields: Default::default(),
                parse_error: None,
                raw_arguments_digest: None,
            },
            budget,
        )
        .await;
        assert_eq!(
            zero_timeout.status,
            ToolStatus::Failed(ToolFailureKind::ArgumentInvalid)
        );
        assert_eq!(
            zero_timeout.failure_source,
            Some(ToolFailureSource::ModelFormat)
        );

        let (timed_out, _, _) = execute_run_code(
            &ctx,
            ToolCallEvent {
                name: "RunCode".into(),
                id: "call_1".into(),
                input_json: serde_json::json!({"code": "1", "description": "timeout test"}),
                fields: Default::default(),
                parse_error: None,
                raw_arguments_digest: None,
            },
            budget,
        )
        .await;
        assert_eq!(
            timed_out.status,
            ToolStatus::Failed(ToolFailureKind::Timeout)
        );
        assert_eq!(timed_out.exit_code, None);

        let mut ctx_exc = (*ctx).clone();
        ctx_exc.ptc_runtime = Some(Arc::new(StubPtcRuntime {
            result: PtcRunResult {
                output: Vec::new(),
                output_truncated: false,
                error: Some(PtcRunFailure {
                    kind: PtcFailureKind::Exception,
                    message: "ValueError".into(),
                }),
            },
        }));
        let (exc_res, _, _) = execute_run_code(
            &Arc::new(ctx_exc),
            ToolCallEvent {
                name: "RunCode".into(),
                id: "call_2".into(),
                input_json: serde_json::json!({"code": "1", "description": "exc test"}),
                fields: Default::default(),
                parse_error: None,
                raw_arguments_digest: None,
            },
            budget,
        )
        .await;
        assert_eq!(
            exc_res.status,
            ToolStatus::Failed(ToolFailureKind::ProcessFailed)
        );
        assert_eq!(exc_res.exit_code, Some(1));
    }

    #[tokio::test]
    async fn exclusive_readonly_mask_permits_run_code_through_tool_runner() {
        let shared = crate::regression::test_context_for_agent_with_config(
            "ptc-runner-exclusive-draft",
            |cfg| {
                cfg.ptc = PtcConfig {
                    mode: PtcMode::Exclusive,
                    ..PtcConfig::default()
                };
            },
        )
        .await
        .unwrap();
        let mut ctx_val = ToolContext::from_shared(&shared);
        ctx_val.tool_mask = Some(
            crate::tools::policy_hook::ToolExecutionMask::allow_only(["Read", "Glob", "Grep"])
                .with_exclusive_ptc(true)
                .with_block_custom_resource_schemes(true)
                .with_disable_native_web(true)
                .with_disable_replan(true)
                .with_block_reason("read-only exploration only"),
        );
        ctx_val.ptc_runtime = Some(Arc::new(StubPtcRuntime {
            result: PtcRunResult {
                output: vec![PtcOutputItem::Text("draft inspected".into())],
                output_truncated: false,
                error: None,
            },
        }));
        let runner = crate::tools::runner::ToolRunner::new(Arc::new(ctx_val));
        let results = runner
            .execute_all(vec![ToolCallEvent {
                name: "RunCode".into(),
                id: "call_draft_ptc".into(),
                input_json: serde_json::json!({"code": "inspect('ok')", "description": "explore"}),
                fields: Default::default(),
                parse_error: None,
                raw_arguments_digest: None,
            }])
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, ToolStatus::Succeeded);
        assert!(results[0].content.contains("draft inspected"));
    }

    #[tokio::test]
    async fn emits_run_summary_includes_sub_call_progress_on_failure_and_preserves_nohup() {
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_sink = events.clone();

        let shared =
            crate::regression::test_context_for_agent_with_config("ptc-tool-summary-bg", |cfg| {
                cfg.ptc = PtcConfig {
                    mode: PtcMode::Exclusive,
                    ..PtcConfig::default()
                };
            })
            .await
            .unwrap();
        let mut ctx_val = ToolContext::from_shared(&shared);
        ctx_val.ptc_runtime = Some(Arc::new(
            crate::ptc::subprocess_py::SubprocessPythonPtcRuntime::new(
                ctx_val.tool_config.ptc.clone(),
            ),
        ));
        ctx_val.ptc_event_sink = Some(Arc::new(move |evt| {
            events_sink.lock().unwrap().push(evt);
        }));
        std::fs::write(ctx_val.cwd.join("sample.txt"), "hello\n").unwrap();
        let ctx = Arc::new(ctx_val);

        let budget = PtcImageBudgetWindow {
            remaining_slots: 4,
            remaining_bytes: 1024 * 1024,
            max_images: 4,
            max_bytes: 1024 * 1024,
            used_images: 0,
            used_bytes: 0,
        };

        // 1. Failing RunCode after Read succeeds reports Sub-calls executed before failure + emits PtcRunSummary.
        let (fail_exec, _, _) = execute_run_code(
            &ctx,
            ToolCallEvent {
                name: "RunCode".into(),
                id: "call_summary_fail".into(),
                input_json: serde_json::json!({
                    "code": "r = await tools.Read(path='sample.txt')\ninspect('partial')\nraise RuntimeError('boom')",
                    "description": "fail after read"
                }),
                fields: Default::default(),
                parse_error: None,
                raw_arguments_digest: None,
            },
            budget,
        )
        .await;
        assert_eq!(
            fail_exec.status,
            ToolStatus::Failed(ToolFailureKind::ProcessFailed)
        );
        assert!(
            fail_exec
                .content
                .contains("Sub-calls executed before failure (1): Read[ok]"),
            "{}",
            fail_exec.content
        );
        assert!(
            fail_exec.content.contains("partial"),
            "{}",
            fail_exec.content
        );

        // 2. Background nohup started inside await tools.Bash(...) survives RunCode exit cleanly.
        let (bg_exec, _, _) = execute_run_code(
            &ctx,
            ToolCallEvent {
                name: "RunCode".into(),
                id: "call_bg_nohup".into(),
                input_json: serde_json::json!({
                    "code": "res = await tools.Bash(command=\"nohup sh -c 'sleep 0.25; echo bg-ok > bg.txt' > /dev/null 2>&1 < /dev/null &\")\ninspect(res['exit_code'])",
                    "description": "start nohup background job",
                    "timeout": 10
                }),
                fields: Default::default(),
                parse_error: None,
                raw_arguments_digest: None,
            },
            budget,
        )
        .await;
        assert_eq!(bg_exec.status, ToolStatus::Succeeded, "{}", bg_exec.content);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(
            std::fs::read_to_string(ctx.cwd.join("bg.txt"))
                .unwrap_or_default()
                .trim(),
            "bg-ok"
        );

        let recorded = events.lock().unwrap().clone();
        let summaries: Vec<_> = recorded
            .iter()
            .filter_map(|e| match e {
                crate::events::EventLog::PtcRunSummary {
                    parent_tool_use_id,
                    sub_call_count,
                    outcome,
                    ..
                } => Some((parent_tool_use_id.clone(), *sub_call_count, outcome.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            summaries,
            vec![
                ("call_summary_fail".to_string(), 1, "exception".to_string()),
                ("call_bg_nohup".to_string(), 1, "ok".to_string()),
            ]
        );
    }
}
