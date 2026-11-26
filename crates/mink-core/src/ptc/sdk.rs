//! Deterministic Python `.pyi` SDK and prompt section renderer for PTC mode.

use super::PtcMode;
use crate::tools::surface::ModelTool;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Return true if `name` is a valid Python identifier that is not a keyword.
pub fn is_valid_python_ident(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    !PYTHON_KEYWORDS.contains(&name)
}

/// Map a JSON Schema node to a concise Python type annotation (bounded depth).
pub fn json_schema_to_python(schema: &Value) -> String {
    json_schema_to_python_depth(schema, 0)
}

fn json_schema_to_python_depth(schema: &Value, depth: usize) -> String {
    if depth >= 16 {
        return "Any".to_string();
    }
    let Some(obj) = schema.as_object() else {
        return "Any".to_string();
    };
    if let Some(const_val) = obj.get("const") {
        return render_literal(const_val);
    }
    if let Some(enum_vals) = obj.get("enum").and_then(Value::as_array) {
        if enum_vals.is_empty() {
            return "Any".to_string();
        }
        let items: Vec<String> = enum_vals.iter().map(render_literal).collect();
        return format!("Literal[{}]", items.join(", "));
    }
    for union_key in ["anyOf", "oneOf"] {
        if let Some(variants) = obj.get(union_key).and_then(Value::as_array) {
            if variants.is_empty() {
                return "Any".to_string();
            }
            let parts: Vec<String> = variants
                .iter()
                .map(|v| json_schema_to_python_depth(v, depth + 1))
                .collect();
            return parts.join(" | ");
        }
    }
    match obj.get("type").and_then(Value::as_str) {
        Some("string") => "str".to_string(),
        Some("integer") => "int".to_string(),
        Some("number") => "float".to_string(),
        Some("boolean") => "bool".to_string(),
        Some("null") => "None".to_string(),
        Some("array") => {
            let item_ty = obj
                .get("items")
                .map(|items| json_schema_to_python_depth(items, depth + 1))
                .unwrap_or_else(|| "Any".to_string());
            format!("list[{item_ty}]")
        }
        Some("object") => "dict[str, Any]".to_string(),
        _ => "Any".to_string(),
    }
}

fn render_literal(val: &Value) -> String {
    match val {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()),
        _ => "Any".to_string(),
    }
}

fn collapse_doc(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("\"\"\"", "\\\"\\\"\\\"")
}

fn return_type_for_tool(name: &str, image_capable: bool) -> &'static str {
    match name {
        "Read" if image_capable => "ReadResult | Image",
        "Read" => "ReadResult",
        "Glob" => "GlobResult",
        "Grep" => "GrepResult",
        "Write" => "WriteResult",
        "Edit" => "EditResult",
        "Bash" => "BashResult",
        _ => "dict[str, Any] | list[Any] | str",
    }
}

fn to_pascal_ident(name: &str) -> String {
    let mut out = String::new();
    for part in name.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
        }
    }
    out
}

fn collect_param_item_typed_dicts(
    sorted: &[&ModelTool],
    out: &mut String,
) -> BTreeMap<(String, String), String> {
    let mut mapping = BTreeMap::new();
    for tool in sorted {
        let tool_name = tool.metadata.name.as_ref();
        if !is_valid_python_ident(tool_name) {
            continue;
        }
        let Some(props) = tool.schema["input_schema"]
            .get("properties")
            .and_then(Value::as_object)
        else {
            continue;
        };
        let mut param_keys: Vec<&String> = props.keys().collect();
        param_keys.sort();
        for key in param_keys {
            let prop = &props[key];
            if !is_valid_python_ident(key)
                || prop
                    .get("description")
                    .and_then(Value::as_str)
                    .is_some_and(|d| d.contains("Accepted for compatibility and ignored"))
            {
                continue;
            }
            if prop.get("type").and_then(Value::as_str) != Some("array") {
                continue;
            }
            let Some(items) = prop.get("items").and_then(Value::as_object) else {
                continue;
            };
            if items.get("type").and_then(Value::as_str) != Some("object") {
                continue;
            }
            let Some(item_props) = items.get("properties").and_then(Value::as_object) else {
                continue;
            };
            let required: BTreeSet<&str> = items
                .get("required")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let mut fields = Vec::new();
            let mut item_keys: Vec<&String> = item_props.keys().collect();
            item_keys.sort();
            for field_key in item_keys {
                if !is_valid_python_ident(field_key) {
                    continue;
                }
                let field_schema = &item_props[field_key];
                if field_schema
                    .get("description")
                    .and_then(Value::as_str)
                    .is_some_and(|d| d.contains("Accepted for compatibility and ignored"))
                {
                    continue;
                }
                let field_ty = json_schema_to_python(field_schema);
                fields.push((
                    field_key.as_str(),
                    field_ty,
                    required.contains(field_key.as_str()),
                ));
            }
            if fields.is_empty() {
                continue;
            }
            let class_name = format!("{}{}Item", to_pascal_ident(tool_name), to_pascal_ident(key));
            let all_required = fields.iter().all(|(_, _, req)| *req);
            if all_required {
                let _ = writeln!(out, "class {class_name}(TypedDict):");
            } else {
                let _ = writeln!(out, "class {class_name}(TypedDict, total=False):");
            }
            for (field_key, field_ty, is_req) in fields {
                if !all_required && is_req {
                    let _ = writeln!(out, "    {field_key}: {field_ty}  # required");
                } else {
                    let _ = writeln!(out, "    {field_key}: {field_ty}");
                }
            }
            out.push('\n');
            mapping.insert(
                (tool_name.to_string(), key.to_string()),
                format!("list[{class_name}]"),
            );
        }
    }
    mapping
}

fn append_active_result_typed_dicts(active_names: &BTreeSet<&str>, out: &mut String) {
    if active_names.contains("Read") {
        out.push_str("class ReadResult(TypedDict, total=False):\n    path: str\n    tag: str | None\n    start_line: int\n    end_line: int\n    total_lines: int\n    content: str\n    raw: bool\n    memo_hit: bool\n    truncated: bool\n\n");
    }
    if active_names.contains("Glob") {
        out.push_str("class GlobResult(TypedDict):\n    files: list[str]\n    truncated: bool\n\n");
    }
    if active_names.contains("Grep") {
        out.push_str("class GrepMatch(TypedDict, total=False):\n    path: str\n    tag: str | None\n    line: int\n    text: str\n    is_context: bool\n\n");
        out.push_str(
            "class GrepResult(TypedDict):\n    matches: list[GrepMatch]\n    truncated: bool\n\n",
        );
    }
    if active_names.contains("Write") {
        out.push_str("class WriteResult(TypedDict, total=False):\n    path: str\n    bytes_written: int\n    tag: str | None\n\n");
    }
    if active_names.contains("Edit") {
        out.push_str(
            "class EditFileResult(TypedDict, total=False):\n    path: str\n    tag: str | None\n\n",
        );
        out.push_str("class EditResult(TypedDict):\n    applied: bool\n    files: list[EditFileResult]\n    summary: str\n\n");
    }
    if active_names.contains("Bash") {
        out.push_str("class BashResult(TypedDict):\n    exit_code: int | None\n    stdout: str\n    stderr: str\n    timed_out: bool\n\n");
    }
}

/// Render the `<ptc-mode>` directive section for `PtcMode::Exclusive`.
pub fn render_ptc_mode_section(wire_tools: &[ModelTool]) -> String {
    let direct_names: Vec<&str> = wire_tools
        .iter()
        .map(|tool| tool.metadata.name.as_ref())
        .collect();
    format!(
        "`RunCode` is the primary programmatic execution tool. Direct wire tools ({}) — including planning, task-tracking (`Todo*`), goal, and delegation tools — MUST be called directly; calling any SDK-only tool directly is rejected. Invoke tools declared in `<ptc-sdk>` inside `RunCode` via `await tools.<Name>(...)`. Batch exploratory reads/greps into one `RunCode` (`await gather(...)`) rather than wrapping single trivial calls.",
        direct_names.join(", ")
    )
}

/// Render the deterministic `<ptc-sdk>` section for the given programmatic tools.
///
/// Tools are emitted in lexicographic order so unchanged tool surfaces yield
/// byte-identical system prompts across assemblies (preserving provider KV-cache).
pub fn render_python_sdk(
    programmatic_tools: &[ModelTool],
    wire_tool_names: &BTreeSet<&str>,
    mode: PtcMode,
    image_capable: bool,
    default_timeout_secs: u64,
    max_timeout_secs: u64,
) -> String {
    let mut sorted: Vec<&ModelTool> = programmatic_tools.iter().collect();
    sorted.sort_by(|a, b| a.metadata.name.as_ref().cmp(b.metadata.name.as_ref()));

    let mut out = String::new();
    out.push_str("## Writing Python code for RunCode\n\n");
    out.push_str(&format!(
        "`RunCode` takes required `code` (Python 3 supporting top-level `await`) and `description` (5-10 words), plus optional `timeout` (seconds; default {default_timeout_secs}s, max {max_timeout_secs}s).\n"
    ));
    if mode == PtcMode::Tool {
        out.push_str("Routing (Hybrid mode): prefer direct tools (or native parallel multi-tool calls in one turn) for single or simple independent steps. Use `RunCode` for loops, conditional branching, data-dependent chains, filtering/aggregation, or keeping large intermediate outputs out of conversation history. For tools already declared in the direct tool schema, parameter signatures are not repeated below.\n");
    }
    out.push_str("\nInside `RunCode`:\n");
    out.push_str("- `tools`, `inspect`, `gather`, `ToolCallError`, and `Image` are pre-injected globals (no import needed; `from tools import ...` also works).\n");
    out.push_str("- Call tools with `await tools.Name(arg=value, ...)` (or `await tools[\"name\"]({...})` for exotic names).\n");
    out.push_str("- Overlap read-only calls with `await gather(...)`; mutating/exec calls serialize automatically.\n");
    out.push_str("- `tools.Bash(...)` returns `BashResult` on normal non-zero exit (`res[\"exit_code\"]`, `res[\"stdout\"]`); `SafetyBlocked`, `Timeout`, `ArgumentInvalid`, `Aborted`, and other tool errors raise `ToolCallError` (`.tool_name`, `.kind`, `.message`) — use `try/except ToolCallError` or `gather(..., return_exceptions=True)` to isolate failures.\n");
    out.push_str("- Use `inspect(*values)` to surface selected outputs; uninspected intermediate data stays out of conversation.\n");
    out.push_str("- Long-running jobs (>5-10 min): launch detached via `await tools.Bash(command=\"nohup <cmd> > run.log 2>&1 < /dev/null &\")` and inspect logs across turns instead of blocking foreground `RunCode`.\n");
    if image_capable {
        out.push_str("- `await tools.Read(path=\"image.png\")` returns an `Image` handle; pass it to `inspect(img)` to attach it to the next model request.\n");
    }
    out.push_str("\nPython SDK bindings:\n\n```python\n");
    out.push_str("from typing import Any, Awaitable, Literal, TypedDict\n\n");
    out.push_str(
        "class ToolCallError(Exception):\n    tool_name: str\n    kind: str\n    message: str\n\n",
    );
    out.push_str("class Image:\n    image_id: str\n    path: str\n    format: str\n    width: int\n    height: int\n    bytes: int\n\n");
    out.push_str("def inspect(*values: object) -> None: ...\n");
    out.push_str("async def gather(*aws: Awaitable[Any], return_exceptions: bool = ...) -> list[Any]: ...\n\n");
    let active_names: BTreeSet<&str> = sorted
        .iter()
        .map(|tool| tool.metadata.name.as_ref())
        .collect();
    append_active_result_typed_dicts(&active_names, &mut out);
    let item_dict_tools: Vec<&ModelTool> = if mode == PtcMode::Tool {
        sorted
            .iter()
            .copied()
            .filter(|tool| !wire_tool_names.contains(tool.metadata.name.as_ref()))
            .collect()
    } else {
        sorted.clone()
    };
    let item_param_types = collect_param_item_typed_dicts(&item_dict_tools, &mut out);
    out.push_str("class tools:\n");

    let mut exotic_tools = Vec::new();
    for tool in &sorted {
        let name = tool.metadata.name.as_ref();
        let ret = return_type_for_tool(name, image_capable);
        let dedup_wire = mode == PtcMode::Tool && wire_tool_names.contains(name);
        if dedup_wire {
            if is_valid_python_ident(name) {
                out.push_str("    @staticmethod\n");
                let _ = writeln!(
                    out,
                    "    async def {name}(**kwargs: Any) -> {ret}: ...  # args: see {name} schema"
                );
            } else {
                exotic_tools.push((
                    name,
                    "**kwargs: Any".to_string(),
                    ret,
                    format!("args: see {name} schema"),
                ));
            }
            continue;
        }
        let doc = tool
            .schema
            .get("description")
            .and_then(Value::as_str)
            .map(collapse_doc)
            .unwrap_or_default();
        let params = render_python_params(name, &tool.schema["input_schema"], &item_param_types);
        if is_valid_python_ident(name) {
            out.push_str("    @staticmethod\n");
            let _ = writeln!(out, "    async def {name}({params}) -> {ret}:");
            if !doc.is_empty() {
                let _ = writeln!(out, "        \"\"\"{doc}\"\"\"");
            }
            out.push_str("        ...\n");
        } else {
            exotic_tools.push((name, params, ret, doc));
        }
    }
    if !exotic_tools.is_empty() {
        out.push_str("    # Non-identifier tool names (call via await tools[\"name\"]({...})):\n");
        for (name, params, ret, doc) in exotic_tools {
            let _ = writeln!(out, "    # tools[{name:?}]({params}) -> {ret}  # {doc}");
        }
    }
    out.push_str("```");
    out
}

fn render_python_params(
    tool_name: &str,
    input_schema: &Value,
    item_param_types: &BTreeMap<(String, String), String>,
) -> String {
    let Some(obj) = input_schema.as_object() else {
        return "**kwargs: Any".to_string();
    };
    let Some(props) = obj.get("properties").and_then(Value::as_object) else {
        return "**kwargs: Any".to_string();
    };
    if props.is_empty() {
        return String::new();
    }
    let required: BTreeSet<&str> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    // Required parameters first (sorted), then optional parameters (sorted), ignoring
    // compatibility-only aliases whose description explicitly says "Accepted for compatibility and ignored".
    let mut req_parts = Vec::new();
    let mut opt_parts = Vec::new();
    let mut has_non_ident_key = false;
    let mut keys: Vec<&String> = props.keys().collect();
    keys.sort();
    for key in keys {
        let prop = &props[key];
        let is_compat_ignored = prop
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|d| d.contains("Accepted for compatibility and ignored"));
        if is_compat_ignored {
            continue;
        }
        if !is_valid_python_ident(key) {
            has_non_ident_key = true;
            continue;
        }
        let ty = item_param_types
            .get(&(tool_name.to_string(), key.to_string()))
            .cloned()
            .unwrap_or_else(|| json_schema_to_python(prop));
        if required.contains(key.as_str()) {
            req_parts.push(format!("{key}: {ty}"));
        } else {
            opt_parts.push(format!("{key}: {ty} = ..."));
        }
    }
    if req_parts.is_empty() && opt_parts.is_empty() && !has_non_ident_key {
        return String::new();
    }
    let mut all = vec!["*".to_string()];
    all.extend(req_parts);
    all.extend(opt_parts);
    if has_non_ident_key {
        all.push("**kwargs: Any".to_string());
    }
    all.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::metadata::{ApprovalTier, ToolMetadata, ToolResultKind};

    fn model_tool(name: &str, schema: Value) -> ModelTool {
        ModelTool {
            metadata: ToolMetadata::new("dummy", ApprovalTier::Read, ToolResultKind::Text)
                .with_name_for_test(name),
            schema,
        }
    }

    trait MetadataTestExt {
        fn with_name_for_test(self, name: &str) -> Self;
    }

    impl MetadataTestExt for ToolMetadata {
        fn with_name_for_test(mut self, name: &str) -> Self {
            self.name = std::borrow::Cow::Owned(name.to_string());
            self
        }
    }

    #[test]
    fn sdk_render_is_deterministic_and_lexicographically_sorted() {
        let b = model_tool(
            "Zeta",
            serde_json::json!({
                "name": "Zeta",
                "description": "Last tool.",
                "input_schema": {
                    "type": "object",
                    "properties": {"x": {"type": "integer"}},
                    "required": ["x"],
                    "additionalProperties": false
                }
            }),
        );
        let a = model_tool(
            "Alpha",
            serde_json::json!({
                "name": "Alpha",
                "description": "First tool.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "mode": {"type": "string", "enum": ["a", "b"]},
                        "limit": {"type": "integer"}
                    },
                    "required": ["mode"],
                    "additionalProperties": false
                }
            }),
        );
        let first = render_python_sdk(
            &[b.clone(), a.clone()],
            &BTreeSet::new(),
            PtcMode::Exclusive,
            false,
            120,
            3600,
        );
        let second = render_python_sdk(
            &[a, b],
            &BTreeSet::new(),
            PtcMode::Exclusive,
            false,
            120,
            3600,
        );
        assert_eq!(first, second);
        let alpha_pos = first.find("async def Alpha(").unwrap();
        let zeta_pos = first.find("async def Zeta(").unwrap();
        assert!(alpha_pos < zeta_pos);
        assert!(first.contains("mode: Literal[\"a\", \"b\"]"));
        assert!(first.contains("limit: int = ..."));
        assert!(first.contains("default 120s, max 3600s"));
    }

    #[test]
    fn exotic_tool_names_are_documented_without_breaking_python_syntax() {
        let exotic = model_tool(
            "my-tool",
            serde_json::json!({
                "name": "my-tool",
                "description": "Hyphenated custom tool.",
                "input_schema": {
                    "type": "object",
                    "properties": {"k": {"type": "string"}},
                    "additionalProperties": false
                }
            }),
        );
        let rendered =
            render_python_sdk(&[exotic], &BTreeSet::new(), PtcMode::Tool, false, 120, 3600);
        assert!(rendered.contains("tools[\"my-tool\"]"));
        assert!(!rendered.contains("async def my-tool("));
        assert!(!rendered.contains("class ReadResult"));
        assert!(!rendered.contains("class BashResult"));
    }

    #[test]
    fn sdk_filters_inactive_typed_dicts_and_deduplicates_wire_tools_in_hybrid_mode() {
        let wire_read = model_tool(
            "Read",
            serde_json::json!({
                "name": "Read",
                "description": "Long wire read description that must not be repeated.",
                "input_schema": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }),
        );
        let sdk_only = model_tool(
            "CustomBatch",
            serde_json::json!({
                "name": "CustomBatch",
                "description": "Custom SDK-only batch tool.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "items": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "target": {"type": "string"}
                                },
                                "required": ["target"]
                            }
                        }
                    },
                    "required": ["items"]
                }
            }),
        );
        let wire_names = BTreeSet::from(["Read"]);
        let rendered = render_python_sdk(
            &[wire_read, sdk_only],
            &wire_names,
            PtcMode::Tool,
            false,
            120,
            3600,
        );
        assert!(rendered.contains("class ReadResult(TypedDict, total=False):"));
        assert!(!rendered.contains("class BashResult"));
        assert!(
            rendered.contains(
                "async def Read(**kwargs: Any) -> ReadResult: ...  # args: see Read schema"
            )
        );
        assert!(!rendered.contains("Long wire read description"));
        assert!(rendered.contains("class CustomBatchItemsItem(TypedDict):"));
        assert!(rendered.contains("items: list[CustomBatchItemsItem]"));
        assert!(rendered.contains("Custom SDK-only batch tool."));
    }
}
