use super::args::parse_bool_value;
use super::*;

pub(crate) fn default_home() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("MINK_HOME")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| String::from(".")),
    )
}

pub fn apply_config_file(cfg: &mut CliConfig) -> Result<()> {
    let defaults = CliConfig::default();
    apply_env_defaults(cfg, &defaults)?;
    // SDK 协议模式：所有配置已通过 --config TOML 传入，跳过文件 I/O
    if cfg.agent_jsonl {
        let cli_cfg = cfg.cli_config.take();
        apply_config_sources(cfg, &defaults, None, None, cli_cfg.as_ref());
        apply_sandbox_config(cfg, None, None, cli_cfg.as_ref());
        cfg.cli_config = cli_cfg;
        return Ok(());
    }
    // Priority: CLI > project .minkrc > user ~/.minkrc > env > default.
    // CLI is inferred by comparing the already-parsed config to defaults.
    let cwd = std::env::current_dir().unwrap_or_default();
    let home = default_home();

    let user_cfg = read_config_file(&home.join(".minkrc"))?;
    let project_cfg = read_config_file(&cwd.join(".minkrc"))?;
    let cli_cfg = cfg.cli_config.take();
    apply_config_sources(
        cfg,
        &defaults,
        user_cfg.as_ref(),
        project_cfg.as_ref(),
        cli_cfg.as_ref(),
    );
    apply_sandbox_config(
        cfg,
        user_cfg.as_ref(),
        project_cfg.as_ref(),
        cli_cfg.as_ref(),
    );
    cfg.cli_config = cli_cfg;
    Ok(())
}

pub(super) fn apply_env_defaults(cfg: &mut CliConfig, defaults: &CliConfig) -> Result<()> {
    if let Ok(value) = std::env::var("MINK_SIGNAL_POLICY") {
        cfg.signal_policy = SignalPolicy::parse(&value)?;
    }
    if let Ok(value) = std::env::var("MINK_IMAGE_INPUT") {
        cfg.image_input = Some(parse_image_input(&value)?);
    }
    if let Ok(value) = std::env::var("MINK_VISION_MODELS") {
        cfg.vision_models = Some(
            value
                .split(',')
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
                .collect(),
        );
    }
    if cfg.log_events == defaults.log_events
        && let Ok(v) = std::env::var("LOG_EVENTS")
    {
        apply_log_events_env_value(cfg, v.as_str());
    }
    // Size-limit env vars sit at the env layer: applied before config files
    // so [tools] settings in .minkrc/--config outrank them (documented
    // priority: CLI > --config > project .minkrc > user .minkrc > env).
    let parse_env = |name: &str| -> Option<usize> {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
    };
    apply_size_limit_envs(
        cfg,
        parse_env("TOOL_RESULT_MAX_BYTES"),
        parse_env("FILE_WRITE_MAX_BYTES"),
        parse_env("MAX_SEARCH_FILES"),
        parse_env("MAX_SEARCH_RESULTS"),
    );
    if !cfg.cli_overrides.edit_mode
        && let Ok(value) = std::env::var("MINK_EDIT_MODE")
    {
        cfg.tools.edit_mode = EditMode::parse(&value)?;
    }
    if !cfg.cli_overrides.edit_fuzzy_match
        && let Ok(value) = std::env::var("MINK_EDIT_FUZZY_MATCH")
    {
        cfg.tools.edit_fuzzy_match = parse_bool_value("MINK_EDIT_FUZZY_MATCH", &value)?;
    }
    if !cfg.cli_overrides.edit_fuzzy_threshold
        && let Ok(value) = std::env::var("MINK_EDIT_FUZZY_THRESHOLD")
    {
        cfg.tools.edit_fuzzy_threshold = value
            .parse()
            .map_err(|_| anyhow!("MINK_EDIT_FUZZY_THRESHOLD requires a number in 0.0..=1.0"))?;
    }
    if !cfg.cli_overrides.edit_enforce_seen_lines
        && let Ok(value) = std::env::var("MINK_EDIT_ENFORCE_SEEN_LINES")
    {
        cfg.tools.edit_enforce_seen_lines =
            parse_bool_value("MINK_EDIT_ENFORCE_SEEN_LINES", &value)?;
    }
    if !cfg.cli_overrides.ptc_mode
        && let Ok(value) = std::env::var("MINK_PTC_MODE")
    {
        cfg.ptc.mode = PtcMode::parse(&value)?;
    }
    Ok(())
}

pub(super) fn apply_size_limit_envs(
    cfg: &mut CliConfig,
    tool_result_max_bytes: Option<usize>,
    file_write_max_bytes: Option<usize>,
    max_search_files: Option<usize>,
    max_search_results: Option<usize>,
) {
    if let Some(n) = tool_result_max_bytes {
        cfg.tools.result_max_bytes = n;
    }
    if let Some(n) = file_write_max_bytes {
        cfg.tools.file_write_max_bytes = n;
    }
    if let Some(n) = max_search_files {
        cfg.tools.max_search_files = n;
    }
    if let Some(n) = max_search_results {
        cfg.tools.max_search_results = n;
    }
}

pub(super) fn apply_log_events_env_value(cfg: &mut CliConfig, value: &str) {
    cfg.log_events = value != "0" && value != "false" && value != "no";
}

pub(super) fn read_config_file(path: &std::path::Path) -> Result<Option<MinkConfigFile>> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "[mink] Warning: failed to read config file {}: {}",
                    path.display(),
                    e
                );
            }
            return if e.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(e.into())
            };
        }
    };
    mink::config_file::parse_config_file(&data)
        .map(Some)
        .map_err(|e| anyhow!("failed to parse config file {}: {e}", path.display()))
}

pub(super) fn apply_config_sources(
    cfg: &mut CliConfig,
    _defaults: &CliConfig,
    user_cfg: Option<&MinkConfigFile>,
    project_cfg: Option<&MinkConfigFile>,
    cli_cfg: Option<&MinkConfigFile>,
) {
    // CLI-provided flags outrank every file layer; snapshot the flags once so
    // the loop below can mutate `cfg` freely.
    let cli = cfg.cli_overrides.clone();

    for toml_cfg in [user_cfg, project_cfg, cli_cfg].into_iter().flatten() {
        if let Some(value) = toml_cfg.recovery.format_window_size {
            cfg.llm_recovery.format_window_size = value;
        }
        if let Some(value) = toml_cfg.recovery.format_max_errors {
            cfg.llm_recovery.format_max_errors = value;
        }
        if let Some(value) = toml_cfg.recovery.request_max_retries {
            cfg.llm_recovery.request_max_retries = value;
        }
        if let Some(value) = toml_cfg.recovery.request_timeout_secs {
            cfg.llm_recovery.request_timeout_secs = Some(value);
        }
        if !cli.model
            && let Some(model) = &toml_cfg.provider.model
        {
            cfg.provider.model = model.clone();
        }
        if let Some(model_aliases) = &toml_cfg.provider.model_aliases {
            for (alias, model) in model_aliases {
                cfg.provider
                    .model_aliases
                    .insert(alias.clone(), model.clone());
            }
        }
        if let Some(reasoning_effort) = &toml_cfg.provider.openai_reasoning_effort {
            cfg.provider.reasoning_effort =
                mink::config_file::normalize_openai_reasoning_effort(reasoning_effort);
        }
        if let Some(include_usage) = toml_cfg.provider.openai_include_usage {
            cfg.provider.include_usage = include_usage;
        }
        if let Some(token_param) = &toml_cfg.provider.openai_token_param {
            if let Some(parsed) = TokenParamKind::parse(token_param) {
                cfg.provider.token_param = parsed;
            } else {
                eprintln!("[mink] Warning: ignoring openai_token_param={token_param:?}");
            }
        }
        if let Some(tool_choice) = &toml_cfg.provider.openai_tool_choice {
            cfg.provider.tool_choice = Some(tool_choice.clone());
        }
        if let Some(value) = &toml_cfg.provider.image_input {
            match parse_image_input(value) {
                Ok(capability) => cfg.image_input = Some(capability),
                Err(error) => eprintln!("[mink] Warning: ignoring image_input={value:?}: {error}"),
            }
        }
        if let Some(models) = &toml_cfg.provider.vision_models {
            cfg.vision_models = Some(models.clone());
        }
        // `[provider.image]` overrides: higher layers win per field; an
        // invalid section is warned and ignored entirely (same stance as
        // invalid image_input).
        match parse_image_limits(&toml_cfg.provider.image) {
            Ok(Some(overrides)) => match &mut cfg.image_limits {
                Some(existing) => existing.merge(overrides),
                None => cfg.image_limits = Some(overrides),
            },
            Ok(None) => {}
            Err(error) => eprintln!("[mink] Warning: ignoring [provider.image]: {error}"),
        }
        if let Some(extra_body) = &toml_cfg.provider.openai_extra_body {
            cfg.provider.extra_body.extend(extra_body.clone());
        }
        if let Some(http_timeout_secs) = toml_cfg.provider.http_timeout_secs {
            cfg.provider.http_timeout_secs = http_timeout_secs;
        }
        if !cli.api_key
            && let Some(api_key) = &toml_cfg.provider.api_key
        {
            cfg.provider.api_key = api_key.clone();
        }
        if !cli.base_url
            && let Some(base_url) = &toml_cfg.provider.base_url
        {
            cfg.provider.base_url = base_url.clone();
        }
        if let Some(max_tokens) = toml_cfg.generation.max_tokens {
            cfg.generation.max_tokens = max_tokens;
        }
        if let Some(max_turns) = toml_cfg.generation.max_turns {
            cfg.generation.max_turns = max_turns;
        }
        if let Some(max_context) = &toml_cfg.context.max_context
            && let Ok(v) = parse_size_bytes(max_context)
        {
            cfg.context.max_context_tokens = v;
        }
        if let Some(v) = toml_cfg.tools.max_search_files {
            cfg.tools.max_search_files = v;
        }
        if let Some(v) = toml_cfg.tools.max_search_results {
            cfg.tools.max_search_results = v;
        }
        if let Some(tool_timeout) = toml_cfg.tools.tool_timeout {
            apply_positive_i32_config(&mut cfg.tools.timeout_secs, tool_timeout, "tool_timeout");
        }
        if let Some(tool_timeout_max) = toml_cfg.tools.tool_timeout_max {
            apply_tool_timeout_max_config(
                &mut cfg.tools.timeout_max_secs,
                tool_timeout_max,
                "tool_timeout_max",
            );
        }
        if let Some(sub_agent_timeout) = toml_cfg.tools.sub_agent_timeout {
            cfg.tools.sub_agent_timeout_secs = sub_agent_timeout;
        }
        if let Some(depth) = toml_cfg.tools.sub_agent_max_depth {
            cfg.tools.sub_agent_max_depth = depth;
        }
        if let Some(timeout) = toml_cfg.generation.llm_first_event_timeout {
            apply_positive_i32_config(
                &mut cfg.generation.first_event_timeout_secs,
                timeout,
                "llm_first_event_timeout",
            );
        }
        if let Some(timeout) = toml_cfg.generation.llm_idle_timeout {
            apply_positive_i32_config(
                &mut cfg.generation.idle_timeout_secs,
                timeout,
                "llm_idle_timeout",
            );
        }
        if let Some(timeout) = toml_cfg.generation.llm_wait_heartbeat {
            apply_nonnegative_i32_config(
                &mut cfg.generation.wait_heartbeat_secs,
                timeout,
                "llm_wait_heartbeat",
            );
        }
        if let Some(context_compact_pct) = toml_cfg.context.context_compact_pct {
            if (1..=100).contains(&context_compact_pct) {
                cfg.context.compact_pct = context_compact_pct;
            } else {
                eprintln!(
                    "[mink] Warning: ignoring context_compact_pct={context_compact_pct}; expected 1-100"
                );
            }
        }
        if let Some(tokens) = toml_cfg.context.context_reserve_tokens {
            if tokens > 0 {
                cfg.context.reserve_tokens = tokens;
            } else {
                eprintln!("[mink] Warning: ignoring context_reserve_tokens=0");
            }
        }
        if let Some(tokens) = toml_cfg.context.context_compact_tail_tokens {
            if tokens > 0 {
                cfg.context.compact_tail_tokens = tokens;
            } else {
                eprintln!("[mink] Warning: ignoring context_compact_tail_tokens=0");
            }
        }
        if let Some(tokens) = toml_cfg.context.context_compact_max_output_tokens {
            apply_positive_i32_config(
                &mut cfg.context.compact_max_output_tokens,
                tokens,
                "context_compact_max_output_tokens",
            );
        }
        if let Some(enabled) = toml_cfg.context.context_compact_input_reduction {
            cfg.context.compact_input_reduction = enabled;
        }
        if let Some(log_events) = toml_cfg.generation.log_events {
            cfg.log_events = log_events;
        }
        let tools = &toml_cfg.tools;
        if let Some(mode) = tools.approval_mode {
            cfg.tools.approval_mode = mode;
        }
        if let Some(approval) = &tools.approval {
            for (name, policy) in approval {
                cfg.tools.approval.insert(name.clone(), *policy);
            }
        }
        if !cli.output_format
            && let Some(ref v) = toml_cfg.generation.output_format
        {
            match v.as_str() {
                "human" => cfg.output_format = OutputFormat::Human,
                "stream-json" => cfg.output_format = OutputFormat::StreamJson,
                _ => {}
            }
        }
        if !cli.skills
            && let Some(ref v) = toml_cfg.tools.skills
        {
            cfg.skills = v.clone();
        }
        if !cli.enabled_tools
            && let Some(ref v) = toml_cfg.tools.enabled_tools
        {
            cfg.tools.enabled_tools = Some(v.clone());
        }
        if !cli.edit_mode
            && let Some(mode) = toml_cfg.tools.edit.mode
        {
            cfg.tools.edit_mode = mode;
        }
        if !cli.edit_fuzzy_match
            && let Some(enabled) = toml_cfg.tools.edit.fuzzy_match
        {
            cfg.tools.edit_fuzzy_match = enabled;
        }
        if !cli.edit_fuzzy_threshold
            && let Some(threshold) = toml_cfg.tools.edit.fuzzy_threshold
        {
            cfg.tools.edit_fuzzy_threshold = threshold;
        }
        if !cli.edit_enforce_seen_lines
            && let Some(enabled) = toml_cfg.tools.edit.enforce_seen_lines
        {
            cfg.tools.edit_enforce_seen_lines = enabled;
        }
        if let Some(policy) = toml_cfg.signal.policy {
            cfg.signal_policy = policy;
        }
        apply_ptc_file(&mut cfg.ptc, &toml_cfg.ptc, cli.ptc_mode);
    }
}

fn apply_ptc_file(
    ptc: &mut PtcConfig,
    file: &mink::config_file::PtcConfigFile,
    cli_mode_override: bool,
) {
    if !cli_mode_override && let Some(ref mode_str) = file.mode {
        match PtcMode::parse(mode_str) {
            Ok(mode) => ptc.mode = mode,
            Err(error) => eprintln!("[mink] Warning: ignoring [ptc].mode={mode_str:?}: {error}"),
        }
    }
    if let Some(ref backend_str) = file.backend {
        match PtcBackendKind::parse(backend_str) {
            Ok(backend) => ptc.backend = backend,
            Err(error) => {
                eprintln!("[mink] Warning: ignoring [ptc].backend={backend_str:?}: {error}")
            }
        }
    }
    if let Some(ref executable) = file.python_executable {
        ptc.python_executable = Some(executable.clone());
    }
    if let Some(v) = file.timeout_secs {
        ptc.timeout_secs = v;
    }
    if let Some(v) = file.max_timeout_secs {
        ptc.max_timeout_secs = v;
    }
    if let Some(v) = file.max_parallel_sub_calls {
        ptc.max_parallel_sub_calls = v;
    }
    if let Some(v) = file.max_sub_calls_per_minute {
        ptc.max_sub_calls_per_minute = v;
    }
    if let Some(v) = file.max_output_bytes {
        ptc.max_output_bytes = v;
    }
    if let Some(v) = file.max_message_bytes {
        ptc.max_message_bytes = v;
    }
    if let Some(v) = file.max_pending_calls {
        ptc.max_pending_calls = v;
    }
    if let Some(v) = file.stop_grace_ms {
        ptc.stop_grace_ms = v;
    }
}

fn apply_positive_i32_config(target: &mut i32, value: i32, name: &str) {
    if value > 0 {
        *target = value;
    } else {
        eprintln!("[mink] Warning: ignoring {name}={value}; must be greater than 0");
    }
}

fn apply_tool_timeout_max_config(target: &mut i32, value: i32, name: &str) {
    if value >= mink::config_file::MIN_TOOL_TIMEOUT_MAX_SECS {
        *target = value;
    } else {
        eprintln!(
            "[mink] Warning: ignoring {name}={value}; must be at least {}",
            mink::config_file::MIN_TOOL_TIMEOUT_MAX_SECS
        );
    }
}

fn apply_nonnegative_i32_config(target: &mut i32, value: i32, name: &str) {
    if value >= 0 {
        *target = value;
    } else {
        eprintln!("[mink] Warning: ignoring {name}={value}; must be zero or greater");
    }
}

/// Apply sandbox config from TOML `[sandbox]` sections.
/// Project-level overrides user-level; both override defaults.
/// Only active when `sandbox.enabled = true` in the highest-priority config.
pub(super) fn apply_sandbox_config(
    cfg: &mut CliConfig,
    user_cfg: Option<&MinkConfigFile>,
    project_cfg: Option<&MinkConfigFile>,
    cli_cfg: Option<&MinkConfigFile>,
) {
    for toml_cfg in [user_cfg, project_cfg, cli_cfg].into_iter().flatten() {
        let sb = &toml_cfg.sandbox;
        if let Some(v) = sb.enabled {
            cfg.sandbox.enabled = v;
        }
        if let Some(ref v) = sb.backend {
            cfg.sandbox.backend = v.clone();
        }
        if let Some(ref v) = sb.read_dirs {
            cfg.sandbox.read_dirs = v.clone();
        }
        if let Some(ref v) = sb.write_dirs {
            cfg.sandbox.write_dirs = v.clone();
        }
        if let Some(v) = sb.allow_network {
            cfg.sandbox.allow_network = v;
        }
        if let Some(v) = sb.max_memory_mb {
            cfg.sandbox.max_memory_mb = v;
        }
        if let Some(v) = sb.max_pids {
            cfg.sandbox.max_pids = v;
        }
        if let Some(v) = sb.timeout_secs {
            cfg.sandbox.timeout_secs = v;
        }
        let sp = &toml_cfg.sandbox_python;
        if let Some(ref v) = sp.wasm_path {
            cfg.sandbox_python.wasm_path = v.clone();
        }
        if let Some(ref v) = sp.stdlib_dir {
            cfg.sandbox_python.stdlib_dir = v.clone();
        }
        if let Some(v) = sp.timeout {
            cfg.sandbox_python.timeout = v;
        }
        if let Some(ref v) = sp.read_dirs {
            cfg.sandbox_python.read_dirs = v.clone();
        }
        if let Some(ref v) = sp.write_dirs {
            cfg.sandbox_python.write_dirs = v.clone();
        }
        if let Some(ref v) = sp.package_dirs {
            cfg.sandbox_python.package_dirs = v.clone();
        }
    }

    // Also check MINK_LIMITS env var (JSON format) — highest priority after CLI
    if let Ok(json) = std::env::var("MINK_LIMITS")
        && let Ok(sb) = serde_json::from_str::<SandboxConfig>(&json)
        && sb.enabled
    {
        cfg.sandbox = sb;
    }
}

pub fn apply_provider_defaults(cfg: &mut CliConfig) -> Result<()> {
    // API key: CLI/config > DEEPSEEK_API_KEY
    if cfg.provider.api_key.is_empty() {
        cfg.provider.api_key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_default();
    }
    // Base URL: DEEPSEEK_BASE_URL > CLI flag > default
    if cfg.provider.base_url.is_empty() {
        cfg.provider.base_url = std::env::var("DEEPSEEK_BASE_URL").unwrap_or_default();
    }
    // Default model tier
    if cfg.provider.model.is_empty() {
        cfg.provider.model = "flash".to_string();
    }
    if cfg.provider.api_key.is_empty() && cfg.provider.base_url.is_empty() {
        bail!("no API key. Set DEEPSEEK_API_KEY or use --api-key");
    }
    Ok(())
}

pub(crate) fn apply_sdk_request_options(
    cfg: &mut CliConfig,
    request: &mink::sdk_protocol::SdkRequest,
) {
    let options = &request.options;
    if let Some(model) = &options.provider.model {
        cfg.provider.model = model.clone();
    }
    if let Some(http_timeout_secs) = options.provider.http_timeout_secs {
        cfg.provider.http_timeout_secs = http_timeout_secs;
    }
    let generation = &options.generation;
    for (value, target) in [
        (generation.max_tokens, &mut cfg.generation.max_tokens),
        (generation.max_turns, &mut cfg.generation.max_turns),
        (
            generation.llm_first_event_timeout,
            &mut cfg.generation.first_event_timeout_secs,
        ),
        (
            generation.llm_idle_timeout,
            &mut cfg.generation.idle_timeout_secs,
        ),
        (
            generation.llm_wait_heartbeat,
            &mut cfg.generation.wait_heartbeat_secs,
        ),
    ] {
        if let Some(value) = value {
            *target = value;
        }
    }
    let context = &options.context;
    if let Some(value) = context.max_context {
        cfg.context.max_context_tokens = value;
    }
    if let Some(value) = context.context_compact_pct {
        cfg.context.compact_pct = value;
    }
    if let Some(value) = context.context_reserve_tokens {
        cfg.context.reserve_tokens = value;
    }
    if let Some(value) = context.context_compact_tail_tokens {
        cfg.context.compact_tail_tokens = value;
    }
    if let Some(value) = context.context_compact_max_output_tokens {
        cfg.context.compact_max_output_tokens = value;
    }
    if let Some(value) = context.context_compact_input_reduction {
        cfg.context.compact_input_reduction = value;
    }
    let tools = &options.tools;
    if let Some(value) = tools.tool_timeout {
        cfg.tools.timeout_secs = value;
    }
    if let Some(value) = tools.tool_timeout_max {
        cfg.tools.timeout_max_secs = value;
    }
    if let Some(value) = tools.sub_agent_timeout {
        cfg.tools.sub_agent_timeout_secs = value;
    }
    if let Some(value) = tools.sub_agent_max_depth {
        cfg.tools.sub_agent_max_depth = value;
    }
    if let Some(value) = &tools.enabled_tools {
        cfg.tools.enabled_tools = Some(value.clone());
    }
    if let Some(value) = tools.edit_mode {
        cfg.tools.edit_mode = match value {
            mink::runtime::EditMode::Hashline => EditMode::Hashline,
            mink::runtime::EditMode::Replace => EditMode::Replace,
        };
    }
    if let Some(value) = tools.edit_fuzzy_match {
        cfg.tools.edit_fuzzy_match = value;
    }
    if let Some(value) = tools.edit_fuzzy_threshold {
        cfg.tools.edit_fuzzy_threshold = value;
    }
    if let Some(value) = tools.edit_enforce_seen_lines {
        cfg.tools.edit_enforce_seen_lines = value;
    }
    if !cfg.cli_overrides.skills
        && let Some(value) = &tools.skills
    {
        cfg.skills = value.clone();
    }
    if let Some(value) = options.output.verbose {
        cfg.verbose = value;
    }
    if options.output.stream_events == Some(false) {
        cfg.output_format = OutputFormat::Human;
    }
    if let Some(value) = request
        .session_id
        .as_deref()
        .or(options.session.session_id.as_deref())
        .filter(|value| !value.is_empty())
    {
        cfg.session_id = value.to_string();
    }
    if let Some(value) = &request.mission {
        cfg.mission_content = Some(value.clone());
    }
    if let Some(value) = options.signal.policy {
        cfg.signal_policy = match value {
            mink::runtime::SignalPolicy::Off => SignalPolicy::Off,
            mink::runtime::SignalPolicy::Evidence => SignalPolicy::Evidence,
            mink::runtime::SignalPolicy::StateOps => SignalPolicy::StateOps,
            mink::runtime::SignalPolicy::Restart => SignalPolicy::Restart,
            mink::runtime::SignalPolicy::Full => SignalPolicy::Full,
        };
    }
    let recovery = &options.recovery;
    if let Some(value) = recovery.format_window_size {
        cfg.llm_recovery.format_window_size = value;
    }
    if let Some(value) = recovery.format_max_errors {
        cfg.llm_recovery.format_max_errors = value;
    }
    if let Some(value) = recovery.request_max_retries {
        cfg.llm_recovery.request_max_retries = value;
    }
    if let Some(value) = recovery.request_timeout_secs {
        cfg.llm_recovery.request_timeout_secs = Some(value);
    }
    let ptc = &options.ptc;
    if !cfg.cli_overrides.ptc_mode
        && let Some(mode) = ptc.mode
    {
        cfg.ptc.mode = mode;
    }
    if let Some(backend) = ptc.backend {
        cfg.ptc.backend = backend;
    }
    if let Some(ref exe) = ptc.python_executable {
        cfg.ptc.python_executable = Some(exe.clone());
    }
    if let Some(v) = ptc.timeout_secs {
        cfg.ptc.timeout_secs = v;
    }
    if let Some(v) = ptc.max_timeout_secs {
        cfg.ptc.max_timeout_secs = v;
    }
    if let Some(v) = ptc.max_parallel_sub_calls {
        cfg.ptc.max_parallel_sub_calls = v;
    }
    if let Some(v) = ptc.max_sub_calls_per_minute {
        cfg.ptc.max_sub_calls_per_minute = v;
    }
    if let Some(v) = ptc.max_output_bytes {
        cfg.ptc.max_output_bytes = v;
    }
    if let Some(v) = ptc.max_message_bytes {
        cfg.ptc.max_message_bytes = v;
    }
    if let Some(v) = ptc.max_pending_calls {
        cfg.ptc.max_pending_calls = v;
    }
    if let Some(v) = ptc.stop_grace_ms {
        cfg.ptc.stop_grace_ms = v;
    }
}
