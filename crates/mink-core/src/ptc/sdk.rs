//! Deterministic Python `.pyi` SDK and prompt section renderer for PTC mode.

use super::PtcMode;
use crate::tools::surface::ModelTool;
use serde_json::Value;
use std::collections::BTreeSet;
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
        "TodoRead" | "TodoWrite" | "TodoAdvance" => "TodoResult",
        _ => "dict[str, Any] | list[Any] | str",
    }
}

/// Render the `<ptc-mode>` directive section for `PtcMode::Exclusive`.
pub fn render_ptc_mode_section(wire_tools: &[ModelTool]) -> String {
    let direct_names: Vec<&str> = wire_tools
        .iter()
        .map(|tool| tool.metadata.name.as_ref())
        .collect();
    format!(
        "`RunCode` is the primary programmatic execution tool. Only direct wire tools ({}) may be called directly; calling any SDK-only tool directly is rejected. Invoke every tool declared in `<ptc-sdk>` from inside `RunCode` using `await tools.<Name>(...)`.",
        direct_names.join(", ")
    )
}

/// Render the deterministic `<ptc-sdk>` section for the given programmatic tools.
///
/// Tools are emitted in lexicographic order so unchanged tool surfaces yield
/// byte-identical system prompts across assemblies (preserving provider KV-cache).
pub fn render_python_sdk(
    programmatic_tools: &[ModelTool],
    mode: PtcMode,
    image_capable: bool,
) -> String {
    let mut sorted: Vec<&ModelTool> = programmatic_tools.iter().collect();
    sorted.sort_by(|a, b| a.metadata.name.as_ref().cmp(b.metadata.name.as_ref()));

    let mut out = String::new();
    out.push_str("## Writing Python code for RunCode\n\n");
    out.push_str("`RunCode` takes two required arguments: `code` (a Python 3 program supporting top-level `await`) and `description` (a concise 5-10 word summary), plus optional `timeout` (seconds).\n");
    if mode == PtcMode::Tool {
        out.push_str("Use direct tools for simple one-off calls, or `RunCode` to compose multi-step loops, filtering, and concurrent fan-out.\n");
    }
    out.push_str("\nInside `RunCode`:\n");
    out.push_str("- Call tools with `await tools.Name(arg=value, ...)` (or `await tools[\"name\"]({...})` for exotic names).\n");
    out.push_str("- Concurrent read-only calls MAY overlap with `await gather(tools.Read(path=\"a\"), tools.Read(path=\"b\"))`; mutating calls serialize automatically.\n");
    out.push_str("- Failed tool calls raise `ToolCallError` (`.tool_name`, `.kind`, `.message`); catch it with `try/except ToolCallError` to recover.\n");
    out.push_str("- Show selected values and `Image` handles with `inspect(*values)`; uninspected intermediate results stay out of the conversation.\n");
    if image_capable {
        out.push_str("- `await tools.Read(path=\"image.png\")` returns an `Image` handle; pass it to `inspect(img)` to attach it to the next model request.\n");
    }
    out.push_str("\nProgram-only Python SDK bindings:\n\n```python\n");
    out.push_str("from typing import Any, Awaitable, Literal, TypedDict\n\n");
    out.push_str(
        "class ToolCallError(Exception):\n    tool_name: str\n    kind: str\n    message: str\n\n",
    );
    out.push_str("class Image:\n    image_id: str\n    path: str\n    format: str\n    width: int\n    height: int\n    bytes: int\n\n");
    out.push_str("def inspect(*values: object) -> None: ...\n");
    out.push_str("async def gather(*aws: Awaitable[Any]) -> list[Any]: ...\n\n");
    out.push_str("class ReadResult(TypedDict, total=False):\n    path: str\n    tag: str | None\n    start_line: int\n    end_line: int\n    total_lines: int\n    content: str\n    raw: bool\n    memo_hit: bool\n\n");
    out.push_str("class GlobResult(TypedDict):\n    files: list[str]\n    truncated: bool\n\n");
    out.push_str("class GrepMatch(TypedDict, total=False):\n    path: str\n    tag: str | None\n    line: int\n    text: str\n    is_context: bool\n\n");
    out.push_str(
        "class GrepResult(TypedDict):\n    matches: list[GrepMatch]\n    truncated: bool\n\n",
    );
    out.push_str("class WriteResult(TypedDict, total=False):\n    path: str\n    bytes_written: int\n    tag: str | None\n\n");
    out.push_str(
        "class EditFileResult(TypedDict, total=False):\n    path: str\n    tag: str | None\n\n",
    );
    out.push_str("class EditResult(TypedDict):\n    applied: bool\n    files: list[EditFileResult]\n    summary: str\n\n");
    out.push_str("class BashResult(TypedDict):\n    exit_code: int | None\n    stdout: str\n    stderr: str\n    timed_out: bool\n\n");
    out.push_str("class TodoResult(TypedDict):\n    revision: int\n    items: list[dict[str, Any]]\n    counts: dict[str, int]\n\n");
    out.push_str("class tools:\n");

    let mut exotic_tools = Vec::new();
    for tool in &sorted {
        let name = tool.metadata.name.as_ref();
        let ret = return_type_for_tool(name, image_capable);
        let doc = tool
            .schema
            .get("description")
            .and_then(Value::as_str)
            .map(collapse_doc)
            .unwrap_or_default();
        let params = render_python_params(&tool.schema["input_schema"]);
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

fn render_python_params(input_schema: &Value) -> String {
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
        let ty = json_schema_to_python(prop);
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
        let first = render_python_sdk(&[b.clone(), a.clone()], PtcMode::Exclusive, false);
        let second = render_python_sdk(&[a, b], PtcMode::Exclusive, false);
        assert_eq!(first, second);
        let alpha_pos = first.find("async def Alpha(").unwrap();
        let zeta_pos = first.find("async def Zeta(").unwrap();
        assert!(alpha_pos < zeta_pos);
        assert!(first.contains("mode: Literal[\"a\", \"b\"]"));
        assert!(first.contains("limit: int = ..."));
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
        let rendered = render_python_sdk(&[exotic], PtcMode::Tool, false);
        assert!(rendered.contains("tools[\"my-tool\"]"));
        assert!(!rendered.contains("async def my-tool("));
    }
}
