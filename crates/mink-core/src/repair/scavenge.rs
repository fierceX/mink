use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

/// A recovered tool call.
#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub name: String,
    /// Raw arguments text. When [`Self::parse_error`] is set this is the exact
    /// payload the model produced (never replaced by a placeholder object).
    pub arguments: String,
    /// Set when the candidate was recognized but its raw arguments could not be
    /// parsed; the round layer turns this into a model-format failure instead
    /// of silently executing a different (empty) call.
    pub parse_error: Option<String>,
}

/// Bounds for regex input — DSML regex can be O(n²) on adversarial input.
const MAX_SCAVENGE_INPUT: usize = 100 * 1024;

// ---- Regex patterns ----

static XML_TOOL_CALL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<tool_call>\s*(.*?)\s*</tool_call>").unwrap());

static BRACKET_TOOL_CALL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\[TOOL_CALL\]\s*(.*?)\s*\[/TOOL_CALL\]").unwrap());

// ---- Main API ----

/// Scavenge tool calls from text content (reasoning or response text).
/// Tries multiple container formats while still routing through the current tool registry/schema.
///
/// A recognized wrapper whose inner JSON does not parse yields a degraded
/// `ToolCallInfo` (empty name + `parse_error`) instead of being dropped: an
/// explicit candidate must reach the correction branch.
pub fn scavenge_tool_calls(text: &str) -> Option<Vec<ToolCallInfo>> {
    if text.len() > MAX_SCAVENGE_INPUT {
        return None;
    }

    let mut results = Vec::new();

    // 1. Try DSML invoke format (DeepSeek-specific markup in reasoning_content).
    // The parser only reports complete invokes (or degraded ones when the
    // markup is truncated / a JSON parameter is invalid): never a panic, never
    // partially parsed arguments presented as complete.
    if let Some(dsml_calls) = scavenge_dsml(text) {
        results.extend(dsml_calls);
        if !results.is_empty() {
            return Some(results);
        }
    }

    // 2. Try wrapper formats. The wrapper tag itself is the candidate marker:
    // the inner text is parsed afterwards, so damaged payloads (truncated
    // braces, wrong types, unusable identity) are surfaced as degraded
    // candidates instead of falling through to the bare-text path.
    for re in [&*XML_TOOL_CALL_RE, &*BRACKET_TOOL_CALL_RE] {
        let caps: Vec<_> = re.captures_iter(text).collect();
        if !caps.is_empty() {
            for cap in &caps {
                let Some(m) = cap.get(1) else { continue };
                match classify_wrapper_payload(m.as_str()) {
                    Ok(call) => results.push(call),
                    Err(degraded) => results.push(degraded),
                }
            }
            if !results.is_empty() {
                return Some(results);
            }
        }
    }

    // 2b. A wrapper whose closing tag never arrived is still an explicit
    // candidate marker: the same closed/unclosed classification applies.
    if results.is_empty()
        && let Some((open, close, start)) = [
            ("<tool_call>", "</tool_call>"),
            ("[TOOL_CALL]", "[/TOOL_CALL]"),
        ]
        .iter()
        .filter_map(|(open, close)| text.find(open).map(|start| (*open, *close, start)))
        .min_by_key(|(_, _, start)| *start)
    {
        let after = &text[start + open.len()..];
        let body = after.split(close).next().unwrap_or(after).trim();
        if !body.is_empty() {
            match classify_wrapper_payload(body) {
                Ok(_usable) => {}
                Err(degraded) => {
                    results.push(degraded);
                    return Some(results);
                }
            }
        }
    }

    // 3. Bare JSON fallback. This is intentionally last because it has the
    // broadest match surface.
    if let Some(call) = scavenge_bare_json(text) {
        results.push(call);
        return Some(results);
    }

    None
}

/// Scavenge from both reasoning_content and content channels.
/// Deduplicates by (name, arguments) signature.
pub fn scavenge_combined(
    reasoning: Option<&str>,
    content: Option<&str>,
    max_calls: usize,
) -> (Vec<ToolCallInfo>, Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    let mut calls = Vec::new();
    let mut notes = Vec::new();

    for (source, text) in [("reasoning", reasoning), ("content", content)] {
        let Some(text) = text else { continue };
        if text.is_empty() {
            continue;
        }

        if let Some(found) = scavenge_tool_calls(text) {
            for info in found {
                let sig = format!("{}::{}", info.name, info.arguments);
                // A degraded candidate has no name but must still reach the
                // correction branch; a nameless candidate without a parse error
                // cannot exist (coerce rejects empty names).
                let usable = !info.name.is_empty() || info.parse_error.is_some();
                if usable && seen.insert(sig) {
                    if calls.len() >= max_calls {
                        notes.push(format!(
                            "{source} reached max recovered calls ({max_calls})"
                        ));
                        break;
                    }
                    calls.push(info);
                }
            }
        }
    }

    (calls, notes)
}

/// Classify one explicit wrapper payload. `Ok(call)` is a usable candidate;
/// `Err(degraded)` is an explicit candidate that cannot be executed (the raw
/// payload is preserved verbatim, never replaced by a placeholder).
fn classify_wrapper_payload(content: &str) -> std::result::Result<ToolCallInfo, ToolCallInfo> {
    match serde_json::from_str::<Value>(content) {
        Ok(v) => match coerce_to_tool_call(&v) {
            Some(call) => Ok(call),
            None => Err(ToolCallInfo {
                name: v
                    .get("name")
                    .or_else(|| v.get("tool_name"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: content.to_string(),
                parse_error: Some("tool call JSON does not contain a usable tool name".to_string()),
            }),
        },
        Err(error) => Err(ToolCallInfo {
            name: String::new(),
            arguments: content.to_string(),
            parse_error: Some(format!("parse tool call JSON: {error}")),
        }),
    }
}

// ---- DSML parsing ----

const DSML_OPEN: &str = "<|DSML|invoke name=\"";
const DSML_PARAM: &str = "<|DSML|parameter name=\"";
const DSML_PARAM_END: &str = "<|DSML|parameter>";
const DSML_CLOSE: &str = "</|DSML|invoke>";

/// A degraded DSML candidate: the invoke marker was recognized but its header,
/// block or a JSON parameter is damaged. The raw payload is preserved and the
/// partially parsed arguments are never presented as a complete call.
fn dsml_degraded(name: &str, raw: &str, error: String) -> ToolCallInfo {
    ToolCallInfo {
        name: name.to_string(),
        arguments: raw.to_string(),
        parse_error: Some(error),
    }
}

/// Parse DSML invoke blocks from text using simple string operations.
/// Format: <|DSML|invoke name="TOOL_NAME">
///           <|DSML|parameter name="KEY" string="true">VALUE<|DSML|parameter>
///         <|DSML|invoke>
fn scavenge_dsml(text: &str) -> Option<Vec<ToolCallInfo>> {
    let mut results = Vec::new();
    let mut pos = 0;

    while pos < text.len() {
        let Some(invoke_start) = text[pos..].find(DSML_OPEN) else {
            break;
        };
        let marker_at = pos + invoke_start;
        let name_begin = marker_at + DSML_OPEN.len();
        let Some(name_end) = text[name_begin..].find('"') else {
            results.push(dsml_degraded(
                "",
                &text[marker_at..],
                "DSML invoke name is not terminated".to_string(),
            ));
            break;
        };
        let name = text[name_begin..name_begin + name_end].to_string();

        // The invoke header ends with `">`; a truncated header must not slice
        // out of bounds.
        let header_end = name_begin + name_end + 1;
        let body_begin = match header_end
            .checked_add(1)
            .filter(|end| *end <= text.len() && text.as_bytes()[*end - 1] == b'>')
        {
            Some(body_begin) => body_begin,
            None => {
                results.push(dsml_degraded(
                    &name,
                    &text[marker_at..],
                    "DSML invoke header is not terminated".to_string(),
                ));
                break;
            }
        };
        let Some(invoke_end) = text[body_begin..].find(DSML_CLOSE) else {
            results.push(dsml_degraded(
                &name,
                &text[body_begin..],
                "DSML invoke block is not terminated".to_string(),
            ));
            break;
        };
        let body = &text[body_begin..body_begin + invoke_end];

        let mut args = serde_json::Map::new();
        let mut damage: Option<String> = None;
        let mut bpos = 0;
        while bpos < body.len() {
            let Some(param_start) = body[bpos..].find(DSML_PARAM) else {
                break;
            };
            let key_begin = bpos + param_start + DSML_PARAM.len();
            let Some(key_end) = body[key_begin..].find('"') else {
                damage = Some("DSML parameter name is not terminated".to_string());
                break;
            };
            let key = body[key_begin..key_begin + key_end].to_string();

            let header_tail = &body[key_begin + key_end + 1..];
            // Consume the complete delimiter, including `>`. A fixed offset
            // after a partial header can exceed the body or split a UTF-8 char.
            let (value_tail, is_json) =
                if let Some(value) = header_tail.strip_prefix(" string=\"true\">") {
                    (value, false)
                } else if let Some(value) = header_tail.strip_prefix(" string=\"false\">") {
                    (value, true)
                } else if let Some(value) = header_tail.strip_prefix('>') {
                    (value, false)
                } else {
                    damage = Some(format!(
                        "DSML parameter `{key}` header is invalid or not terminated"
                    ));
                    break;
                };

            let Some(param_end) = value_tail.find(DSML_PARAM_END) else {
                damage = Some(format!("DSML parameter `{key}` is not terminated"));
                break;
            };
            let raw = value_tail[..param_end].trim().to_string();

            if is_json {
                // A declared-JSON parameter that fails to parse is a model
                // format error: never converted to a string and never silently
                // accepted.
                match serde_json::from_str::<Value>(&raw) {
                    Ok(v) => {
                        args.insert(key, v);
                    }
                    Err(error) => {
                        damage = Some(format!("parse DSML parameter `{key}` as JSON: {error}"));
                        break;
                    }
                }
            } else {
                args.insert(key, Value::String(raw));
            }

            bpos = body.len() - value_tail.len() + param_end + DSML_PARAM_END.len();
        }

        match damage {
            None => results.push(ToolCallInfo {
                name,
                arguments: serde_json::to_string(&args).unwrap_or_default(),
                parse_error: None,
            }),
            Some(error) => results.push(dsml_degraded(&name, &text[marker_at..], error)),
        }
        pos = body_begin + invoke_end + DSML_CLOSE.len();
    }

    if results.is_empty() {
        None
    } else {
        Some(results)
    }
}

// ---- Bare JSON scanning ----

fn scavenge_bare_json(text: &str) -> Option<ToolCallInfo> {
    let start = text.find('{')?;
    let slice = &text[start..];
    let mut depth = 0i32;
    let mut end = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (i, ch) in slice.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '"' => in_string = !in_string,
            '\\' if in_string => escaped = true,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }

    if end == 0 {
        return None;
    }

    let json_str = &slice[..end];
    let v: Value = serde_json::from_str(json_str).ok()?;

    coerce_to_tool_call(&v)
}

/// Try supported JSON shapes for tool call representation. This keeps
/// container recovery compatible while current tool implementations enforce
/// the active argument schema. A declared-but-unparsable arguments payload is
/// preserved as `parse_error`, never replaced with an empty object.
fn coerce_to_tool_call(v: &Value) -> Option<ToolCallInfo> {
    // Shape 1: { "name": "...", "arguments": {...} }
    if let Some(name) = v.get("name").and_then(Value::as_str)
        && !name.is_empty()
    {
        let args = v
            .get("arguments")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        return Some(json_arguments_info(name, args));
    }

    // Shape 2: OpenAI-style { "type": "function", "function": { "name": "...", "arguments": "..." } }
    if v.get("type").and_then(Value::as_str) == Some("function")
        && let Some(func) = v.get("function")
        && let Some(name) = func.get("name").and_then(Value::as_str)
        && !name.is_empty()
    {
        return Some(match func.get("arguments") {
            // Field missing: the provider declared no arguments at all.
            None => json_arguments_info(name, Value::Object(Default::default())),
            Some(Value::String(raw)) => match serde_json::from_str::<Value>(raw) {
                Ok(args) => json_arguments_info(name, args),
                Err(error) => ToolCallInfo {
                    name: name.to_string(),
                    arguments: raw.clone(),
                    parse_error: Some(format!("parse function arguments: {error}")),
                },
            },
            // Field present with the wrong type: a type error, never `{}`.
            Some(other) => ToolCallInfo {
                name: name.to_string(),
                arguments: serde_json::to_string(other).unwrap_or_default(),
                parse_error: Some(format!(
                    "function arguments must be a JSON string (got {})",
                    json_type_name(other)
                )),
            },
        });
    }

    // Shape 3: { "tool_name": "...", "tool_args": {...} } (free-form variant)
    if let Some(name) = v.get("tool_name").and_then(Value::as_str)
        && !name.is_empty()
    {
        let args = v
            .get("tool_args")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        return Some(json_arguments_info(name, args));
    }

    None
}

/// Serialize an arguments value back to raw text for the shared
/// `build_tool_call_event` validation.
fn json_arguments_info(name: &str, args: Value) -> ToolCallInfo {
    ToolCallInfo {
        name: name.to_string(),
        arguments: serde_json::to_string(&args).unwrap_or_default(),
        parse_error: None,
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
#[path = "scavenge_tests.rs"]
mod tests;
