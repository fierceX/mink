//! Pure cut/estimation/instruction helpers for compaction.
//!
//! Moved out of `compaction.rs` (Q15): these functions operate on messages,
//! triggers and config only; the engine keeps the stateful logic.

use super::compaction::{
    COMPACTION_INSTRUCTION, COMPACTION_MIN_TAIL_USER_MESSAGES, RUNTIME_INJECTED_MARKERS,
};
use crate::config::ResolvedConfig as Config;
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

pub(crate) fn compacted_summary_message(summary: &str) -> Value {
    json!({
        "role": "user",
        "internal": true,
        "content": format!(
            "This is an automatically generated checkpoint condensing an earlier span of the conversation. Treat it as established background and continue from the messages that follow without acknowledging the checkpoint.\n\n<compacted-summary>\n{}\n</compacted-summary>",
            summary.trim()
        ),
    })
}

pub(crate) fn compaction_instruction_message() -> Value {
    json!({
        "role": "user",
        "internal": true,
        "content": COMPACTION_INSTRUCTION,
    })
}

pub(crate) fn read_active_plan_checkpoint(summary_path: &Path) -> Result<Option<Value>> {
    let plan_path = summary_path.with_file_name("plan.md");
    let content = match std::fs::read_to_string(&plan_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "cannot read active plan checkpoint {}: {error}",
                plan_path.display()
            ));
        }
    };
    let content = content.trim();
    Ok((!content.is_empty()).then(|| {
        json!({
            "role": "user",
            "internal": true,
            "content": format!("<active-plan-checkpoint>\n{content}\n</active-plan-checkpoint>"),
        })
    }))
}

pub(crate) fn summary_input_over_budget(
    config: &Config,
    messages: &[Value],
    tools: &[Value],
    system_prompt: &str,
) -> Result<bool> {
    if config.max_context_tokens == 0 {
        return Ok(false);
    }
    let input_tokens =
        crate::llm::transport::estimate_openai_context_tokens(messages, tools, system_prompt)?;
    let input_limit = config
        .max_context_tokens
        .saturating_sub(usize::try_from(compaction_max_output_tokens(config)).unwrap_or(0));
    Ok(input_tokens > input_limit)
}

pub(crate) fn message_hashes(messages: &[Value]) -> Vec<[u8; 32]> {
    use sha2::{Digest, Sha256};

    messages
        .iter()
        .map(|message| {
            let mut hasher = Sha256::new();
            hasher.update(serde_json::to_vec(message).unwrap_or_default());
            hasher.finalize().into()
        })
        .collect()
}

/// A cache LCP may end between an assistant tool call and its user-side tool
/// result (for example when an attachment in that result changed projection).
/// Keep the aligned prefix protocol-complete: otherwise OpenAI conversion
/// strips the orphan call before the reduced suffix can describe the exchange.
pub(crate) fn rollback_incomplete_tool_exchange_boundary(
    messages: &[Value],
    boundary: usize,
) -> usize {
    let prefix = &messages[..boundary.min(messages.len())];
    let result_ids = prefix
        .iter()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
        .collect::<std::collections::HashSet<_>>();

    prefix
        .iter()
        .enumerate()
        .filter(|(_, message)| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .find_map(|(index, message)| {
            message
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_use")
                            && block
                                .get("id")
                                .and_then(Value::as_str)
                                .is_none_or(|id| id.is_empty() || !result_ids.contains(id))
                    })
                })
                .then_some(index)
        })
        .unwrap_or(boundary)
}

pub fn effective_max_tokens(config: &Config) -> i32 {
    if config.max_context_tokens == 0 {
        return config.max_tokens;
    }
    let reserve = config.context_reserve_tokens.max(1);
    let requested = usize::try_from(config.max_tokens.max(1)).unwrap_or(1);
    i32::try_from(requested.min(reserve)).unwrap_or(i32::MAX)
}

pub fn compaction_max_output_tokens(config: &Config) -> i32 {
    config.context_compact_max_output_tokens.max(1)
}

pub fn request_input_limit(config: &Config) -> usize {
    if config.max_context_tokens == 0 {
        return usize::MAX;
    }
    config
        .max_context_tokens
        .saturating_sub(usize::try_from(effective_max_tokens(config)).unwrap_or(0))
}

pub(crate) fn is_forced_trigger(trigger: &str) -> bool {
    matches!(trigger, "manual" | "preflight" | "overflow")
}

pub(crate) fn compaction_trigger_tokens(config: &Config) -> usize {
    let percentage = ((config.max_context_tokens as u128
        * u128::from(config.context_compact_pct.clamp(1, 100)))
        / 100)
        .min(usize::MAX as u128) as usize;
    percentage.min(
        config
            .max_context_tokens
            .saturating_sub(config.context_reserve_tokens),
    )
}

pub(crate) fn estimate_messages_tokens(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|message| {
            serde_json::to_vec(message)
                .map_or(0, |value| value.len())
                .div_ceil(3)
                .max(1)
        })
        .fold(0, |total, tokens| total.saturating_add(tokens))
}

pub(crate) fn find_compaction_cut_point(messages: &[Value], tail_target: usize) -> usize {
    if messages.len() < 2 {
        return 0;
    }
    let mut tokens = 0usize;
    let mut candidate = messages.len() - 1;
    for index in (0..messages.len()).rev() {
        tokens = tokens.saturating_add(estimate_messages_tokens(&messages[index..=index]));
        candidate = index;
        if tokens >= tail_target {
            break;
        }
    }
    let safe = (1..=candidate)
        .rev()
        .find(|&index| is_safe_context_start(&messages[index]))
        .unwrap_or(0);

    // Keep at least COMPACTION_MIN_TAIL_USER_MESSAGES real user messages in the
    // tail so user constraints do not silently fall behind the cut point.
    let total_users = messages.iter().filter(|m| is_real_user_message(m)).count();
    if total_users < COMPACTION_MIN_TAIL_USER_MESSAGES {
        return safe;
    }
    let mut cut = safe;
    let mut users_seen = messages[cut..]
        .iter()
        .filter(|m| is_real_user_message(m))
        .count();
    while users_seen < COMPACTION_MIN_TAIL_USER_MESSAGES && cut > 0 {
        cut -= 1;
        if is_real_user_message(&messages[cut]) {
            users_seen += 1;
        }
    }
    if cut == 0 {
        // Not enough history to satisfy the guard; keep the token-based cut.
        return safe;
    }
    // `cut` 由 safe start 或真实 user 消息得出，必然是安全边界；
    // 前向对齐循环不可达，以断言钉住该不变式。
    debug_assert!(
        cut >= messages.len() || is_safe_context_start(&messages[cut]),
        "compaction cut point must land on a safe context start"
    );
    cut
}

pub(crate) fn is_real_user_message(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if message.get("internal").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if message.get("_mink").is_some() {
        return false;
    }
    let Some(content) = message.get("content").and_then(Value::as_str) else {
        // Array content (tool results / attachments) is never a real user
        // message and never a safe compaction start (v7 §10.1).
        return false;
    };
    !RUNTIME_INJECTED_MARKERS
        .iter()
        .any(|marker| content.starts_with(marker))
}

pub(crate) fn is_safe_context_start(message: &Value) -> bool {
    // Runtime-injected user messages are not safe boundaries: a cut may never
    // land on an internal prompt that the caller did not author.
    message.get("role").and_then(Value::as_str) == Some("assistant")
        || is_real_user_message(message)
}

pub(crate) fn strip_dsml_tags(text: &str) -> String {
    let regex = regex::Regex::new(r"</?ds_\w+[^>]*>").unwrap();
    regex.replace_all(text, "").into_owned()
}
