//! Server adapter for shared `.minkrc` configuration and runtime options.
//!
//! The file schema and strict TOML parser live in `mink::config_file`. This
//! module owns server layer precedence, environment overrides, and conversion
//! into runtime options.

#[cfg(test)]
use mink::runtime::ImageDetail;
use mink::runtime::{
    AgentOptions, ContextPolicy, EditMode, GenerationOptions, ImageInputCapability,
    ImageLimitsOverrides, ProviderOptions, PtcBackendKind, PtcConfig, PtcMode, SandboxConfig,
    SandboxPythonConfig, SignalPolicy, TokenParamKind, ToolApprovalMode, ToolApprovalPolicy,
    ToolOptions,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

use mink::config_file::{
    MIN_TOOL_TIMEOUT_MAX_SECS, MinkConfigFile, PtcConfigFile, RecoveryConfigFile,
    SandboxConfigFile, SandboxPythonConfigFile, WebConfigFile, normalize_openai_reasoning_effort,
    parse_image_input, parse_image_limits, parse_size_bytes,
};

/// Merge two recovery sections per field (higher layer wins).
fn merge_recovery(base: RecoveryConfigFile, over: RecoveryConfigFile) -> RecoveryConfigFile {
    RecoveryConfigFile {
        format_window_size: over.format_window_size.or(base.format_window_size),
        format_max_errors: over.format_max_errors.or(base.format_max_errors),
        request_max_retries: over.request_max_retries.or(base.request_max_retries),
        request_timeout_secs: over.request_timeout_secs.or(base.request_timeout_secs),
    }
}

/// Resolved agent configuration layer (one file layer or the merged result).
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentConfig {
    pub model: Option<String>,
    pub model_aliases: BTreeMap<String, String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub image_input: Option<ImageInputCapability>,
    pub vision_models: Option<Vec<String>>,
    pub image_limits: Option<ImageLimitsOverrides>,
    pub openai_reasoning_effort: Option<String>,
    pub openai_include_usage: Option<bool>,
    pub openai_token_param: Option<TokenParamKind>,
    pub openai_tool_choice: Option<Value>,
    pub openai_extra_body: BTreeMap<String, Value>,
    pub http_timeout_secs: Option<u64>,
    pub max_tokens: Option<i32>,
    pub max_turns: Option<i32>,
    pub llm_first_event_timeout_secs: Option<i32>,
    pub llm_idle_timeout_secs: Option<i32>,
    pub llm_wait_heartbeat_secs: Option<i32>,
    pub log_events: Option<bool>,
    pub max_context_tokens: Option<usize>,
    pub context_compact_pct: Option<u8>,
    pub context_reserve_tokens: Option<usize>,
    pub context_compact_tail_tokens: Option<usize>,
    pub context_compact_max_output_tokens: Option<i32>,
    pub context_compact_input_reduction: Option<bool>,
    pub tool_timeout_secs: Option<i32>,
    pub tool_timeout_max_secs: Option<i32>,
    pub sub_agent_timeout_secs: Option<i32>,
    pub sub_agent_max_depth: Option<u32>,
    pub tool_result_max_bytes: Option<usize>,
    pub file_write_max_bytes: Option<usize>,
    pub max_search_files: Option<usize>,
    pub max_search_results: Option<usize>,
    pub enabled_tools: Option<Vec<String>>,
    pub approval_mode: Option<ToolApprovalMode>,
    pub approval: BTreeMap<String, ToolApprovalPolicy>,
    pub skills: Option<Vec<String>>,
    pub edit_mode: Option<EditMode>,
    pub edit_fuzzy_match: Option<bool>,
    pub edit_fuzzy_threshold: Option<f64>,
    pub edit_enforce_seen_lines: Option<bool>,
    pub signal_policy: Option<SignalPolicy>,
    pub recovery: RecoveryConfigFile,
    pub sandbox: SandboxConfigFile,
    pub sandbox_python: SandboxPythonConfigFile,
    pub ptc: PtcConfigFile,
    pub web: WebConfigFile,
}

mod apply;
mod env;
mod parse;
#[cfg(test)]
mod tests;

pub(crate) use apply::apply_to;
pub(crate) use env::apply_env_overrides;
#[cfg(test)]
pub(crate) use parse::parse_layer;
#[cfg(test)]
use parse::{from_file, load_layer};
pub(crate) use parse::{load_project_layer, load_user_layer};

/// Layer merge: every field of `over` replaces `base` when present
/// (project over user, exactly like the CLI's layered application order).
pub(crate) fn merge(base: AgentConfig, over: AgentConfig) -> AgentConfig {
    fn pick<T>(base: Option<T>, over: Option<T>) -> Option<T> {
        over.or(base)
    }
    // `[provider.image]` merges per field across layers (the higher layer
    // wins); absent fields keep the base layer, like the CLI.
    let mut image_limits = base.image_limits;
    if let Some(over) = over.image_limits {
        match &mut image_limits {
            Some(existing) => existing.merge(over),
            None => image_limits = Some(over),
        }
    }
    AgentConfig {
        model: pick(base.model, over.model),
        model_aliases: {
            let mut aliases = base.model_aliases;
            aliases.extend(over.model_aliases);
            aliases
        },
        api_key: pick(base.api_key, over.api_key),
        base_url: pick(base.base_url, over.base_url),
        image_input: pick(base.image_input, over.image_input),
        vision_models: pick(base.vision_models, over.vision_models),
        image_limits,
        openai_reasoning_effort: pick(base.openai_reasoning_effort, over.openai_reasoning_effort),
        openai_include_usage: pick(base.openai_include_usage, over.openai_include_usage),
        openai_token_param: pick(base.openai_token_param, over.openai_token_param),
        openai_tool_choice: pick(base.openai_tool_choice, over.openai_tool_choice),
        openai_extra_body: {
            let mut body = base.openai_extra_body;
            body.extend(over.openai_extra_body);
            body
        },
        http_timeout_secs: pick(base.http_timeout_secs, over.http_timeout_secs),
        max_tokens: pick(base.max_tokens, over.max_tokens),
        max_turns: pick(base.max_turns, over.max_turns),
        llm_first_event_timeout_secs: pick(
            base.llm_first_event_timeout_secs,
            over.llm_first_event_timeout_secs,
        ),
        llm_idle_timeout_secs: pick(base.llm_idle_timeout_secs, over.llm_idle_timeout_secs),
        llm_wait_heartbeat_secs: pick(base.llm_wait_heartbeat_secs, over.llm_wait_heartbeat_secs),
        log_events: pick(base.log_events, over.log_events),
        max_context_tokens: pick(base.max_context_tokens, over.max_context_tokens),
        context_compact_pct: pick(base.context_compact_pct, over.context_compact_pct),
        context_reserve_tokens: pick(base.context_reserve_tokens, over.context_reserve_tokens),
        context_compact_tail_tokens: pick(
            base.context_compact_tail_tokens,
            over.context_compact_tail_tokens,
        ),
        context_compact_max_output_tokens: pick(
            base.context_compact_max_output_tokens,
            over.context_compact_max_output_tokens,
        ),
        context_compact_input_reduction: pick(
            base.context_compact_input_reduction,
            over.context_compact_input_reduction,
        ),
        tool_timeout_secs: pick(base.tool_timeout_secs, over.tool_timeout_secs),
        tool_timeout_max_secs: pick(base.tool_timeout_max_secs, over.tool_timeout_max_secs),
        sub_agent_timeout_secs: pick(base.sub_agent_timeout_secs, over.sub_agent_timeout_secs),
        sub_agent_max_depth: pick(base.sub_agent_max_depth, over.sub_agent_max_depth),
        tool_result_max_bytes: pick(base.tool_result_max_bytes, over.tool_result_max_bytes),
        file_write_max_bytes: pick(base.file_write_max_bytes, over.file_write_max_bytes),
        max_search_files: pick(base.max_search_files, over.max_search_files),
        max_search_results: pick(base.max_search_results, over.max_search_results),
        enabled_tools: pick(base.enabled_tools, over.enabled_tools),
        approval_mode: pick(base.approval_mode, over.approval_mode),
        approval: {
            let mut approval = base.approval;
            approval.extend(over.approval);
            approval
        },
        skills: pick(base.skills, over.skills),
        edit_mode: pick(base.edit_mode, over.edit_mode),
        edit_fuzzy_match: pick(base.edit_fuzzy_match, over.edit_fuzzy_match),
        edit_fuzzy_threshold: pick(base.edit_fuzzy_threshold, over.edit_fuzzy_threshold),
        edit_enforce_seen_lines: pick(base.edit_enforce_seen_lines, over.edit_enforce_seen_lines),
        signal_policy: pick(base.signal_policy, over.signal_policy),
        recovery: merge_recovery(base.recovery, over.recovery),
        sandbox: merge_sandbox(base.sandbox, over.sandbox),
        sandbox_python: merge_sandbox_python(base.sandbox_python, over.sandbox_python),
        ptc: merge_ptc(base.ptc, over.ptc),
        web: WebConfigFile {
            client: pick(base.web.client, over.web.client),
            native_search: pick(base.web.native_search, over.web.native_search),
            native_read_url: pick(base.web.native_read_url, over.web.native_read_url),
        },
    }
}

fn merge_ptc(base: PtcConfigFile, over: PtcConfigFile) -> PtcConfigFile {
    fn pick<T>(base: Option<T>, over: Option<T>) -> Option<T> {
        over.or(base)
    }
    PtcConfigFile {
        mode: pick(base.mode, over.mode),
        backend: pick(base.backend, over.backend),
        python_executable: pick(base.python_executable, over.python_executable),
        timeout_secs: pick(base.timeout_secs, over.timeout_secs),
        max_timeout_secs: pick(base.max_timeout_secs, over.max_timeout_secs),
        max_parallel_sub_calls: pick(base.max_parallel_sub_calls, over.max_parallel_sub_calls),
        max_sub_calls_per_minute: pick(
            base.max_sub_calls_per_minute,
            over.max_sub_calls_per_minute,
        ),
        max_output_bytes: pick(base.max_output_bytes, over.max_output_bytes),
        max_message_bytes: pick(base.max_message_bytes, over.max_message_bytes),
        max_pending_calls: pick(base.max_pending_calls, over.max_pending_calls),
        stop_grace_ms: pick(base.stop_grace_ms, over.stop_grace_ms),
    }
}

fn merge_sandbox(base: SandboxConfigFile, over: SandboxConfigFile) -> SandboxConfigFile {
    fn pick<T>(base: Option<T>, over: Option<T>) -> Option<T> {
        over.or(base)
    }
    SandboxConfigFile {
        enabled: pick(base.enabled, over.enabled),
        backend: pick(base.backend, over.backend),
        read_dirs: pick(base.read_dirs, over.read_dirs),
        write_dirs: pick(base.write_dirs, over.write_dirs),
        allow_network: pick(base.allow_network, over.allow_network),
        max_memory_mb: pick(base.max_memory_mb, over.max_memory_mb),
        max_pids: pick(base.max_pids, over.max_pids),
        timeout_secs: pick(base.timeout_secs, over.timeout_secs),
    }
}

fn merge_sandbox_python(
    base: SandboxPythonConfigFile,
    over: SandboxPythonConfigFile,
) -> SandboxPythonConfigFile {
    fn pick<T>(base: Option<T>, over: Option<T>) -> Option<T> {
        over.or(base)
    }
    SandboxPythonConfigFile {
        wasm_path: pick(base.wasm_path, over.wasm_path),
        stdlib_dir: pick(base.stdlib_dir, over.stdlib_dir),
        timeout: pick(base.timeout, over.timeout),
        read_dirs: pick(base.read_dirs, over.read_dirs),
        write_dirs: pick(base.write_dirs, over.write_dirs),
        package_dirs: pick(base.package_dirs, over.package_dirs),
    }
}
