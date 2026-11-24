use super::*;

pub fn validate_runtime_config(cfg: &CliConfig) -> Result<()> {
    mink::runtime::validate_runtime_limits(
        cfg.tools.edit_fuzzy_threshold,
        cfg.generation.max_tokens,
        cfg.generation.max_turns,
        cfg.context.compact_pct,
        cfg.context.reserve_tokens,
        cfg.context.compact_tail_tokens,
        cfg.context.compact_max_output_tokens,
        cfg.context.max_context_tokens,
    )?;
    if cfg.tools.timeout_max_secs < 5 {
        bail!("tool_timeout_max_secs must be at least 5 seconds");
    }
    if cfg.tools.sub_agent_timeout_secs <= 0 {
        bail!("sub_agent_timeout must be greater than 0");
    }
    if cfg.tools.sub_agent_max_depth > 3 {
        bail!("sub_agent_max_depth must be between 0 and 3");
    }
    cfg.llm_recovery.validate()?;
    Ok(())
}
