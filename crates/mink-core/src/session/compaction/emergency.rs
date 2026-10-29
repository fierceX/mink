use super::{head_tail_with_marker, is_real_user_message};
use serde_json::Value;

/// Request excerpt budgets and per-block limits are local to the lossy fallback.
pub(crate) const EMERGENCY_REQUEST_BUDGET_BYTES: usize = 3_600;
const EMERGENCY_REQUEST_MIN_BYTES: usize = 160;
const EMERGENCY_BLOCK_CHARS: usize = 1_200;

/// Ordered source material for the deterministic emergency checkpoint.
#[derive(Default)]
pub(super) struct EmergencyBlocks {
    marker: String,
    /// Re-render from the original request; never truncate an already omitted excerpt.
    request_source: Option<String>,
    request_budget: usize,
    request: Option<String>,
    previous: Option<String>,
    repair_loss: Option<String>,
    recent: Option<String>,
    artifacts: Option<String>,
}

impl EmergencyBlocks {
    pub(super) fn render(&self) -> String {
        let mut out = self.marker.clone();
        for (title, block) in [
            ("latest user request", &self.request),
            ("previous checkpoint (may be partial)", &self.previous),
            (
                "startup boundary repair (messages no longer in the active window)",
                &self.repair_loss,
            ),
            ("recent tool evidence", &self.recent),
            ("available artifacts", &self.artifacts),
        ] {
            if let Some(block) = block.as_deref().filter(|b| !b.trim().is_empty()) {
                out.push_str("\n\n[");
                out.push_str(title);
                out.push_str("]\n");
                out.push_str(block);
            }
        }
        out
    }

    /// Drop low-priority blocks first, then halve the request excerpt.
    /// `false` means the fixed marker and minimum request excerpt remain.
    pub(super) fn shrink(&mut self) -> bool {
        if self.artifacts.take().is_some() {
            return true;
        }
        if self.recent.take().is_some() {
            return true;
        }
        if self.previous.take().is_some() {
            return true;
        }
        if self.repair_loss.take().is_some() {
            return true;
        }
        let Some(source) = self.request_source.as_deref() else {
            return false;
        };
        let current_len = self.request.as_deref().map_or(0, str::len);
        let mut next_budget = self.request_budget / 2;
        while next_budget >= EMERGENCY_REQUEST_MIN_BYTES {
            let next = head_tail_with_marker(source, next_budget, "[…", " bytes omitted…]");
            if next.len() < current_len {
                self.request_budget = next_budget;
                self.request = Some(next);
                return true;
            }
            next_budget /= 2;
        }
        false
    }
}

/// Collapse whitespace and bound event-log fields to one line.
pub(super) fn sanitize_field(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join("_");
    truncate_chars(&collapsed, max_chars)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let head = max_chars.saturating_sub(16).max(1);
    let mut out: String = text.chars().take(head).collect();
    out.push_str(&format!("[…{} chars omitted…]", count - head));
    out
}

pub(crate) fn last_real_user_text(active: &[Value]) -> Option<String> {
    active
        .iter()
        .rev()
        .find(|message| is_real_user_message(message))
        .and_then(|message| message.get("content").and_then(Value::as_str))
        .map(str::to_string)
}

fn recent_evidence(active: &[Value]) -> Option<String> {
    const MAX_EXCHANGES: usize = 6;
    let mut lines: Vec<String> = Vec::new();
    let mut calls = 0usize;
    for message in active.iter().rev() {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                    let id = block.get("id").and_then(Value::as_str).unwrap_or("?");
                    let input = block
                        .get("input")
                        .map(|value| value.to_string())
                        .unwrap_or_default();
                    lines.push(format!(
                        "- call {name} ({id}): {}",
                        truncate_chars(&input, 200)
                    ));
                    calls += 1;
                    if calls >= MAX_EXCHANGES {
                        break;
                    }
                }
                Some("tool_result") => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    let text = block
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    lines.push(format!("- result {id}: {}", truncate_chars(text, 240)));
                }
                _ => {}
            }
        }
        if calls >= MAX_EXCHANGES {
            break;
        }
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

fn repair_loss_text(messages: &[Value]) -> Option<String> {
    const MAX_MESSAGES: usize = 8;
    if messages.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    for message in messages.iter().take(MAX_MESSAGES) {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let content = message
            .get("content")
            .map(|content| {
                content
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| content.to_string())
            })
            .unwrap_or_default();
        lines.push(format!("- {role}: {}", truncate_chars(&content, 240)));
    }
    if messages.len() > MAX_MESSAGES {
        lines.push(format!(
            "[…] {} earlier messages omitted…",
            messages.len() - MAX_MESSAGES
        ));
    }
    Some(lines.join("\n"))
}

/// Build bounded, prioritized material for a lossy emergency checkpoint.
pub(super) fn emergency_blocks(
    reason: &str,
    active: &[Value],
    previous_summary: &str,
    current_user_input: Option<&str>,
    artifact_refs: &[String],
    repair_loss: &[Value],
) -> EmergencyBlocks {
    let request_source = current_user_input
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
        .or_else(|| last_real_user_text(active))
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    EmergencyBlocks {
        marker: format!(
            "[emergency-context-excerpt] This runtime-generated checkpoint replaces the earlier span of this conversation without a model summary (reason: {}). The full history is still stored in the session records; this is a lossy excerpt, not a completion signal, and details may be omitted.",
            sanitize_field(reason, 96)
        ),
        request: request_source.as_deref().map(|text| {
            head_tail_with_marker(
                text,
                EMERGENCY_REQUEST_BUDGET_BYTES,
                "[…",
                " bytes omitted…",
            )
        }),
        request_source,
        request_budget: EMERGENCY_REQUEST_BUDGET_BYTES,
        previous: Some(previous_summary.trim())
            .filter(|summary| !summary.is_empty())
            .map(|summary| truncate_chars(summary, EMERGENCY_BLOCK_CHARS / 2)),
        repair_loss: repair_loss_text(repair_loss),
        recent: recent_evidence(active),
        artifacts: (!artifact_refs.is_empty()).then(|| {
            artifact_refs
                .iter()
                .map(|id| format!("- artifact://{id}"))
                .collect::<Vec<_>>()
                .join("\n")
        }),
    }
}
