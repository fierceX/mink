use crate::protocol::{ErrorEvent, Event, StopEvent, TextEvent, ThinkingEvent, UsageEvent};
use crate::sse::toolcall::build_tool_call_event;
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeMap;

// ---- SSE frame parsing (reuses extract_frames from claude module) ----
// OpenAI SSE also uses `data:` lines delimited by `\n\n`.

#[derive(Default, Clone)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
    /// Set when a provider sent `arguments` with a non-string type (e.g.
    /// `false`); the raw value is kept for the degraded candidate and its
    /// storm digest. A later valid string fragment clears it again.
    arguments_type_error: Option<String>,
}
/// Incremental OpenAI SSE parser.
#[derive(Default)]
pub struct OpenAIParser {
    stop_reason: String,
    prompt_tokens_total: Option<i64>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_input_tokens: i64,
    cache_creation_input_tokens: i64,
    saw_text: bool,
    pending_calls: BTreeMap<i64, PendingCall>,
    pending_usage: Option<UsageEvent>,
    pending_stop: Option<String>,
    saw_done: bool,
    saw_usage: bool,
}

impl OpenAIParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process one raw SSE line (e.g. `data: {"id":"chatcmpl-...",...}`).
    /// Returns Ok(true) when stream is done ([DONE]).
    pub fn process_line(
        &mut self,
        line: &str,
        emit: &mut dyn FnMut(Event) -> Result<()>,
    ) -> Result<bool> {
        let l = line.trim_end_matches(['\n', '\r']);

        // SSE 规范的 `retry: <ms>` 行只影响重连间隔，对单次请求流是可忽略
        // 的控制行（落入下方非 data 分支静默跳过）。历史上的大写 `RETRY:`
        // 分支匹配无值字面量、实际不可达，且一旦触发会半重置累计状态并
        // 丢失重置前的 usage 计费，已删除。

        if l.is_empty() || !l.starts_with("data:") {
            return Ok(false);
        }

        // Accept both `data: {...}` and `data:{...}` (some SSE senders omit
        // the optional space after the colon).
        let payload = l[5..].trim_start_matches(' ');
        if payload == "[DONE]" {
            self.saw_done = true;
            self.prepare_terminal_events(emit)?;
            return Ok(true);
        }

        let body: Value = serde_json::from_str(payload)?;
        if let Some(error) = body.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider error envelope");
            // Structured fields let the runtime classify a `200` envelope by
            // code/type/status exactly like an HTTP error status. Unknown
            // envelopes stay permanent instead of guessing from text.
            let provider_code = error
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    error
                        .get("type")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            let status = error.get("status").and_then(|status| {
                status
                    .as_u64()
                    .and_then(|value| u16::try_from(value).ok())
                    .or_else(|| status.as_str().and_then(|value| value.parse().ok()))
            });
            emit(Event::Error(ErrorEvent {
                message: message.into(),
                provider_code,
                status,
            }))?;
            return Ok(false);
        }

        let usage = body.get("usage").cloned().unwrap_or(Value::Null);
        if usage.get("prompt_tokens").is_some()
            || usage.get("completion_tokens").is_some()
            || usage.get("cached_tokens").is_some()
            || usage.get("prompt_tokens_details").is_some()
            || usage.get("cache_creation_input_tokens").is_some()
            || usage.get("prompt_cache_hit_tokens").is_some()
        {
            self.saw_usage = true;
        }
        if let Some(v) = usage.get("completion_tokens").and_then(Value::as_i64) {
            self.output_tokens = v;
        }
        // 缓存命中字段双拼写兜底（对齐 DSH translate.ts 的 disjoint 约定）：
        // OpenAI 兼容拼写（顶层 cached_tokens 或 prompt_tokens_details.cached_tokens）
        // 优先，DeepSeek 原生拼写 prompt_cache_hit_tokens 兜底。
        // prompt_cache_miss_tokens 不单独记——它隐含在互斥分区
        // input_tokens = prompt_tokens - cache_read - cache_creation 中。
        if let Some(v) = usage
            .get("cached_tokens")
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
            })
            .or_else(|| usage.get("prompt_cache_hit_tokens"))
            .and_then(Value::as_i64)
        {
            self.cache_read_input_tokens = v;
        }
        // cache_creation_input_tokens: direct field or from prompt_tokens_details.cache_creation
        if let Some(v) = usage
            .get("cache_creation_input_tokens")
            .or_else(|| {
                usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cache_creation"))
            })
            .and_then(Value::as_i64)
        {
            self.cache_creation_input_tokens = v;
        }
        if let Some(v) = usage.get("prompt_tokens").and_then(Value::as_i64) {
            self.prompt_tokens_total = Some(v);
        }
        self.recompute_input_tokens();
        let choice = body
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .cloned()
            .unwrap_or(Value::Null);

        if let Some(r) = choice.get("finish_reason").and_then(Value::as_str)
            && !r.is_empty()
            && r != "null"
        {
            self.stop_reason = r.into();
        }

        let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
        if let Some(content) = delta.get("content").and_then(Value::as_str) {
            let c = if !self.saw_text {
                content.trim_start_matches(['\n', '\r']).to_string()
            } else {
                content.to_string()
            };
            if !c.is_empty() {
                self.saw_text = true;
                emit(Event::Text(TextEvent { content: c }))?;
            }
        }

        if let Some(reasoning) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            && !reasoning.is_empty()
        {
            let reasoning = strip_reasoning_think_tags(reasoning);
            if !reasoning.trim().is_empty() {
                emit(Event::Thinking(ThinkingEvent { content: reasoning }))?;
            }
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in tool_calls {
                let idx = tc.get("index").and_then(Value::as_i64).unwrap_or(0);
                let entry = self.pending_calls.entry(idx).or_default();
                if let Some(v) = tc.get("id").and_then(Value::as_str) {
                    entry.id = v.into();
                }
                if let Some(v) = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                {
                    entry.name = v.into();
                }
                if let Some(arguments) = tc.get("function").and_then(|f| f.get("arguments")) {
                    match arguments.as_str() {
                        Some(v) => {
                            // Fragments accumulate; whether an earlier
                            // wrong-typed value was corrected is decided at
                            // emit time from the *final* payload, never from
                            // the mere arrival of a (possibly empty) fragment.
                            entry.arguments.push_str(v);
                        }
                        None if !arguments.is_null() => {
                            entry.arguments_type_error = Some(arguments.to_string());
                        }
                        None => {}
                    }
                }
            }
        }
        if let Some(function_call) = delta.get("function_call") {
            let entry = self.pending_calls.entry(0).or_default();
            if entry.id.is_empty() {
                // Legacy compatibility: providers send no id, so generate one
                // that cannot collide with ids already in the conversation
                // history (deterministic compatibility never consumes the
                // format window).
                entry.id = crate::sse::toolcall::unique_call_id("legacy_function_call");
            }
            if let Some(v) = function_call.get("name").and_then(Value::as_str) {
                entry.name = v.into();
            }
            if let Some(arguments) = function_call.get("arguments") {
                match arguments.as_str() {
                    Some(v) => entry.arguments.push_str(v),
                    None if !arguments.is_null() => {
                        entry.arguments_type_error = Some(arguments.to_string());
                    }
                    None => {}
                }
            }
        }

        if matches!(
            choice.get("finish_reason").and_then(Value::as_str),
            Some("tool_calls" | "function_call")
        ) {
            self.emit_pending(emit)?;
        }

        Ok(false)
    }

    /// Flush pending usage + stop events.
    pub fn flush(&mut self, emit: &mut dyn FnMut(Event) -> Result<()>) -> Result<()> {
        if let Some(usage) = self.pending_usage.take() {
            emit(Event::Usage(usage))?;
        }
        if let Some(reason) = self.pending_stop.take() {
            emit(Event::Stop(StopEvent { reason }))?;
        }
        Ok(())
    }

    /// Finalize a stream that reached transport EOF. Some compatible
    /// providers close after a finish_reason frame without sending [DONE].
    pub fn finish_eof(&mut self, emit: &mut dyn FnMut(Event) -> Result<()>) -> Result<()> {
        if self.pending_stop.is_some() || self.saw_done {
            return Ok(());
        }
        if self.stop_reason.is_empty() {
            anyhow::bail!("stream ended before finish_reason");
        }
        self.prepare_terminal_events(emit)
    }

    fn prepare_terminal_events(&mut self, emit: &mut dyn FnMut(Event) -> Result<()>) -> Result<()> {
        self.emit_pending(emit)?;
        if self.saw_usage {
            self.pending_usage = Some(UsageEvent {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cache_read_input_tokens: self.cache_read_input_tokens,
                cache_creation_input_tokens: self.cache_creation_input_tokens,
            });
        } else {
            emit(Event::UsageUnavailable)?;
        }
        self.pending_stop = Some(self.stop_reason.clone());
        Ok(())
    }

    fn recompute_input_tokens(&mut self) {
        let Some(total) = self.prompt_tokens_total else {
            return;
        };
        // UsageEvent input/cache-read/cache-creation are disjoint partitions.
        // Cache details may arrive in later SSE frames than prompt_tokens, so
        // recompute from the retained total after every usage update.
        self.input_tokens = total
            .saturating_sub(self.cache_read_input_tokens)
            .saturating_sub(self.cache_creation_input_tokens)
            .max(0);
    }

    fn emit_pending(&mut self, emit: &mut dyn FnMut(Event) -> Result<()>) -> Result<()> {
        for call in self.pending_calls.values_mut() {
            // Every candidate is surfaced with its identity: a missing name is
            // emitted as an invalid candidate (the round layer drops the whole
            // batch) instead of being silently skipped.
            let name = std::mem::take(&mut call.name);
            let id = std::mem::take(&mut call.id);
            let arguments = std::mem::take(&mut call.arguments);
            let type_error = call.arguments_type_error.take();
            let built = build_tool_call_event(&name, &id, &arguments);
            let event = match (type_error, built) {
                // A wrong-typed `arguments` is real response damage: keep the
                // error — later fragments never "correct" it in stream; the
                // model resends in the next round — and surface the degraded
                // candidate instead of executing with empty/wrong
                // arguments.
                (Some(raw), _) => crate::sse::toolcall::degraded_tool_call(
                    &name,
                    &id,
                    &raw,
                    "tool arguments must be a JSON string",
                ),
                (None, Ok(event)) => event,
                (None, Err(original)) => {
                    // Unsafe lexical repairs (closing truncated strings,
                    // filling nulls) must never be executed directly: an
                    // unparsable payload is reported to the model, which
                    // resends corrected arguments. The raw payload digest
                    // keeps distinct bad calls distinct for storm accounting.
                    crate::sse::toolcall::degraded_tool_call(
                        &name,
                        &id,
                        &arguments,
                        &format!("{original:#}"),
                    )
                }
            };
            emit(Event::ToolCall(event))?;
        }
        // Consumed candidates are removed so a second terminal pass (finish
        // reason followed by [DONE]/EOF) cannot re-emit empty placeholders.
        self.pending_calls.clear();
        Ok(())
    }
}

fn strip_reasoning_think_tags(content: &str) -> String {
    content.replace("<think>", "").replace("</think>", "")
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
