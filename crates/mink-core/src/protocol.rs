use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum Event {
    Text(TextEvent),
    Thinking(ThinkingEvent),
    ToolCall(ToolCallEvent),
    Usage(UsageEvent),
    UsageUnavailable,
    Stop(StopEvent),
    Error(ErrorEvent),
    Retry(RetryEvent),
}

#[derive(Debug, Clone)]
pub struct TextEvent {
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ThinkingEvent {
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct StopEvent {
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ErrorEvent {
    pub message: String,
    /// Provider error code (`code`/`type`) from an error envelope delivered
    /// inside a `200` stream, when the provider sent one.
    pub provider_code: Option<String>,
    /// Provider status from an error envelope, when present.
    pub status: Option<u16>,
}

impl ErrorEvent {
    /// Construct an envelope-less provider error (legacy shape).
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            provider_code: None,
            status: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RetryEvent {}

#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct ToolCallEvent {
    pub name: String,
    pub id: String,
    pub input_json: Value,
    pub fields: BTreeMap<String, String>,
    /// Model-generated tool arguments that could not be parsed. The SSE layer
    /// fills this instead of failing the turn; the runner converts the call
    /// into a failed tool result so the model can retry.
    pub parse_error: Option<String>,
    /// Digest of the original raw arguments text when they failed to parse.
    /// Storm accounting uses it so distinct bad payloads never collapse into
    /// one identity through their shared `{}` placeholder.
    pub raw_arguments_digest: Option<String>,
}

impl ToolCallEvent {
    /// Stable identity for storm accounting.
    pub fn storm_identity(&self) -> String {
        match &self.raw_arguments_digest {
            Some(digest) => format!("raw:{digest}"),
            None => serde_json::to_string(&self.input_json).unwrap_or_default(),
        }
    }
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
