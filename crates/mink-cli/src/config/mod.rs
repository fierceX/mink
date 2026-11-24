use anyhow::{Result, anyhow, bail};
use std::path::PathBuf;

pub use mink::runtime::{
    ContextPolicy, EditMode, GenerationOptions, OutputFormat, ProviderOptions, SandboxConfig,
    SandboxPythonConfig, SignalPolicy, TokenParamKind, ToolOptions,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TuiMode {
    #[default]
    Off,
    Full,
    Inline,
}

impl TuiMode {
    pub fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CliEarlyExit {
    Help,
    Version,
}

impl std::fmt::Display for CliEarlyExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Help => f.write_str("help requested"),
            Self::Version => f.write_str("version requested"),
        }
    }
}

impl std::error::Error for CliEarlyExit {}

use mink::config_file::{ImageConfigFile, MinkConfigFile};

#[derive(Debug, Clone)]
pub struct CliConfig {
    pub provider: ProviderOptions,
    pub generation: GenerationOptions,
    pub context: ContextPolicy,
    pub tools: ToolOptions,
    pub output_format: OutputFormat,
    pub verbose: bool,
    pub tui_mode: TuiMode,
    pub prompt: String,
    pub skills: Vec<String>,
    pub interactive: bool,
    pub session_id: String,
    pub continue_session: bool,
    pub list_sessions: bool,
    pub list_skills: bool,
    pub log_events: bool,
    pub cli_overrides: CliOverrides,
    /// Stable single-shot SDK protocol: stdin request + stdout JSONL events + final.
    pub agent_jsonl: bool,
    /// 沙箱配置（从 .minkrc 加载）
    pub sandbox: SandboxConfig,
    /// CPython WASI 沙箱工具配置
    pub sandbox_python: SandboxPythonConfig,
    /// 自定义系统提示词文件（MISSION.md）
    pub mission_file: Option<PathBuf>,
    /// 内联 mission 内容（通过 SDK 协议传入，不写临时文件）
    pub mission_content: Option<String>,
    /// Explicit image-input capability: `Some(cap)` overrides the backend
    /// declaration; `None` defers to it (v7 §3.1).
    pub image_input: Option<mink::runtime::ImageInputCapability>,
    /// Explicit vision model ids; `Some(list)` replaces the built-in default
    /// list (empty disables image capture), `None` keeps the built-in list.
    pub vision_models: Option<Vec<String>>,
    /// `[provider.image]` overrides, merged across config-file layers
    /// (higher layer wins per field).
    pub image_limits: Option<mink::runtime::ImageLimitsOverrides>,
    /// 从 --config CLI 参数解析的 TOML 配置（最高优先级，在 apply_config_sources 中应用）
    pub cli_config: Option<MinkConfigFile>,
    pub signal_policy: SignalPolicy,
    /// Bounded LLM recovery policy ([recovery] / SDK options.recovery).
    pub llm_recovery: mink::runtime::LlmRecoveryPolicy,
}

/// Fields explicitly provided by CLI flags. Only these inputs outrank the
/// config-file layer; env vars are applied before files and never set these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOverrides {
    pub model: bool,
    pub api_key: bool,
    pub base_url: bool,
    pub output_format: bool,
    pub enabled_tools: bool,
    pub skills: bool,
    pub edit_mode: bool,
    pub edit_fuzzy_match: bool,
    pub edit_fuzzy_threshold: bool,
    pub edit_enforce_seen_lines: bool,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            provider: ProviderOptions::default(),
            generation: GenerationOptions::default(),
            context: ContextPolicy::default(),
            tools: ToolOptions::default(),
            output_format: OutputFormat::Human,
            verbose: false,
            tui_mode: TuiMode::Off,
            prompt: String::new(),
            skills: Vec::new(),
            interactive: false,
            session_id: String::new(),
            continue_session: false,
            list_sessions: false,
            list_skills: false,
            log_events: true,
            cli_overrides: CliOverrides::default(),
            agent_jsonl: false,
            sandbox: SandboxConfig::default(),
            sandbox_python: SandboxPythonConfig::default(),
            mission_file: None,
            mission_content: None,
            image_input: None,
            vision_models: None,
            image_limits: None,
            cli_config: None,
            signal_policy: SignalPolicy::Full,
            llm_recovery: mink::runtime::LlmRecoveryPolicy::default(),
        }
    }
}

mod args;
mod layers;
mod validate;

pub use args::{parse_args, parse_image_input, parse_image_limits, parse_size_bytes};
pub use layers::{apply_config_file, apply_provider_defaults};
#[cfg(test)]
use layers::{
    apply_config_sources, apply_env_defaults, apply_log_events_env_value, apply_size_limit_envs,
};
pub(crate) use layers::{apply_sdk_request_options, default_home};
pub use validate::validate_runtime_config;

/// Resolve the display label for the title bar.
#[cfg(feature = "tui")]
pub fn resolve_model_label(
    model: &str,
    aliases: &std::collections::BTreeMap<String, String>,
) -> String {
    crate::runtime::ModelResolver::new(aliases)
        .resolve(model)
        .label
}

#[cfg(test)]
use mink::runtime::{ToolApprovalMode, ToolApprovalPolicy};
#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
#[path = "../config_tests.rs"]
mod tests;
