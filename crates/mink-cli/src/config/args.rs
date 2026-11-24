use super::*;

pub fn parse_args(args: Vec<String>) -> Result<CliConfig> {
    let mut cfg = CliConfig::default();
    let mut i = 0usize;
    let mut seen_prompt = false;
    let mut after_double_dash = false;

    while i < args.len() {
        let arg = &args[i];
        if after_double_dash {
            if seen_prompt {
                bail!(
                    "unexpected extra argument: {arg} (prompt already set to {:?})",
                    cfg.prompt
                );
            }
            cfg.prompt = arg.clone();
            seen_prompt = true;
            i += 1;
            continue;
        }
        if arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        let (flag, inline_val) =
            if arg.starts_with("--") && !matches!(arg.as_str(), "--tui=full" | "--tui=inline") {
                match arg.split_once('=') {
                    Some((name, val)) => (name, Some(val)),
                    None => (arg.as_str(), None),
                }
            } else {
                (arg.as_str(), None)
            };
        match flag {
            "-m" | "--model" => {
                let (val, step) = take_value(&args, i, inline_val)?;
                if val.trim().is_empty() {
                    bail!("model must not be empty");
                }
                cfg.provider.model = val;
                cfg.cli_overrides.model = true;
                i += step;
            }
            "--mission" => {
                let (val, step) = take_value(&args, i, inline_val)?;
                cfg.mission_file = Some(val.into());
                i += step;
            }
            "--api-key" => {
                let (val, step) = take_value(&args, i, inline_val)?;
                cfg.provider.api_key = val;
                cfg.cli_overrides.api_key = true;
                i += step;
            }
            "--base-url" => {
                let (val, step) = take_value(&args, i, inline_val)?;
                cfg.provider.base_url = val;
                cfg.cli_overrides.base_url = true;
                i += step;
            }
            "--print" if inline_val.is_none() => {
                cfg.output_format = OutputFormat::StreamJson;
                cfg.cli_overrides.output_format = true;
                i += 1;
            }
            "--session" => {
                let (value, step) = match inline_val {
                    Some(v) if v.trim().is_empty() => bail!("missing value for --session"),
                    Some(v) => (v.trim().to_string(), 1),
                    None => {
                        let value = require_value(&args, i)?;
                        if value.trim().is_empty() {
                            bail!("session name must not be empty");
                        }
                        (value, 2)
                    }
                };
                cfg.session_id = value;
                i += step;
            }
            "--continue" if inline_val.is_none() => {
                cfg.continue_session = true;
                i += 1;
            }
            "--list-sessions" if inline_val.is_none() => {
                cfg.list_sessions = true;
                i += 1;
            }
            "--list-skills" if inline_val.is_none() => {
                cfg.list_skills = true;
                i += 1;
            }
            "-v" | "--verbose" if inline_val.is_none() => {
                cfg.verbose = true;
                i += 1;
            }
            "--tui" if inline_val.is_none() => {
                cfg.tui_mode = TuiMode::Full;
                i += 1;
            }
            "--tui=full" => {
                cfg.tui_mode = TuiMode::Full;
                i += 1;
            }
            "--tui=inline" => {
                cfg.tui_mode = TuiMode::Inline;
                i += 1;
            }
            "-i" | "--interactive" if inline_val.is_none() => {
                cfg.interactive = true;
                i += 1;
            }
            "-h" | "--help" if inline_val.is_none() => {
                return Err(anyhow!(CliEarlyExit::Help));
            }
            "-V" | "--version" if inline_val.is_none() => {
                return Err(anyhow!(CliEarlyExit::Version));
            }
            "--agent-jsonl" if inline_val.is_none() => {
                cfg.agent_jsonl = true;
                cfg.output_format = OutputFormat::StreamJson;
                cfg.cli_overrides.output_format = true;
                i += 1;
            }
            "--skill" => {
                let (value, step) = take_value(&args, i, inline_val)?;
                if value.trim().is_empty() {
                    bail!("skill name must not be empty");
                }
                if !cfg.skills.iter().any(|s| s == &value) {
                    cfg.skills.push(value);
                }
                cfg.cli_overrides.skills = true;
                i += step;
            }
            "--enabled-tools" => {
                let (value, step) = take_value(&args, i, inline_val)?;
                cfg.tools.enabled_tools = Some(parse_enabled_tools(value)?);
                cfg.cli_overrides.enabled_tools = true;
                i += step;
            }
            "--edit-mode" => {
                let (val, step) = take_value(&args, i, inline_val)?;
                cfg.tools.edit_mode = EditMode::parse(&val)?;
                cfg.cli_overrides.edit_mode = true;
                i += step;
            }
            "--edit-fuzzy-match" => {
                let (val, step) = take_value(&args, i, inline_val)?;
                cfg.tools.edit_fuzzy_match = parse_bool_value("--edit-fuzzy-match", &val)?;
                cfg.cli_overrides.edit_fuzzy_match = true;
                i += step;
            }
            "--edit-fuzzy-threshold" => {
                let (val, step) = take_value(&args, i, inline_val)?;
                cfg.tools.edit_fuzzy_threshold = val.parse().map_err(|_| {
                    anyhow!("--edit-fuzzy-threshold requires a number in 0.0..=1.0")
                })?;
                cfg.cli_overrides.edit_fuzzy_threshold = true;
                i += step;
            }
            "--edit-enforce-seen-lines" => {
                let (val, step) = take_value(&args, i, inline_val)?;
                cfg.tools.edit_enforce_seen_lines =
                    parse_bool_value("--edit-enforce-seen-lines", &val)?;
                cfg.cli_overrides.edit_enforce_seen_lines = true;
                i += step;
            }
            "--config" => {
                let (toml_str, step) = take_value(&args, i, inline_val)?;
                cfg.cli_config = Some(mink::config_file::parse_config_file(&toml_str)?);
                i += step;
            }
            _ => {
                if arg.starts_with('-') {
                    bail!("unknown option: {arg}");
                }
                if seen_prompt {
                    bail!(
                        "unexpected extra argument: {arg} (prompt already set to {:?})",
                        cfg.prompt
                    );
                }
                cfg.prompt = arg.clone();
                seen_prompt = true;
                i += 1;
            }
        }
    }

    Ok(cfg)
}

fn take_value(args: &[String], i: usize, inline: Option<&str>) -> Result<(String, usize)> {
    match inline {
        Some("") => bail!("missing value for {}", args[i]),
        Some(val) => Ok((val.to_string(), 1)),
        None => Ok((require_value(args, i)?, 2)),
    }
}

fn require_value(args: &[String], i: usize) -> Result<String> {
    if i + 1 >= args.len() {
        bail!("missing value for {}", args[i]);
    }
    let value = args[i + 1].clone();
    if value.starts_with('-') && value.len() > 1 {
        bail!("missing value for {}", args[i]);
    }
    Ok(value)
}

fn parse_enabled_tools(value: String) -> Result<Vec<String>> {
    if value.trim().eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let tools = value
        .split(',')
        .map(str::trim)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if tools.is_empty() || tools.iter().any(String::is_empty) {
        bail!("--enabled-tools requires comma-separated tool names or 'none'");
    }
    Ok(tools)
}

pub(super) fn parse_bool_value(name: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => bail!("{name} requires true or false"),
    }
}

/// Parse `image_input = "on" | "off"` from TOML/env into the explicit
/// capability override (v7 §3.1 priority: explicit > backend > Unsupported).
pub fn parse_image_input(value: &str) -> Result<mink::runtime::ImageInputCapability> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" => Ok(mink::runtime::ImageInputCapability::OpenAiChatImageUrl(
            mink::runtime::OpenAiChatImageUrlLimits::default(),
        )),
        "off" | "false" | "0" => Ok(mink::runtime::ImageInputCapability::Unsupported),
        other => anyhow::bail!("invalid image_input {other:?}: expected \"on\" or \"off\""),
    }
}

/// Parse the `[provider.image]` TOML section into per-field overrides.
/// `Ok(None)` for an all-empty section; any invalid field fails the whole
/// section (callers warn and ignore).
pub fn parse_image_limits(
    file: &ImageConfigFile,
) -> Result<Option<mink::runtime::ImageLimitsOverrides>> {
    let detail = match file.detail.as_deref() {
        None => None,
        Some(value) => Some(match value.trim().to_ascii_lowercase().as_str() {
            "high" => mink::runtime::ImageDetail::High,
            "low" => mink::runtime::ImageDetail::Low,
            other => anyhow::bail!("invalid image.detail {other:?}: expected \"high\" or \"low\""),
        }),
    };
    let parse_u64 = |name: &str, value: &str| -> Result<u64> {
        parse_size_bytes(value)
            .map(|n| n as u64)
            .map_err(|_| anyhow!("invalid image.{name} {value:?}: expected a byte count"))
    };
    let max_image_bytes_per_request = match file.max_image_bytes_per_request.as_deref() {
        None => None,
        Some(value) => Some(parse_u64("max_image_bytes_per_request", value)?),
    };
    let max_image_bytes = match file.max_image_bytes.as_deref() {
        None => None,
        Some(value) => Some(parse_u64("max_image_bytes", value)?),
    };
    let max_pixels = match file.max_pixels.as_deref() {
        None => None,
        Some(value) => Some(parse_u64("max_pixels", value)?),
    };
    let overrides = mink::runtime::ImageLimitsOverrides {
        detail,
        max_images_per_request: file.max_images_per_request,
        max_image_bytes_per_request,
        max_image_bytes,
        max_dimension: file.max_dimension,
        max_pixels,
    };
    Ok((!overrides.is_empty()).then_some(overrides))
}

pub fn parse_size_bytes(raw: &str) -> Result<usize> {
    if raw.is_empty() {
        bail!("empty size");
    }
    let lower = raw.to_lowercase();
    let (num, m) = if let Some(v) = lower.strip_suffix('k') {
        (v, 1_000usize)
    } else if let Some(v) = lower.strip_suffix('m') {
        (v, 1_000_000usize)
    } else if let Some(v) = lower.strip_suffix('g') {
        (v, 1_000_000_000usize)
    } else {
        (lower.as_str(), 1usize)
    };
    num.parse::<usize>()?
        .checked_mul(m)
        .ok_or_else(|| anyhow!("size overflow: {raw}"))
}
