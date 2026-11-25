//! Nested tool dispatch bridge for PTC runs.
//!
//! Enforces programmatic surface membership, rolling bounded concurrency
//! (parallel read-only calls up to `max_parallel`, serialized barriers for
//! mutating/exec calls), total sub-call quota, quiescence drain, event logging,
//! and image/todo/edit state tracking.

use super::runtime::{
    PtcBindingDispatcher, PtcBindingError, PtcBindingSuccess, PtcImageDescriptor,
};
use crate::cancel::CancellationToken;
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
    max_sub_calls: usize,
    sub_call_limit_label: String,
    attempt_seq: AtomicUsize,
    lane_lock: RwLock<()>,
    parallel_slots: Semaphore,
    active_dispatches: Arc<AtomicUsize>,
    drained_notify: Arc<Notify>,
    state: Mutex<BridgeState>,
}

#[derive(Default)]
struct BridgeState {
    sub_calls: Vec<PtcSubCallDisplay>,
    images_by_id: HashMap<String, ImageAttachment>,
    unknown_effect: bool,
    todo_advanced: bool,
    edited_paths: Vec<String>,
    quota_exceeded_message: Option<String>,
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
        Self {
            ctx,
            parent_call_id: parent_call_id.into(),
            run_cancel,
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

    /// Abort any queued work and wait until every started nested dispatch has settled.
    pub async fn drain(&self) {
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
            return custom.definition.execution
                == crate::tools::extension::ToolExecutionMode::Sequential
                || (self
                    .ctx
                    .goal_store
                    .snapshot()
                    .is_some_and(|g| g.phase == crate::session::goal::GoalPhase::Executing));
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
        if self.ctx.goal_drafting
            || self.ctx.goal_review.is_some()
            || self.ctx.goal_adjudication.is_some()
        {
            return Err(PtcBindingError {
                tool_name: tool_name.to_string(),
                kind: ToolFailureKind::SafetyBlocked,
                message: "RunCode is unavailable during goal drafting, review, or adjudication"
                    .to_string(),
            });
        }

        let input_json = match args {
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
        let fields = input_json
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
            .invoke("GoalDraft", serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(err2.kind, ToolFailureKind::SafetyBlocked);
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

        // Call 3: TodoRead
        let r3 = bridge
            .invoke("TodoRead", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(r3.value["revision"], 0);

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
}
