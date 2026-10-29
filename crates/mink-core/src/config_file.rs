//! Shared `.minkrc` file schema and parser used by the CLI and server.
//!
//! Host-specific precedence and conversion into runtime options stay with the
//! host adapters; the accepted TOML shape and unknown-field behavior live here.

use crate::runtime::{EditMode, SignalPolicy, ToolApprovalMode, ToolApprovalPolicy};
use std::collections::BTreeMap;

/// Root of the grouped `.minkrc` TOML format.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinkConfigFile {
    #[serde(default)]
    pub provider: ProviderConfigFile,
    #[serde(default)]
    pub generation: GenerationConfigFile,
    #[serde(default)]
    pub context: ContextConfigFile,
    #[serde(default)]
    pub tools: ToolsConfigFile,
    #[serde(default)]
    pub signal: SignalPolicyFile,
    #[serde(default)]
    pub recovery: RecoveryConfigFile,
    #[serde(default)]
    pub sandbox: SandboxConfigFile,
    #[serde(default)]
    pub sandbox_python: SandboxPythonConfigFile,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfigFile {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub model_aliases: Option<BTreeMap<String, String>>,
    pub openai_reasoning_effort: Option<String>,
    pub openai_include_usage: Option<bool>,
    pub openai_token_param: Option<String>,
    pub openai_tool_choice: Option<serde_json::Value>,
    pub openai_extra_body: Option<BTreeMap<String, serde_json::Value>>,
    /// Provider HTTP request total timeout in seconds; `0` disables the total timeout.
    pub http_timeout_secs: Option<u64>,
    /// Explicit image input capability: `on` or `off`.
    pub image_input: Option<String>,
    /// Model ids declared image capable. An empty list disables image capture.
    pub vision_models: Option<Vec<String>>,
    /// Per-field image limit overrides under `[provider.image]`.
    pub image: ImageConfigFile,
}

/// `[provider.image]` limits. Byte and pixel values accept plain integers or K/M/G suffixes.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImageConfigFile {
    pub detail: Option<String>,
    pub max_images_per_request: Option<usize>,
    pub max_image_bytes_per_request: Option<String>,
    pub max_image_bytes: Option<String>,
    pub max_dimension: Option<u32>,
    pub max_pixels: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GenerationConfigFile {
    pub max_tokens: Option<i32>,
    pub max_turns: Option<i32>,
    pub llm_first_event_timeout: Option<i32>,
    pub llm_idle_timeout: Option<i32>,
    pub llm_wait_heartbeat: Option<i32>,
    pub log_events: Option<bool>,
    pub output_format: Option<String>,
}

/// Bounded format-error window and request retry settings.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecoveryConfigFile {
    pub format_window_size: Option<usize>,
    pub format_max_errors: Option<usize>,
    pub request_max_retries: Option<u32>,
    pub request_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfigFile {
    pub max_context: Option<String>,
    pub context_compact_pct: Option<u8>,
    pub context_reserve_tokens: Option<usize>,
    pub context_compact_tail_tokens: Option<usize>,
    pub context_compact_max_output_tokens: Option<i32>,
    pub context_compact_input_reduction: Option<bool>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditConfigFile {
    pub mode: Option<EditMode>,
    pub fuzzy_match: Option<bool>,
    pub fuzzy_threshold: Option<f64>,
    pub enforce_seen_lines: Option<bool>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfigFile {
    pub approval_mode: Option<ToolApprovalMode>,
    pub approval: Option<BTreeMap<String, ToolApprovalPolicy>>,
    pub enabled_tools: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub tool_timeout: Option<i32>,
    pub tool_timeout_max: Option<i32>,
    pub sub_agent_timeout: Option<i32>,
    pub sub_agent_max_depth: Option<u32>,
    pub max_search_files: Option<usize>,
    pub max_search_results: Option<usize>,
    pub edit: EditConfigFile,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SignalPolicyFile {
    pub policy: Option<SignalPolicy>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfigFile {
    pub enabled: Option<bool>,
    pub backend: Option<String>,
    pub read_dirs: Option<Vec<String>>,
    pub write_dirs: Option<Vec<String>>,
    pub allow_network: Option<bool>,
    pub max_memory_mb: Option<u64>,
    pub max_pids: Option<u32>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxPythonConfigFile {
    pub wasm_path: Option<String>,
    pub stdlib_dir: Option<String>,
    pub timeout: Option<u64>,
    pub read_dirs: Option<Vec<String>>,
    pub write_dirs: Option<Vec<String>>,
    pub package_dirs: Option<Vec<String>>,
}

/// Parse a `.minkrc` using the one shared schema and strict unknown-field rule.
pub fn parse_config_file(text: &str) -> anyhow::Result<MinkConfigFile> {
    Ok(toml::from_str(text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_schema_keeps_explicit_empty_tool_list_and_rejects_unknown_fields() {
        let parsed = parse_config_file("[tools]\nenabled_tools = []\n").unwrap();
        assert_eq!(parsed.tools.enabled_tools, Some(Vec::new()));
        assert!(parse_config_file("[tools]\nenabled_tools = []\nunknown = true\n").is_err());
    }

    #[test]
    fn shared_schema_accepts_multimodal_and_recovery_sections() {
        let parsed = parse_config_file(
            "[provider]\nimage_input = 'on'\n\n[provider.image]\nmax_images_per_request = 2\n\n[recovery]\nrequest_max_retries = 3\n",
        )
        .unwrap();
        assert_eq!(parsed.provider.image_input.as_deref(), Some("on"));
        assert_eq!(parsed.provider.image.max_images_per_request, Some(2));
        assert_eq!(parsed.recovery.request_max_retries, Some(3));
    }
}
