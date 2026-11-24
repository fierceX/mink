use super::*;

/// Server environment overrides (documented server precedence: env wins).
/// Mirrors the keys the CLI honours, applied on top of the file layers.
pub(crate) fn apply_env_overrides(cfg: &mut AgentConfig) {
    if let Ok(model) = std::env::var("MODEL")
        && !model.trim().is_empty()
    {
        cfg.model = Some(model);
    }
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY")
        && !key.trim().is_empty()
    {
        cfg.api_key = Some(key);
    }
    if let Ok(url) = std::env::var("DEEPSEEK_BASE_URL")
        && !url.trim().is_empty()
    {
        cfg.base_url = Some(url);
    }
    if let Ok(value) = std::env::var("MINK_IMAGE_INPUT") {
        match parse_image_input(&value) {
            Ok(capability) => cfg.image_input = Some(capability),
            Err(error) => {
                eprintln!("[mink-server] Warning: ignoring MINK_IMAGE_INPUT={value:?}: {error}")
            }
        }
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
    if let Ok(policy) = std::env::var("MINK_SIGNAL_POLICY")
        && let Ok(policy) = SignalPolicy::parse(&policy)
    {
        cfg.signal_policy = Some(policy);
    }
    if let Ok(value) = std::env::var("LOG_EVENTS")
        && let Ok(enabled) = parse_bool_env(&value)
    {
        cfg.log_events = Some(enabled);
    }
    let parse_usize_env = |name: &str| -> Option<usize> {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
    };
    if let Some(n) = parse_usize_env("TOOL_RESULT_MAX_BYTES") {
        cfg.tool_result_max_bytes = Some(n);
    }
    if let Some(n) = parse_usize_env("FILE_WRITE_MAX_BYTES") {
        cfg.file_write_max_bytes = Some(n);
    }
    if let Some(n) = parse_usize_env("MAX_SEARCH_FILES") {
        cfg.max_search_files = Some(n);
    }
    if let Some(n) = parse_usize_env("MAX_SEARCH_RESULTS") {
        cfg.max_search_results = Some(n);
    }
    if let Ok(value) = std::env::var("MINK_EDIT_MODE") {
        match EditMode::parse(&value) {
            Ok(mode) => cfg.edit_mode = Some(mode),
            Err(error) => {
                eprintln!("[mink-server] Warning: ignoring MINK_EDIT_MODE={value:?}: {error}")
            }
        }
    }
    if let Ok(value) = std::env::var("MINK_EDIT_FUZZY_MATCH")
        && let Ok(enabled) = parse_bool_env(&value)
    {
        cfg.edit_fuzzy_match = Some(enabled);
    }
    if let Ok(value) = std::env::var("MINK_EDIT_FUZZY_THRESHOLD")
        && let Ok(threshold) = value.parse::<f64>()
    {
        cfg.edit_fuzzy_threshold = Some(threshold);
    }
    if let Ok(value) = std::env::var("MINK_EDIT_ENFORCE_SEEN_LINES")
        && let Ok(enabled) = parse_bool_env(&value)
    {
        cfg.edit_enforce_seen_lines = Some(enabled);
    }
}

pub(super) fn parse_bool_env(value: &str) -> Result<bool, ()> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(()),
    }
}
