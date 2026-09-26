use crate::protocol::ToolCallEvent;
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process sequence for compatibility-generated call ids.
static CALL_ID_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Generate a call id that cannot collide with ids already present in the
/// conversation history (used for deterministic legacy `function_call`
/// compatibility, where providers send no id at all).
pub fn unique_call_id(prefix: &str) -> String {
    let sequence = CALL_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    format!("{prefix}_{nanos:x}_{sequence:x}")
}

/// Short hex digest of the raw arguments text used as a storm-comparison key
/// for unparsable arguments.
pub fn raw_arguments_digest(arguments: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(arguments.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn build_tool_call_event(name: &str, id: &str, input: &str) -> Result<ToolCallEvent> {
    let trimmed = if input.trim().is_empty() {
        "{}"
    } else {
        input.trim()
    };
    let obj: Value = serde_json::from_str(trimmed).map_err(|e| anyhow!("parse tool input: {e}"))?;
    let mut event = ToolCallEvent {
        name: name.to_string(),
        id: id.to_string(),
        input_json: obj.clone(),
        fields: BTreeMap::new(),
        parse_error: None,
        raw_arguments_digest: None,
    };
    let map = obj
        .as_object()
        .ok_or_else(|| anyhow!("tool input must be object"))?;
    for (k, v) in map {
        event.fields.insert(k.clone(), json_scalar_string(v));
    }
    Ok(event)
}

/// Build a degraded candidate whose raw arguments could not be parsed. It
/// carries the parse error and the raw-argument digest, so the runner reports
/// a model-format failure and storm keeps distinct bad payloads apart.
pub fn degraded_tool_call(name: &str, id: &str, raw_arguments: &str, error: &str) -> ToolCallEvent {
    ToolCallEvent {
        name: name.to_string(),
        id: id.to_string(),
        input_json: Value::Object(serde_json::Map::new()),
        fields: BTreeMap::new(),
        parse_error: Some(error.to_string()),
        raw_arguments_digest: Some(raw_arguments_digest(raw_arguments)),
    }
}

fn json_scalar_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        _ => v.to_string(),
    }
}

#[cfg(test)]
#[path = "toolcall_tests.rs"]
mod tests;
