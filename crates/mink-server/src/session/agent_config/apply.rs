use super::*;

/// Apply the merged agent config to runtime options — mirrors the CLI's
/// `assemble_runtime_options` for the agent-relevant groups. Only fields
/// present in the config layer override the runtime defaults.
pub(crate) fn apply_to(mut options: AgentOptions, cfg: &AgentConfig) -> AgentOptions {
    // NOTE: `with_provider_options` replaces the resolved model/api_key/
    // base_url with `ProviderOptions` values, so those three are re-applied
    // by the caller AFTER this function (see `Registry::build_options`).
    let mut provider = ProviderOptions::default();
    if let Some(v) = &cfg.openai_reasoning_effort {
        provider.reasoning_effort = Some(v.clone());
    }
    if let Some(v) = cfg.openai_include_usage {
        provider.include_usage = v;
    }
    if let Some(v) = cfg.openai_token_param {
        provider.token_param = v;
    }
    if let Some(v) = &cfg.openai_tool_choice {
        provider.tool_choice = Some(v.clone());
    }
    if !cfg.openai_extra_body.is_empty() {
        provider.extra_body = cfg.openai_extra_body.clone();
    }
    if let Some(v) = cfg.http_timeout_secs {
        provider.http_timeout_secs = v;
    }
    options = options.with_provider_options(provider);
    for (alias, model) in &cfg.model_aliases {
        options = options.with_model_alias(alias, model);
    }

    let mut generation = GenerationOptions::default();
    if let Some(v) = cfg.max_tokens {
        generation.max_tokens = v;
    }
    if let Some(v) = cfg.max_turns {
        generation.max_turns = v;
    }
    if let Some(v) = cfg.llm_first_event_timeout_secs {
        generation.first_event_timeout_secs = v;
    }
    if let Some(v) = cfg.llm_idle_timeout_secs {
        generation.idle_timeout_secs = v;
    }
    if let Some(v) = cfg.llm_wait_heartbeat_secs {
        generation.wait_heartbeat_secs = v;
    }
    options = options.with_generation_options(generation);

    let mut context = ContextPolicy::default();
    if let Some(v) = cfg.max_context_tokens {
        context.max_context_tokens = v;
    }
    if let Some(v) = cfg.context_compact_pct {
        context.compact_pct = v;
    }
    if let Some(v) = cfg.context_reserve_tokens {
        context.reserve_tokens = v;
    }
    if let Some(v) = cfg.context_compact_tail_tokens {
        context.compact_tail_tokens = v;
    }
    if let Some(v) = cfg.context_compact_max_output_tokens {
        context.compact_max_output_tokens = v;
    }
    if let Some(v) = cfg.context_compact_input_reduction {
        context.compact_input_reduction = v;
    }
    options = options.with_context_policy(context);

    let mut tools = ToolOptions::default();
    if let Some(v) = cfg.tool_timeout_secs {
        tools.timeout_secs = v;
    }
    if let Some(v) = cfg.tool_timeout_max_secs {
        tools.timeout_max_secs = v;
    }
    if let Some(v) = cfg.sub_agent_timeout_secs {
        tools.sub_agent_timeout_secs = v;
    }
    if let Some(v) = cfg.sub_agent_max_depth {
        tools.sub_agent_max_depth = v;
    }
    if let Some(v) = cfg.edit_mode {
        tools.edit_mode = v;
    }
    if let Some(v) = cfg.edit_fuzzy_match {
        tools.edit_fuzzy_match = v;
    }
    if let Some(v) = cfg.edit_fuzzy_threshold {
        tools.edit_fuzzy_threshold = v;
    }
    if let Some(v) = cfg.edit_enforce_seen_lines {
        tools.edit_enforce_seen_lines = v;
    }
    if let Some(v) = cfg.tool_result_max_bytes {
        tools.result_max_bytes = v;
    }
    if let Some(v) = cfg.file_write_max_bytes {
        tools.file_write_max_bytes = v;
    }
    if let Some(v) = cfg.max_search_files {
        tools.max_search_files = v;
    }
    if let Some(v) = cfg.max_search_results {
        tools.max_search_results = v;
    }
    if let Some(v) = &cfg.enabled_tools {
        tools.enabled_tools = Some(v.clone());
    }
    if let Some(v) = cfg.approval_mode {
        tools.approval_mode = v;
    }
    if !cfg.approval.is_empty() {
        tools.approval = cfg.approval.clone();
    }
    options = options.with_tool_options(tools);

    if let Some(policy) = cfg.signal_policy {
        options = options.with_signal_policy(policy);
    }
    {
        let file = &cfg.recovery;
        let mut policy = mink::runtime::LlmRecoveryPolicy::default();
        let mut any = false;
        if let Some(v) = file.format_window_size {
            policy.format_window_size = v;
            any = true;
        }
        if let Some(v) = file.format_max_errors {
            policy.format_max_errors = v;
            any = true;
        }
        if let Some(v) = file.request_max_retries {
            policy.request_max_retries = v;
            any = true;
        }
        if let Some(v) = file.request_timeout_secs {
            policy.request_timeout_secs = Some(v);
            any = true;
        }
        if any {
            options = options.with_llm_recovery(policy);
        }
    }
    if let Some(enabled) = cfg.log_events {
        options = options.with_log_events(enabled);
    }
    if let Some(skills) = &cfg.skills {
        options = options.with_selected_skills(skills.iter().cloned());
    }
    options = apply_sandbox(options, &cfg.sandbox);
    options = apply_sandbox_python(options, &cfg.sandbox_python);
    // Image capability wiring (v7 §3.1), mirroring the CLI's
    // `assemble_runtime_options`: an explicit override beats the backend
    // declaration; an explicit vision-model list replaces the built-in
    // default list; limit overrides only adjust an already-supported
    // capability. Without this mapping the server freezes a text-only
    // capability and fails to reopen sessions created by the TUI/CLI.
    if let Some(capability) = cfg.image_input.clone() {
        options = options.with_image_input(capability);
    }
    if let Some(limits) = &cfg.image_limits {
        options = options.with_image_limits(limits.clone());
    }
    if let Some(models) = &cfg.vision_models {
        options = options.with_vision_models(models.clone());
    }
    options
}

pub(super) fn apply_sandbox(options: AgentOptions, file: &SandboxConfigFile) -> AgentOptions {
    let mut sandbox = SandboxConfig::default();
    if let Some(v) = file.enabled {
        sandbox.enabled = v;
    }
    if let Some(ref v) = file.backend {
        sandbox.backend = v.clone();
    }
    if let Some(ref v) = file.read_dirs {
        sandbox.read_dirs = v.clone();
    }
    if let Some(ref v) = file.write_dirs {
        sandbox.write_dirs = v.clone();
    }
    if let Some(v) = file.allow_network {
        sandbox.allow_network = v;
    }
    if let Some(v) = file.max_memory_mb {
        sandbox.max_memory_mb = v;
    }
    if let Some(v) = file.max_pids {
        sandbox.max_pids = v;
    }
    if let Some(v) = file.timeout_secs {
        sandbox.timeout_secs = v;
    }
    options.with_sandbox(sandbox)
}

pub(super) fn apply_sandbox_python(
    options: AgentOptions,
    file: &SandboxPythonConfigFile,
) -> AgentOptions {
    let mut sandbox_python = SandboxPythonConfig::default();
    if let Some(ref v) = file.wasm_path {
        sandbox_python.wasm_path = v.clone();
    }
    if let Some(ref v) = file.stdlib_dir {
        sandbox_python.stdlib_dir = v.clone();
    }
    if let Some(v) = file.timeout {
        sandbox_python.timeout = v;
    }
    if let Some(ref v) = file.read_dirs {
        sandbox_python.read_dirs = v.clone();
    }
    if let Some(ref v) = file.write_dirs {
        sandbox_python.write_dirs = v.clone();
    }
    if let Some(ref v) = file.package_dirs {
        sandbox_python.package_dirs = v.clone();
    }
    options.with_sandbox_python(sandbox_python)
}
