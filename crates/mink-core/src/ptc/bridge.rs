//! Nested tool dispatch bridge for PTC runs.
//!
//! Enforces programmatic surface membership, rolling bounded concurrency
//! (parallel read-only calls up to `max_parallel`, serialized barriers for
//! mutating/exec calls), total sub-call quota, quiescence drain, event logging,
//! and image/todo/edit state tracking.

use super::runtime::{
    PtcBindingDispatcher, PtcBindingError, PtcBindingSuccess, PtcImageDescriptor,
};
use crate::cancel::{CancellationToken, InterruptSignal};
use crate::context::ToolContext;
use crate::events::EventLog;
use crate::protocol::ToolCallEvent;
use crate::tools::image::ImageAttachment;
use crate::tools::metadata::{ToolFailureKind, ToolStatus};
use crate::tools::runner::requires_sequential_execution;
use crate::ui::PtcSubCallDisplay;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{Notify, RwLock, Semaphore};

/// Bridges nested `tools.<Name>(...)` invocations from a `PtcRuntime` to `ToolRunner`.
pub struct PtcDispatchBridge {
    ctx: Arc<ToolContext>,
    parent_call_id: String,
    run_cancel: CancellationToken,
    run_interrupt: InterruptSignal,
    cancel_watcher: Option<tokio::task::JoinHandle<()>>,
    run_deadline: Instant,
    max_sub_calls: usize,
    sub_call_limit_label: String,
    attempt_seq: AtomicUsize,
    lane_lock: RwLock<()>,
    parallel_slots: Semaphore,
    active_dispatches: Arc<AtomicUsize>,
    drained_notify: Arc<Notify>,
    state: Mutex<BridgeState>,
}

impl Drop for PtcDispatchBridge {
    fn drop(&mut self) {
        if let Some(handle) = self.cancel_watcher.take() {
            handle.abort();
        }
    }
}

#[derive(Default)]
struct BridgeState {
    sub_calls: Vec<PtcSubCallDisplay>,
    images_by_id: HashMap<String, ImageAttachment>,
    unknown_effect: bool,
    todo_advanced: bool,
    edited_paths: Vec<String>,
    quota_exceeded_message: Option<String>,
    raw_sub_call_bytes: usize,
    failed_sub_calls: usize,
}

struct ActiveDispatchGuard {
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Drop for ActiveDispatchGuard {
    fn drop(&mut self) {
        if self.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.notify.notify_waiters();
        }
    }
}

impl PtcDispatchBridge {
    pub fn new(
        ctx: Arc<ToolContext>,
        parent_call_id: impl Into<String>,
        run_cancel: CancellationToken,
        max_sub_calls: usize,
        max_parallel: usize,
        sub_call_limit_label: impl Into<String>,
    ) -> Self {
        let mut bridge_ctx = (*ctx).clone();
        bridge_ctx.bypass_read_memo = true;
        let run_interrupt = InterruptSignal::new();
        if ctx.interrupt.is_set() || run_cancel.is_cancelled() {
            run_interrupt.trigger();
        }
        let cancel_watcher = tokio::runtime::Handle::try_current().ok().map(|handle| {
            let watcher_cancel = run_cancel.clone();
            let watcher_parent_interrupt = ctx.interrupt.clone();
            let watcher_run_interrupt = run_interrupt.clone();
            handle.spawn(async move {
                tokio::select! {
                    _ = watcher_cancel.cancelled() => {
                        watcher_run_interrupt.trigger();
                    }
                    _ = watcher_parent_interrupt.notified() => {
                        watcher_run_interrupt.trigger();
                        watcher_cancel.cancel();
                    }
                }
            })
        });
        Self {
            ctx: Arc::new(bridge_ctx),
            parent_call_id: parent_call_id.into(),
            run_cancel,
            run_interrupt,
            cancel_watcher,
            run_deadline: Instant::now() + std::time::Duration::from_secs(3600),
            max_sub_calls: max_sub_calls.max(1),
            sub_call_limit_label: sub_call_limit_label.into(),
            attempt_seq: AtomicUsize::new(0),
            lane_lock: RwLock::new(()),
            parallel_slots: Semaphore::new(max_parallel.max(1)),
            active_dispatches: Arc::new(AtomicUsize::new(0)),
            drained_notify: Arc::new(Notify::new()),
            state: Mutex::new(BridgeState::default()),
        }
    }

    pub fn with_deadline(mut self, run_deadline: Instant) -> Self {
        self.run_deadline = run_deadline;
        self
    }

    /// Abort any queued work and wait until every started nested dispatch has settled.
    pub async fn drain(&self) {
        self.run_interrupt.trigger();
        self.run_cancel.cancel();
        while self.active_dispatches.load(Ordering::SeqCst) > 0 {
            let notified = self.drained_notify.notified();
            if self.active_dispatches.load(Ordering::SeqCst) == 0 {
                break;
            }
            notified.await;
        }
    }

    pub fn sub_calls(&self) -> Vec<PtcSubCallDisplay> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sub_calls
            .clone()
    }

    pub fn take_image(&self, image_id: &str) -> Option<ImageAttachment> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .images_by_id
            .get(image_id)
            .cloned()
    }

    pub fn has_unknown_effect(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unknown_effect
    }

    pub fn todo_advanced(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .todo_advanced
    }

    pub fn edited_paths(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .edited_paths
            .clone()
    }

    pub fn quota_exceeded_message(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .quota_exceeded_message
            .clone()
    }

    pub fn raw_sub_call_bytes(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .raw_sub_call_bytes
    }

    pub fn failed_sub_calls(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failed_sub_calls
    }

    fn clamp_subcall_timeout(
        &self,
        tool_name: &str,
        input_json: &mut Value,
        fields: &mut BTreeMap<String, String>,
    ) -> std::result::Result<(), PtcBindingError> {
        let remaining = self.run_deadline.saturating_duration_since(Instant::now());
        if remaining < std::time::Duration::from_secs(3) {
            return Err(PtcBindingError {
                tool_name: tool_name.to_string(),
                kind: ToolFailureKind::Timeout,
                message: format!(
                    "remaining RunCode timeout budget is too small (< 3s) to start {tool_name}"
                ),
            });
        }
        let max_subcall_secs = remaining.as_secs().saturating_sub(2).max(1);
        let configured_max = u64::try_from(self.ctx.tool_config.tool_timeout_max_secs)
            .unwrap_or(0)
            .max(5);
        let req_timeout = input_json.get("timeout").and_then(Value::as_u64);
        let clamp_to = match req_timeout {
            Some(req) if req > 0 && req <= configured_max && req > max_subcall_secs => {
                Some(max_subcall_secs)
            }
            Some(_) => None,
            None => {
                let default_req = match tool_name {
                    "PythonSandbox" => self.ctx.tool_config.sandbox_python.timeout.clamp(5, 300),
                    _ => u64::try_from(self.ctx.tool_config.tool_timeout_secs)
                        .unwrap_or(120)
                        .clamp(5, configured_max),
                };
                (default_req > max_subcall_secs).then_some(max_subcall_secs)
            }
        };
        if let Some(clamped) = clamp_to
            && let Some(obj) = input_json.as_object_mut()
        {
            obj.insert("timeout".to_string(), Value::from(clamped));
            fields.insert("timeout".to_string(), clamped.to_string());
        }
        Ok(())
    }

    fn emit_event(&self, event: EventLog) {
        if let Some(sink) = &self.ctx.ptc_event_sink {
            sink(event);
        }
    }

    fn is_sequential_tool(&self, name: &str) -> bool {
        if let Some(custom) = self
            .ctx
            .custom_tools
            .iter()
            .find(|t| t.definition.name == name)
        {
            let tracks_effects = self
                .ctx
                .effect_journal
                .as_ref()
                .is_some_and(|journal| journal.tracks_durable_effects());
            return custom.definition.execution
                == crate::tools::extension::ToolExecutionMode::Sequential
                || (tracks_effects && !custom.definition.side_effect_exempt);
        }
        let metadata = self
            .ctx
            .tool_surface
            .programmatic_get(name)
            .map(|t| &t.metadata);
        requires_sequential_execution(metadata)
    }
}

#[allow(clippy::double_must_use)]
#[async_trait]
impl PtcBindingDispatcher for PtcDispatchBridge {
    fn cancel_pending(&self) {
        self.run_interrupt.trigger();
        self.run_cancel.cancel();
    }

    async fn invoke(
        &self,
        tool_name: &str,
        args: Value,
    ) -> std::result::Result<PtcBindingSuccess, PtcBindingError> {
        if self.run_cancel.is_cancelled()
            || self.ctx.cancel.is_cancelled()
            || self.ctx.interrupt.is_set()
        {
            return Err(PtcBindingError {
                tool_name: tool_name.to_string(),
                kind: ToolFailureKind::Aborted,
                message: "RunCode execution is cancelled".to_string(),
            });
        }
        if let Err(error) = self.ctx.persistence_fault.check() {
            return Err(PtcBindingError {
                tool_name: tool_name.to_string(),
                kind: ToolFailureKind::Unknown,
                message: format!("session state persistence fault: {error:#}"),
            });
        }

        let seq = self.attempt_seq.fetch_add(1, Ordering::SeqCst) + 1;
        if seq > self.max_sub_calls {
            let msg = format!(
                "RunCode exceeded its limit of {} tool calls ({}); call {seq} was not dispatched. Check loop termination or split the work into smaller RunCode calls.",
                self.max_sub_calls, self.sub_call_limit_label
            );
            {
                let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                guard
                    .quota_exceeded_message
                    .get_or_insert_with(|| msg.clone());
            }
            self.run_cancel.cancel();
            return Err(PtcBindingError {
                tool_name: tool_name.to_string(),
                kind: ToolFailureKind::Aborted,
                message: msg,
            });
        }

        if tool_name == "RunCode" || !self.ctx.tool_surface.has_programmatic(tool_name) {
            return Err(PtcBindingError {
                tool_name: tool_name.to_string(),
                kind: ToolFailureKind::SafetyBlocked,
                message: format!(
                    "Tool '{tool_name}' is not callable inside RunCode in the resolved tool surface"
                ),
            });
        }
        if let Some(mask) = &self.ctx.tool_mask {
            if !mask.allow_exclusive_ptc {
                return Err(PtcBindingError {
                    tool_name: tool_name.to_string(),
                    kind: ToolFailureKind::SafetyBlocked,
                    message: mask.block_reason.clone(),
                });
            }
            if self.ctx.tool_config.ptc.mode != super::PtcMode::Exclusive {
                return Err(PtcBindingError {
                    tool_name: tool_name.to_string(),
                    kind: ToolFailureKind::SafetyBlocked,
                    message: "RunCode is unavailable under the current turn policy".to_string(),
                });
            }
            let is_custom = self
                .ctx
                .custom_tools
                .iter()
                .any(|t| t.definition.name == tool_name);
            if !mask.ptc_allowed_tools.contains(tool_name) || is_custom {
                return Err(PtcBindingError {
                    tool_name: tool_name.to_string(),
                    kind: ToolFailureKind::SafetyBlocked,
                    message: format!(
                        "Tool '{tool_name}' is blocked inside RunCode under the current turn policy"
                    ),
                });
            }
            if mask.block_custom_resource_schemes
                && tool_name == "Read"
                && args.get("path").and_then(Value::as_str).is_some_and(|p| {
                    p.contains("://")
                        && !["artifact://", "skill://", "rule://", "session://"]
                            .iter()
                            .any(|scheme| p.starts_with(scheme))
                })
            {
                return Err(PtcBindingError {
                    tool_name: tool_name.to_string(),
                    kind: ToolFailureKind::SafetyBlocked,
                    message: "draft exploration cannot invoke custom resource handlers".to_string(),
                });
            }
        }

        let mut input_json = match args {
            Value::Null => serde_json::json!({}),
            Value::Object(_) => args,
            other => {
                return Err(PtcBindingError {
                    tool_name: tool_name.to_string(),
                    kind: ToolFailureKind::ArgumentInvalid,
                    message: format!("tool arguments must be a JSON object, got {other}"),
                });
            }
        };
        let mut fields = input_json
            .as_object()
            .map(|map| {
                map.iter()
                    .map(|(k, v)| {
                        let s = v
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string());
                        (k.clone(), s)
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();

        if matches!(tool_name, "Bash" | "Python" | "PythonSandbox") {
            self.clamp_subcall_timeout(tool_name, &mut input_json, &mut fields)?;
        }

        let sub_call_id = format!("{}:ptc:{seq}", self.parent_call_id);
        let call = ToolCallEvent {
            name: tool_name.to_string(),
            id: sub_call_id.clone(),
            input_json: input_json.clone(),
            fields,
            parse_error: None,
            raw_arguments_digest: None,
        };

        let is_sequential = self.is_sequential_tool(tool_name);
        let _write_guard;
        let _read_guard;
        let _permit;
        if is_sequential {
            let guard = tokio::select! {
                _ = self.run_cancel.cancelled() => {
                    return Err(PtcBindingError {
                        tool_name: tool_name.to_string(),
                        kind: ToolFailureKind::Aborted,
                        message: "RunCode execution cancelled while queued".to_string(),
                    });
                }
                g = self.lane_lock.write() => g,
            };
            _write_guard = Some(guard);
            _read_guard = None;
            _permit = None;
        } else {
            let guard = tokio::select! {
                _ = self.run_cancel.cancelled() => {
                    return Err(PtcBindingError {
                        tool_name: tool_name.to_string(),
                        kind: ToolFailureKind::Aborted,
                        message: "RunCode execution cancelled while queued".to_string(),
                    });
                }
                g = self.lane_lock.read() => g,
            };
            let permit = tokio::select! {
                _ = self.run_cancel.cancelled() => {
                    return Err(PtcBindingError {
                        tool_name: tool_name.to_string(),
                        kind: ToolFailureKind::Aborted,
                        message: "RunCode execution cancelled while queued".to_string(),
                    });
                }
                p = self.parallel_slots.acquire() => p.map_err(|_| PtcBindingError {
                    tool_name: tool_name.to_string(),
                    kind: ToolFailureKind::Aborted,
                    message: "PTC dispatch pool closed".to_string(),
                })?,
            };
            _write_guard = None;
            _read_guard = Some(guard);
            _permit = Some(permit);
        }

        if self.run_cancel.is_cancelled() {
            return Err(PtcBindingError {
                tool_name: tool_name.to_string(),
                kind: ToolFailureKind::Aborted,
                message: "RunCode execution cancelled".to_string(),
            });
        }

        self.active_dispatches.fetch_add(1, Ordering::SeqCst);
        let _active_guard = ActiveDispatchGuard {
            count: self.active_dispatches.clone(),
            notify: self.drained_notify.clone(),
        };

        self.emit_event(EventLog::PtcDispatchStart {
            parent_tool_use_id: self.parent_call_id.clone(),
            sub_call_id: sub_call_id.clone(),
            tool_name: tool_name.to_string(),
            input: input_json,
        });

        let started = Instant::now();
        // Run-scoped context shares snapshots/memos/stores with parent, while binding
        // cancellation to `run_cancel` (parent cancel/interrupt also cancels `run_cancel`).
        let mut nested_ctx = (*self.ctx).clone();
        nested_ctx.cancel = self.run_cancel.linked_child_token();
        nested_ctx.interrupt = self.run_interrupt.clone();
        let remaining_secs = self
            .run_deadline
            .saturating_duration_since(Instant::now())
            .as_secs()
            .saturating_sub(2)
            .max(1);
        if let Ok(bounded_secs) = i32::try_from(remaining_secs) {
            nested_ctx.tool_config.tool_timeout_secs =
                nested_ctx.tool_config.tool_timeout_secs.min(bounded_secs);
            nested_ctx.tool_config.tool_timeout_max_secs = nested_ctx
                .tool_config
                .tool_timeout_max_secs
                .min(bounded_secs.max(5));
        }
        let nested_ctx = Arc::new(nested_ctx);

        let outcome = crate::tools::runner::execute_nested_tool_call(&nested_ctx, call).await;
        let duration_ms = started.elapsed().as_millis() as u64;

        match outcome {
            Ok(res) => {
                let summary = truncate_summary(&res.rendered_text, 240);
                self.emit_event(EventLog::PtcDispatchEnd {
                    parent_tool_use_id: self.parent_call_id.clone(),
                    sub_call_id: sub_call_id.clone(),
                    tool_name: tool_name.to_string(),
                    status: res.status,
                    exit_code: res.exit_code,
                    duration_ms,
                    summary,
                    result_kind: res.result_kind,
                });

                {
                    let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    guard.sub_calls.push(PtcSubCallDisplay {
                        sub_call_id: sub_call_id.clone(),
                        tool_name: tool_name.to_string(),
                        status: res.status,
                        duration_ms,
                    });
                    guard.raw_sub_call_bytes = guard
                        .raw_sub_call_bytes
                        .saturating_add(res.rendered_text.len());
                    if !res.status.is_success() {
                        guard.failed_sub_calls = guard.failed_sub_calls.saturating_add(1);
                    }
                    if res.unknown_effect {
                        guard.unknown_effect = true;
                    }
                    if res.todo_advanced {
                        guard.todo_advanced = true;
                    }
                    guard.edited_paths.extend(res.edited_paths);
                    if let Some((ref attachment, _)) = res.image {
                        guard
                            .images_by_id
                            .insert(attachment.image_id.clone(), attachment.clone());
                    }
                }

                if !res.status.is_success() {
                    if tool_name == "Bash"
                        && res.status == ToolStatus::Failed(ToolFailureKind::ProcessFailed)
                        && let Some(val) = res.structured_value
                    {
                        return Ok(PtcBindingSuccess {
                            value: val,
                            inspection_text: Some(res.rendered_text),
                            image: None,
                        });
                    }
                    let kind = res
                        .status
                        .failure_kind()
                        .unwrap_or(ToolFailureKind::Unknown);
                    return Err(PtcBindingError {
                        tool_name: tool_name.to_string(),
                        kind,
                        message: res.rendered_text,
                    });
                }

                if let Some((attachment, path)) = res.image {
                    return Ok(PtcBindingSuccess {
                        value: serde_json::json!({
                            "image_id": attachment.image_id,
                            "path": path,
                            "format": format!("{:?}", attachment.format).to_lowercase(),
                            "width": attachment.width,
                            "height": attachment.height,
                            "bytes": attachment.bytes,
                        }),
                        inspection_text: Some(res.rendered_text),
                        image: Some(PtcImageDescriptor {
                            image_id: attachment.image_id,
                            path,
                            format: format!("{:?}", attachment.format).to_lowercase(),
                            width: attachment.width,
                            height: attachment.height,
                            bytes: attachment.bytes,
                        }),
                    });
                }

                let value = res
                    .structured_value
                    .unwrap_or_else(|| Value::String(res.rendered_text.clone()));
                let inspection_text = if value.is_object() || value.is_array() {
                    Some(res.rendered_text)
                } else {
                    None
                };
                Ok(PtcBindingSuccess {
                    value,
                    inspection_text,
                    image: None,
                })
            }
            Err(error) => {
                let message = format!("nested tool execution failed: {error:#}");
                let status = ToolStatus::Failed(ToolFailureKind::Unknown);
                self.emit_event(EventLog::PtcDispatchEnd {
                    parent_tool_use_id: self.parent_call_id.clone(),
                    sub_call_id: sub_call_id.clone(),
                    tool_name: tool_name.to_string(),
                    status,
                    exit_code: None,
                    duration_ms,
                    summary: truncate_summary(&message, 240),
                    result_kind: crate::tools::metadata::ToolResultKind::Text,
                });
                {
                    let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    guard.sub_calls.push(PtcSubCallDisplay {
                        sub_call_id,
                        tool_name: tool_name.to_string(),
                        status,
                        duration_ms,
                    });
                    guard.failed_sub_calls = guard.failed_sub_calls.saturating_add(1);
                    guard.unknown_effect = true;
                }
                Err(PtcBindingError {
                    tool_name: tool_name.to_string(),
                    kind: ToolFailureKind::Unknown,
                    message,
                })
            }
        }
    }
}

fn truncate_summary(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ToolContext;
    use crate::ptc::{PtcConfig, PtcMode};

    #[tokio::test]
    async fn bridge_blocks_recursive_run_code_and_non_programmatic_tools() {
        let root =
            std::env::temp_dir().join(format!("mink-ptc-bridge-block-{}", std::process::id()));
        let home = root.join("home");
        let cwd = root.join("cwd");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();

        let shared =
            crate::regression::test_context_for_agent_with_config("ptc-bridge-block", |cfg| {
                cfg.ptc = PtcConfig {
                    mode: PtcMode::Exclusive,
                    ..PtcConfig::default()
                };
            })
            .await
            .unwrap();
        let ctx = Arc::new(ToolContext::from_shared(&shared));
        let cancel = ctx.cancel.linked_child_token();
        let bridge = PtcDispatchBridge::new(ctx, "call_ptc", cancel, 10, 4, "10 per min");

        let err = bridge
            .invoke(
                "RunCode",
                serde_json::json!({"code": "1", "description": "x"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind, ToolFailureKind::SafetyBlocked);

        let err2 = bridge
            .invoke("PlanDraft", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err2.kind, ToolFailureKind::SafetyBlocked);

        let err3 = bridge
            .invoke("TodoRead", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err3.kind, ToolFailureKind::SafetyBlocked);

        let err4 = bridge
            .invoke("PlanConfirm", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err4.kind, ToolFailureKind::SafetyBlocked);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn bridge_runs_parallel_reads_serializes_barrier_and_aborts_on_quota() {
        let root =
            std::env::temp_dir().join(format!("mink-ptc-bridge-pool-{}", std::process::id()));
        let home = root.join("home");
        let cwd = root.join("cwd");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("a.txt"), "alpha\n").unwrap();
        std::fs::write(cwd.join("b.txt"), "beta\n").unwrap();

        let events = Arc::new(Mutex::new(Vec::new()));
        let events_sink = events.clone();

        let shared =
            crate::regression::test_context_for_agent_with_config("ptc-bridge-pool", |cfg| {
                cfg.ptc = PtcConfig {
                    mode: PtcMode::Exclusive,
                    max_parallel_sub_calls: 2,
                    ..PtcConfig::default()
                };
            })
            .await
            .unwrap();
        let mut ctx_val = ToolContext::from_shared(&shared);
        ctx_val.cwd = cwd.clone();
        ctx_val.ptc_event_sink = Some(Arc::new(move |evt| {
            events_sink.lock().unwrap().push(evt);
        }));
        let ctx = Arc::new(ctx_val);

        let cancel = ctx.cancel.linked_child_token();
        let bridge = Arc::new(PtcDispatchBridge::new(
            ctx, "parent_1", cancel, 3, 2, "quota 3",
        ));

        // Calls 1 & 2: Read a.txt and b.txt concurrently
        let b1 = bridge.clone();
        let b2 = bridge.clone();
        let (r1, r2) = tokio::join!(
            b1.invoke("Read", serde_json::json!({"path": "a.txt"})),
            b2.invoke("Read", serde_json::json!({"path": "b.txt"})),
        );
        assert!(r1.is_ok(), "read a failed: {r1:?}");
        assert!(r2.is_ok(), "read b failed: {r2:?}");

        // Call 3: Glob (mutating/read pool check)
        let r3 = bridge
            .invoke("Glob", serde_json::json!({"pattern": "*.txt"}))
            .await
            .unwrap();
        assert!(r3.value["files"].is_array());

        // Call 4 exceeds max_sub_calls = 3 -> Aborted & quota message set
        let r4 = bridge
            .invoke("Read", serde_json::json!({"path": "a.txt"}))
            .await
            .unwrap_err();
        assert_eq!(r4.kind, ToolFailureKind::Aborted);
        assert!(bridge.quota_exceeded_message().is_some());

        let recorded = events.lock().unwrap().clone();
        let start_ids: Vec<String> = recorded
            .iter()
            .filter_map(|e| match e {
                EventLog::PtcDispatchStart { sub_call_id, .. } => Some(sub_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            start_ids,
            vec!["parent_1:ptc:1", "parent_1:ptc:2", "parent_1:ptc:3"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn bridge_bypasses_read_memo_and_isolates_direct_read() {
        let shared =
            crate::regression::test_context_for_agent_with_config("ptc-bridge-memo", |cfg| {
                cfg.ptc = PtcConfig {
                    mode: PtcMode::Tool,
                    ..PtcConfig::default()
                };
            })
            .await
            .unwrap();
        let ctx = Arc::new(ToolContext::from_shared(&shared));
        std::fs::write(ctx.cwd.join("memo_ptc.txt"), "hello ptc\nsecond line\n").unwrap();

        let bridge = Arc::new(PtcDispatchBridge::new(
            ctx.clone(),
            "parent_memo",
            ctx.cancel.linked_child_token(),
            10,
            2,
            "10 per min",
        ));

        let first = bridge
            .invoke("Read", serde_json::json!({"path": "memo_ptc.txt"}))
            .await
            .unwrap();
        assert_eq!(first.value["memo_hit"], false);
        assert!(first.value["tag"].as_str().is_some());
        assert_eq!(first.value["content"], "hello ptc\nsecond line\n");

        // Second nested Read must NOT return memo stub: Python relies on content & tag.
        let second = bridge
            .invoke("Read", serde_json::json!({"path": "memo_ptc.txt"}))
            .await
            .unwrap();
        assert_eq!(second.value["memo_hit"], false);
        assert!(second.value["tag"].as_str().is_some());
        assert_eq!(second.value["content"], "hello ptc\nsecond line\n");

        // Nested Read must NOT seed parent's ReadMemo since the model never saw it.
        let runner = crate::tools::runner::ToolRunner::new(ctx.clone());
        let direct = runner
            .execute_all(vec![ToolCallEvent {
                name: "Read".into(),
                id: "direct_1".into(),
                input_json: serde_json::json!({"path": "memo_ptc.txt"}),
                fields: BTreeMap::from([("path".into(), "memo_ptc.txt".into())]),
                parse_error: None,
                raw_arguments_digest: None,
            }])
            .await
            .unwrap();
        assert!(direct[0].content.contains("hello ptc"));
    }

    #[tokio::test]
    async fn bridge_allows_readonly_exploration_only_under_exclusive_readonly_mask() {
        let shared =
            crate::regression::test_context_for_agent_with_config("ptc-bridge-draft", |cfg| {
                cfg.ptc = PtcConfig {
                    mode: PtcMode::Exclusive,
                    ..PtcConfig::default()
                };
            })
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
        std::fs::write(ctx_val.cwd.join("draft_info.txt"), "draft data\n").unwrap();
        let ctx = Arc::new(ctx_val);

        let bridge = PtcDispatchBridge::new(
            ctx.clone(),
            "draft_parent",
            ctx.cancel.linked_child_token(),
            10,
            2,
            "10 per min",
        );

        let ok_read = bridge
            .invoke("Read", serde_json::json!({"path": "draft_info.txt"}))
            .await
            .unwrap();
        assert_eq!(ok_read.value["content"], "draft data\n");

        let err_write = bridge
            .invoke(
                "Write",
                serde_json::json!({"path": "out.txt", "content": "nope"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err_write.kind, ToolFailureKind::SafetyBlocked);

        let err_custom_scheme = bridge
            .invoke("Read", serde_json::json!({"path": "custom://secret"}))
            .await
            .unwrap_err();
        assert_eq!(err_custom_scheme.kind, ToolFailureKind::SafetyBlocked);
    }

    #[tokio::test]
    async fn bash_nonzero_exit_returns_bash_result_and_clamps_to_remaining_deadline() {
        let shared =
            crate::regression::test_context_for_agent_with_config("ptc-bridge-bash", |cfg| {
                cfg.ptc = PtcConfig {
                    mode: PtcMode::Exclusive,
                    ..PtcConfig::default()
                };
            })
            .await
            .unwrap();
        let ctx = Arc::new(ToolContext::from_shared(&shared));
        let bridge = PtcDispatchBridge::new(
            ctx.clone(),
            "bash_parent",
            ctx.cancel.linked_child_token(),
            10,
            2,
            "10 per min",
        )
        .with_deadline(Instant::now() + std::time::Duration::from_secs(6));

        let nonzero = bridge
            .invoke(
                "Bash",
                serde_json::json!({"command": "echo out-err; exit 3", "timeout": 120}),
            )
            .await
            .expect("normal non-zero Bash exit should return BashResult instead of raising");
        assert_eq!(nonzero.value["exit_code"], 3);
        assert_eq!(nonzero.value["timed_out"], false);
        assert!(
            nonzero.value["stdout"]
                .as_str()
                .unwrap_or("")
                .contains("out-err")
        );
        assert_eq!(bridge.failed_sub_calls(), 1);
        assert_eq!(
            bridge.sub_calls()[0].status,
            ToolStatus::Failed(ToolFailureKind::ProcessFailed)
        );

        let tight = PtcDispatchBridge::new(
            ctx.clone(),
            "bash_tight",
            ctx.cancel.linked_child_token(),
            10,
            2,
            "10 per min",
        )
        .with_deadline(Instant::now() + std::time::Duration::from_secs(2));
        let err = tight
            .invoke("Bash", serde_json::json!({"command": "echo hi"}))
            .await
            .unwrap_err();
        assert_eq!(err.kind, ToolFailureKind::Timeout);

        // Also verify clamp_subcall_timeout directly covers Python / PythonSandbox.
        let mut py_json = serde_json::json!({"script": "print(1)", "timeout": 120});
        let mut py_fields = BTreeMap::new();
        bridge
            .clamp_subcall_timeout("Python", &mut py_json, &mut py_fields)
            .unwrap();
        assert!(py_json["timeout"].as_u64().unwrap() < 10);
        let py_tight = tight.clamp_subcall_timeout("PythonSandbox", &mut py_json, &mut py_fields);
        assert_eq!(py_tight.unwrap_err().kind, ToolFailureKind::Timeout);
    }

    #[tokio::test]
    async fn run_cancel_interrupts_inflight_bash_subcall() {
        let shared = crate::regression::test_context_for_agent_with_config(
            "ptc-bridge-cancel-bash",
            |cfg| {
                cfg.ptc = PtcConfig {
                    mode: PtcMode::Exclusive,
                    ..PtcConfig::default()
                };
            },
        )
        .await
        .unwrap();
        let ctx = Arc::new(ToolContext::from_shared(&shared));
        let run_cancel = ctx.cancel.linked_child_token();
        let bridge = Arc::new(PtcDispatchBridge::new(
            ctx,
            "bash_cancel_parent",
            run_cancel.clone(),
            10,
            2,
            "10 per min",
        ));

        let bridge_call = bridge.clone();
        let handle = tokio::spawn(async move {
            bridge_call
                .invoke(
                    "Bash",
                    serde_json::json!({"command": "sleep 30", "timeout": 30}),
                )
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let started = Instant::now();
        run_cancel.cancel();
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("cancelled Bash sub-call must terminate promptly")
            .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let err = res.unwrap_err();
        assert_eq!(err.kind, ToolFailureKind::Aborted);
    }
}
