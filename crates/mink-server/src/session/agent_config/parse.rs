use super::*;

/// Parse a `.minkrc` file body into a resolved layer.
///
/// A malformed existing file is an error: silently replacing it with an empty
/// layer can remove an explicit tool restriction and restore the default tool
/// set. Only an absent file is treated as an empty layer by the loaders below.
pub(crate) fn parse_layer(text: &str, origin: &str) -> anyhow::Result<AgentConfig> {
    let file = mink::config_file::parse_config_file(text)
        .map_err(|error| anyhow::anyhow!("failed to parse {origin}: {error}"))?;
    Ok(from_file(&file))
}

/// Read the user-level layer from `$MINK_HOME/.minkrc` or `~/.minkrc`（与 TUI/CLI 同一配置文件）。
pub(crate) fn load_user_layer() -> anyhow::Result<AgentConfig> {
    let home = std::env::var("MINK_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|v| !v.is_empty()));
    let Some(home) = home else {
        return Ok(AgentConfig::default());
    };
    let path = Path::new(&home).join(".minkrc");
    load_layer(&path)
}

/// Read the project-level layer from `<cwd>/.minkrc`（CLI 同款项目级覆盖）。
pub(crate) fn load_project_layer(cwd: &Path) -> anyhow::Result<AgentConfig> {
    let path = cwd.join(".minkrc");
    load_layer(&path)
}

pub(super) fn load_layer(path: &Path) -> anyhow::Result<AgentConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_layer(&text, &path.display().to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AgentConfig::default()),
        Err(error) => Err(error)
            .map_err(|error| anyhow::anyhow!("failed to read {}: {error}", path.display())),
    }
}

pub(super) fn from_file(file: &MinkConfigFile) -> AgentConfig {
    let mut cfg = AgentConfig::default();
    let provider = &file.provider;
    cfg.model = provider.model.clone();
    if let Some(aliases) = &provider.model_aliases {
        cfg.model_aliases = aliases.clone();
    }
    cfg.api_key = provider.api_key.clone();
    cfg.base_url = provider.base_url.clone();
    cfg.openai_reasoning_effort = provider
        .openai_reasoning_effort
        .as_deref()
        .and_then(normalize_openai_reasoning_effort);
    cfg.openai_include_usage = provider.openai_include_usage;
    cfg.openai_token_param = provider
        .openai_token_param
        .as_deref()
        .and_then(TokenParamKind::parse);
    cfg.openai_tool_choice = provider.openai_tool_choice.clone();
    if let Some(extra_body) = &provider.openai_extra_body {
        cfg.openai_extra_body = extra_body.clone();
    }
    if let Some(http_timeout_secs) = provider.http_timeout_secs {
        cfg.http_timeout_secs = Some(http_timeout_secs);
    }
    if let Some(value) = &provider.image_input {
        match parse_image_input(value) {
            Ok(capability) => cfg.image_input = Some(capability),
            Err(error) => {
                eprintln!("[mink-server] Warning: ignoring image_input={value:?}: {error}")
            }
        }
    }
    if let Some(models) = &provider.vision_models {
        cfg.vision_models = Some(models.clone());
    }
    // `[provider.image]` overrides: higher layers win per field; an invalid
    // section is warned and ignored entirely (same stance as the CLI).
    match parse_image_limits(&provider.image) {
        Ok(Some(overrides)) => match &mut cfg.image_limits {
            Some(existing) => existing.merge(overrides),
            None => cfg.image_limits = Some(overrides),
        },
        Ok(None) => {}
        Err(error) => eprintln!("[mink-server] Warning: ignoring [provider.image]: {error}"),
    }
    cfg.max_tokens = positive(file.generation.max_tokens, "max_tokens");
    cfg.max_turns = positive(file.generation.max_turns, "max_turns");
    cfg.llm_first_event_timeout_secs = positive(
        file.generation.llm_first_event_timeout,
        "llm_first_event_timeout",
    );
    cfg.llm_idle_timeout_secs = positive(file.generation.llm_idle_timeout, "llm_idle_timeout");
    cfg.llm_wait_heartbeat_secs =
        nonnegative(file.generation.llm_wait_heartbeat, "llm_wait_heartbeat");
    cfg.log_events = file.generation.log_events;
    if let Some(ref max_context) = file.context.max_context
        && let Ok(v) = parse_size_bytes(max_context)
    {
        cfg.max_context_tokens = Some(v);
    }
    if let Some(pct) = file.context.context_compact_pct {
        if (1..=100).contains(&pct) {
            cfg.context_compact_pct = Some(pct);
        } else {
            eprintln!("[mink-server] Warning: ignoring context_compact_pct={pct}; expected 1-100");
        }
    }
    cfg.context_reserve_tokens = file.context.context_reserve_tokens.filter(|v| *v > 0);
    cfg.context_compact_tail_tokens = file.context.context_compact_tail_tokens.filter(|v| *v > 0);
    cfg.context_compact_max_output_tokens = positive(
        file.context.context_compact_max_output_tokens,
        "context_compact_max_output_tokens",
    );
    cfg.context_compact_input_reduction = file.context.context_compact_input_reduction;

    let tools = &file.tools;
    cfg.tool_timeout_secs = positive(tools.tool_timeout, "tool_timeout");
    cfg.tool_timeout_max_secs = tools.tool_timeout_max.filter(|v| {
        if *v >= MIN_TOOL_TIMEOUT_MAX_SECS {
            true
        } else {
            eprintln!(
                "[mink-server] Warning: ignoring tool_timeout_max={v}; must be at least {MIN_TOOL_TIMEOUT_MAX_SECS}"
            );
            false
        }
    });
    cfg.sub_agent_timeout_secs = tools.sub_agent_timeout;
    cfg.sub_agent_max_depth = tools.sub_agent_max_depth;
    cfg.max_search_files = tools.max_search_files;
    cfg.max_search_results = tools.max_search_results;
    cfg.enabled_tools = tools.enabled_tools.clone();
    cfg.approval_mode = tools.approval_mode;
    if let Some(approval) = &tools.approval {
        cfg.approval = approval.clone();
    }
    cfg.skills = tools.skills.clone();
    cfg.edit_mode = tools.edit.mode;
    cfg.edit_fuzzy_match = tools.edit.fuzzy_match;
    cfg.edit_fuzzy_threshold = tools.edit.fuzzy_threshold;
    cfg.edit_enforce_seen_lines = tools.edit.enforce_seen_lines;
    cfg.signal_policy = file.signal.policy;
    cfg.recovery = file.recovery.clone();
    cfg.sandbox = file.sandbox.clone();
    cfg.sandbox_python = file.sandbox_python.clone();
    cfg.ptc = file.ptc.clone();
    cfg.web = file.web.clone();
    cfg
}

pub(super) fn positive(value: Option<i32>, name: &str) -> Option<i32> {
    value.filter(|v| {
        if *v > 0 {
            true
        } else {
            eprintln!("[mink-server] Warning: ignoring {name}={v}; expected > 0");
            false
        }
    })
}

pub(super) fn nonnegative(value: Option<i32>, name: &str) -> Option<i32> {
    value.filter(|v| {
        if *v >= 0 {
            true
        } else {
            eprintln!("[mink-server] Warning: ignoring {name}={v}; expected >= 0");
            false
        }
    })
}
