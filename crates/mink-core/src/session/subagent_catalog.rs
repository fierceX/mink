//! Derived view of the sub-agent delegation catalog recorded in `events.jsonl`.
//!
//! The event log is diagnostic-grade (best effort), so this catalog is a
//! discovery/UI view, never an authority. `conversation.jsonl` remains the
//! authoritative record of a delegation's tool result.

use anyhow::Result;
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubAgentCatalogEntry {
    pub session_id: String,
    pub label: Option<String>,
    pub mode: Option<String>,
    pub status: String,
    pub stop_reason: Option<String>,
    pub parent_session_id: Option<String>,
    pub depth: Option<u32>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub duration_ms: Option<u64>,
    pub updated_seq: u64,
}

/// Read the parent session catalog, tolerating a missing/partial event log.
pub fn read_catalog(session_dir: &Path) -> Result<Vec<SubAgentCatalogEntry>> {
    let path = session_dir.join("events.jsonl");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(fold_events(text.lines().enumerate().filter_map(
        |(index, line)| {
            serde_json::from_str::<Value>(line)
                .ok()
                .map(|value| (index as u64, value))
        },
    )))
}

/// Fold `(sequence, event)` pairs into one entry per child id.
///
/// The first `launched` record owns identity fields; later terminal records
/// update status, tokens, stop reason and duration. Unknown statuses and
/// versions are preserved instead of rejected.
pub fn fold_events(events: impl IntoIterator<Item = (u64, Value)>) -> Vec<SubAgentCatalogEntry> {
    let mut out: Vec<SubAgentCatalogEntry> = Vec::new();
    let mut index: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (seq, event) in events {
        if event.get("type").and_then(Value::as_str) != Some("sub_agent") {
            continue;
        }
        let Some(session_id) = event.get("session_id").and_then(Value::as_str) else {
            continue;
        };
        let status = event
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let entry = match index.get(session_id).copied() {
            Some(position) => &mut out[position],
            None => {
                index.insert(session_id.to_string(), out.len());
                out.push(SubAgentCatalogEntry {
                    session_id: session_id.to_string(),
                    status: status.clone(),
                    ..Default::default()
                });
                out.last_mut().expect("just pushed")
            }
        };
        if entry.status == "launched" || entry.label.is_none() {
            if let Some(label) = event.get("label").and_then(Value::as_str) {
                entry.label = Some(label.to_string());
            }
            if let Some(mode) = event.get("mode").and_then(Value::as_str) {
                entry.mode = Some(mode.to_string());
            }
            if let Some(parent) = event.get("parent_session_id").and_then(Value::as_str) {
                entry.parent_session_id = Some(parent.to_string());
            }
            if let Some(depth) = event.get("depth").and_then(Value::as_u64) {
                entry.depth = u32::try_from(depth).ok();
            }
        }
        entry.status = status;
        if let Some(reason) = event.get("stop_reason").and_then(Value::as_str) {
            entry.stop_reason = Some(reason.to_string());
        }
        if let Some(tokens) = event.get("input_tokens").and_then(Value::as_u64) {
            entry.input_tokens = Some(tokens);
        }
        if let Some(tokens) = event.get("output_tokens").and_then(Value::as_u64) {
            entry.output_tokens = Some(tokens);
        }
        if let Some(duration) = event.get("duration_ms").and_then(Value::as_u64) {
            entry.duration_ms = Some(duration);
        }
        entry.updated_seq = seq;
    }
    out
}

/// Sanitize a model-authored description into a durable one-line label.
pub fn sanitize_label(raw: &str, fallback: &str) -> String {
    let collapsed = raw
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    let one_line = collapsed.split_whitespace().collect::<Vec<_>>().join(" ");
    let label = if one_line.is_empty() {
        fallback.to_string()
    } else {
        one_line
    };
    let mut out = String::new();
    for ch in label.chars() {
        if out.chars().count() >= 80 {
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fold_tolerates_legacy_and_unknown_events() {
        let events = vec![
            (
                0,
                json!({"type":"sub_agent","session_id":"sub_1","status":"launched"}),
            ),
            (
                1,
                json!({"type":"sub_agent","session_id":"sub_2","status":"launched","label":"x","mode":"fork","depth":1}),
            ),
            (
                2,
                json!({"type":"sub_agent","session_id":"sub_1","status":"weird","stop_reason":"error","input_tokens":1,"output_tokens":2,"duration_ms":3}),
            ),
            (3, json!({"type":"text","content":"ignored"})),
            (4, json!({"type":"sub_agent","status":"launched"})),
        ];
        let catalog = fold_events(events);
        assert_eq!(catalog.len(), 2);
        let first = catalog
            .iter()
            .find(|entry| entry.session_id == "sub_1")
            .unwrap();
        assert_eq!(first.status, "weird");
        assert_eq!(first.stop_reason.as_deref(), Some("error"));
        assert_eq!(first.input_tokens, Some(1));
        assert_eq!(first.duration_ms, Some(3));
        let second = catalog
            .iter()
            .find(|entry| entry.session_id == "sub_2")
            .unwrap();
        assert_eq!(second.label.as_deref(), Some("x"));
        assert_eq!(second.mode.as_deref(), Some("fork"));
        assert_eq!(second.depth, Some(1));
        assert_eq!(second.status, "launched");
    }

    #[test]
    fn label_is_single_line_and_bounded() {
        assert_eq!(
            sanitize_label("  review\n  diff \t now ", "fallback"),
            "review diff now"
        );
        assert_eq!(sanitize_label("\u{0}\u{1}", "fallback"), "fallback");
        assert_eq!(
            sanitize_label(&"x".repeat(200), "fallback").chars().count(),
            80
        );
    }
}
