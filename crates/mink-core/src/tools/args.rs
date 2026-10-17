//! Shared built-in tool argument decoding.
//!
//! Every built-in tool decodes its argument struct through [`decode_args`] at
//! the position where it would otherwise call `serde_json::from_value`. The
//! resulting [`ToolArgumentError`] is the runner's only signal that a failure
//! came from model output formatting instead of a real execution failure —
//! therefore the helper must run before any tool side effect.
//!
//! The argument structs keep their original serde rules (including
//! `deny_unknown_fields`): decode failures are reported to the model, never
//! silently repaired by accepting unknown fields.

/// A built-in tool argument decode failure.
#[derive(Debug)]
pub struct ToolArgumentError {
    detail: String,
}

impl ToolArgumentError {
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for ToolArgumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for ToolArgumentError {}

/// Decode one built-in tool's arguments, tagging failures as model format
/// errors. Call this before any tool side effect.
pub fn decode_args<T: serde::de::DeserializeOwned>(input: &serde_json::Value) -> anyhow::Result<T> {
    serde_json::from_value(input.clone()).map_err(|error| {
        anyhow::Error::new(ToolArgumentError::new(format!(
            "invalid tool arguments: {error}"
        )))
    })
}

/// Same as [`decode_args`] but lets the call site attach extra context while
/// keeping the typed error wrapper.
pub fn decode_args_context<T: serde::de::DeserializeOwned>(
    input: &serde_json::Value,
    context: impl std::fmt::Display,
) -> anyhow::Result<T> {
    serde_json::from_value(input.clone()).map_err(|error| {
        anyhow::Error::new(ToolArgumentError::new(format!(
            "invalid tool arguments: {context}: {error}"
        )))
    })
}

/// Whether an execution error is a model-format argument failure.
pub fn is_argument_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ToolArgumentError>().is_some()
}

/// Tag a post-decode validation failure as model-format input.
///
/// Argument structs decode before a tool body runs; shape/empty/unknown-tag
/// checks inside the body are still the model's input, not an execution
/// failure. They must use this wrapper so the runner keeps them in the format
/// window and out of belief/evidence (same path as [`decode_args`]).
pub fn invalid_input(detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ToolArgumentError::new(detail))
}
