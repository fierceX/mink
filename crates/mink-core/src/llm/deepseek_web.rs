use serde_json::{Value, json};

pub(crate) const PRIVATE_FIELD: &str = "deepseek_web";
const SEARCH: &str = "deepseek_web_search";
const READ_URL: &str = "deepseek_read_url";

pub(crate) fn is_declaration(tool: &Value) -> bool {
    matches!(tool["type"].as_str(), Some(SEARCH | READ_URL))
}

pub(crate) fn enabled(tools: &[Value]) -> bool {
    tools.iter().any(is_declaration)
}

pub(crate) fn project_replay(messages: &mut [Value], tools: &[Value]) {
    if !enabled(tools) {
        for message in messages {
            remove_replay_fields(message);
            if let Some(object) = message.as_object_mut() {
                object.remove("server_tool_calls");
            }
        }
    }
}

pub(crate) fn declarations(options: crate::runtime::WebOptions) -> Vec<Value> {
    [
        (options.native_search, SEARCH),
        (options.native_read_url, READ_URL),
    ]
    .into_iter()
    .filter(|(enabled, _)| *enabled)
    .map(|(_, name)| json!({"type": name}))
    .collect()
}

pub(crate) fn replay_fields(message: &Value) -> Option<&Value> {
    message.get("_mink")?.get(PRIVATE_FIELD)
}

pub(crate) fn remove_replay_fields(message: &mut Value) {
    if let Some(meta) = message.get_mut("_mink").and_then(Value::as_object_mut)
        && meta.remove(PRIVATE_FIELD).is_some()
        && meta.is_empty()
        && let Some(object) = message.as_object_mut()
    {
        object.remove("_mink");
    }
}

#[derive(Debug, Clone)]
pub struct DeepSeekWebResponse {
    pub calls: Option<Value>,
    pub annotations: Option<Value>,
    /// The final step's raw reasoning. Earlier reasoning lives within calls.
    pub reasoning: String,
}

impl DeepSeekWebResponse {
    fn tool_calls(&self) -> impl Iterator<Item = &Value> {
        self.calls
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(|step| step["calls"].as_array().into_iter().flatten())
    }

    pub(crate) fn read_link_urls(&self) -> Vec<&str> {
        let mut urls = Vec::new();
        for call in self.tool_calls() {
            if call["kind"] == "read_link"
                && let Some(url) =
                    safe_url(&call["action"]["url"]).or_else(|| safe_url(&call["page"]["url"]))
                && !urls.contains(&url)
            {
                urls.push(url);
                if urls.len() == 10 {
                    break;
                }
            }
        }
        urls
    }

    pub(crate) fn public_block(&self) -> Value {
        let calls = self
            .tool_calls()
            .filter_map(public_call)
            .take(40)
            .collect::<Vec<_>>();
        let citations = self
            .annotations
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|annotation| {
                let citation = &annotation["url_citation"];
                let url = safe_url(&citation["url"])?;
                Some(json!({
                    "url": url,
                    "title": bounded(&citation["title"], 400),
                    "start_index": citation["start_index"].as_u64()?,
                    "end_index": citation["end_index"].as_u64()?,
                }))
            })
            .take(50)
            .collect::<Vec<_>>();
        json!({"type": "deepseek_web", "calls": calls, "citations": citations})
    }
}

/// Project only the display fields. The signature and all provider replay
/// fields stay with the assistant record on disk.
pub(crate) fn public_call(call: &Value) -> Option<Value> {
    let id = call["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256)?;
    let action = &call["action"];
    let tool = match (call["type"].as_str(), action["type"].as_str()) {
        (Some(SEARCH), _) | (None, Some("search" | "open_page" | "find_in_page")) => {
            "DeepSeekWebSearch"
        }
        (Some(READ_URL), _) | (None, Some("read_link")) => "DeepSeekReadUrl",
        _ => return None,
    };
    let queries = action["queries"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|query| bounded(query, 240))
        .take(10)
        .collect::<Vec<_>>();
    let sources = call["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|source| {
            Some(json!({
                "url": safe_url(&source["url"])?,
                "title": bounded(&source["title"], 400),
                "snippet": bounded(&source["snippet"], 600),
            }))
        })
        .take(10)
        .collect::<Vec<_>>();
    let page = safe_url(&call["page"]["url"])
        .map(|url| json!({"url": url, "title": bounded(&call["page"]["title"], 400)}));
    let status = match call["status"].as_str() {
        Some("completed") => Some("completed"),
        Some("failed") => Some("failed"),
        _ => None,
    };
    Some(json!({
        "id": id,
        "tool": tool,
        "action": {
            "type": bounded(&action["type"], 32),
            "url": safe_url(&action["url"]),
            "queries": queries,
            "pattern": bounded(&action["pattern"], 240),
        },
        "status": status,
        "sources": sources,
        "page": page,
        "error": bounded(&call["error"]["code"], 80),
    }))
}

fn bounded(value: &Value, max_chars: usize) -> Option<String> {
    Some(value.as_str()?.chars().take(max_chars).collect())
}

fn safe_url(value: &Value) -> Option<&str> {
    let raw = value
        .as_str()
        .filter(|url| url.len() <= 8192 && !url.chars().any(char::is_control))?;
    reqwest::Url::parse(raw)
        .ok()
        .filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
        })
        .map(|_| raw)
}

pub(crate) fn render_progress(display: &dyn crate::ui::Display, call: &Value) {
    use crate::tools::metadata::{ToolResultKind, ToolStatus};
    use crate::ui::{PresentedToolResultDisplay, ToolCallDisplay, ToolResultDisplay};
    let name = call["tool"].as_str().unwrap_or_default();
    let id = call["id"].as_str().unwrap_or_default();
    let action = &call["action"];
    let summary = action["url"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| {
            action["queries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        });
    let status = match call["status"].as_str() {
        Some("completed") => ToolStatus::Succeeded,
        Some("failed") => ToolStatus::Failed(crate::tools::metadata::ToolFailureKind::Unknown),
        _ => {
            display.render_tool_call(&ToolCallDisplay {
                tool_use_id: id,
                tool_name: name,
                summary: &summary,
                input: Some(action),
            });
            return;
        }
    };
    let content = serde_json::to_string_pretty(call).unwrap_or_default();
    let preview = if let Some(error) = call["error"].as_str() {
        format!("{summary} · {error}")
    } else {
        let sources = call["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|source| source["url"].as_str())
            .take(3)
            .collect::<Vec<_>>();
        if sources.is_empty() {
            summary
        } else {
            sources.join("\n")
        }
    };
    display.render_tool_result(&PresentedToolResultDisplay {
        base: ToolResultDisplay {
            tool_name: name,
            content_preview: &format!("{preview}\n"),
            content: &content,
            tool_use_id: Some(id),
            exit_code: None,
        },
        status,
        result_kind: ToolResultKind::Search,
        presentation: None,
        artifacts: &[],
    });
}
