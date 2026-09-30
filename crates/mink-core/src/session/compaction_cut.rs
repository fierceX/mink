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
            "This is a runtime-generated checkpoint replacing an earlier span of the conversation; it may be a model summary or a lossy excerpt, and details may be omitted. Treat it as background and continue from the messages that follow without acknowledging the checkpoint.\n\n<compacted-summary>\n{}\n</compacted-summary>",
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

/// 模型可见派生 checkpoint（plan / todo 展示）在本次请求里可用的 token 额度：
/// 主请求输入预算的固定内部比例，带下限。权威文件与 revision 不受影响，只有
/// 派生展示会被截短（有损展示，不宣称可用引用恢复细节）。
pub(crate) const DERIVED_DISPLAY_MIN_TOKENS: usize = 256;

pub(crate) fn derived_display_tokens(config: &Config) -> usize {
    let limit = request_input_limit(config);
    if limit == usize::MAX {
        return usize::MAX;
    }
    (limit / 8).max(DERIVED_DISPLAY_MIN_TOKENS)
}

/// 按 token 额度有界渲染一段派生正文：能放下就原样返回，否则头部 + 省略标记 + 尾部。
/// 省略标记计入额度；按 UTF-8 字节上界约束（4 字节字符不会超额）。
pub(crate) fn bounded_derived_text(content: &str, allowance_tokens: usize) -> String {
    let max_bytes = allowance_tokens.saturating_mul(3);
    if max_bytes == usize::MAX || content.len() <= max_bytes {
        return content.to_string();
    }
    head_tail_with_marker(
        content,
        max_bytes,
        "\n[derived display truncated: ",
        " bytes omitted; the authoritative content is unchanged]\n",
    )
}

/// 按 UTF-8 字节预算保留首尾：省略标记（prefix + 省略字节数 + suffix）计入预算，
/// 正常路径下结果**严格短于输入**；输入已在预算内时原样返回。始终从原文生成
/// （绝不在已省略的文本上二次裁剪），按字符边界切片。
/// 优先使用完整标记；额度放不下完整标记时退回短标记 `…`，**任何被截短的文本
/// 都带显式省略提示**（仅预算 < 3 字节、连单字符标记都放不下时例外）。
pub(crate) fn head_tail_with_marker(
    text: &str,
    budget_bytes: usize,
    marker_prefix: &str,
    marker_suffix: &str,
) -> String {
    if budget_bytes == 0 {
        return String::new();
    }
    if text.len() <= budget_bytes {
        return text.to_string();
    }
    if let Some(fitted) = fit_head_tail(text, budget_bytes, &|omitted| {
        format!("{marker_prefix}{omitted}{marker_suffix}")
    }) {
        return fitted;
    }
    if let Some(fitted) = fit_head_tail(text, budget_bytes, &|_| "…".to_string()) {
        return fitted;
    }
    // 预算小于短标记（3 字节）：无法容纳任何标记，只能保留能装下的部分。
    // 调用方的正常额度到不了这里（Todo/plan 会先按最小正文额度整条省略）。
    let short = "…";
    let head = floor_char_boundary(text, budget_bytes.saturating_sub(short.len()));
    if head == 0 {
        return short[..floor_char_boundary(short, budget_bytes)].to_string();
    }
    format!("{}{short}", &text[..head])
}

/// 在给定预算内按 `marker(omitted_bytes)` 生成头部 + 标记 + 尾部；放不下返回 `None`。
fn fit_head_tail(
    text: &str,
    budget_bytes: usize,
    marker: &dyn Fn(usize) -> String,
) -> Option<String> {
    let mut head = (budget_bytes / 2).min(text.len());
    let mut tail = (budget_bytes / 2).min(text.len());
    for _ in 0..64 {
        head = floor_char_boundary(text, head);
        // 尾部切片起点向“更少保留”方向对齐到字符边界。
        let mut tail_start = text.len().saturating_sub(tail);
        while tail_start < text.len() && !text.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        tail = text.len().saturating_sub(tail_start);
        let omitted = text.len().saturating_sub(head + tail);
        let marker_text = marker(omitted);
        if head + marker_text.len() + tail <= budget_bytes {
            let mut out = String::with_capacity(head + marker_text.len() + tail);
            out.push_str(&text[..head]);
            out.push_str(&marker_text);
            out.push_str(&text[text.len() - tail..]);
            return Some(out);
        }
        let excess = head + marker_text.len() + tail - budget_bytes;
        if head >= tail {
            head = head.saturating_sub(excess.max(1));
        } else {
            tail = tail.saturating_sub(excess.max(1));
            if tail == 0 && head == 0 {
                return None;
            }
        }
    }
    None
}

/// 按 UTF-8 边界向下取整的字节位置。
pub(crate) fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// 发布前的真实净收益判定：auto 要求 ≥10% 净下降（摘要/checkpoint/TodoSync 都计入）；
/// 强制路径（manual/preflight/overflow）只要严格下降。`before == 0` 表示调用方没有
/// 真实估算（旧测试入口），不做门控。
pub(crate) fn net_benefit_sufficient(trigger: &str, before: usize, after: usize) -> bool {
    if before == 0 {
        return true;
    }
    if trigger == "auto" {
        after.saturating_mul(10) <= before.saturating_mul(9)
    } else {
        after < before
    }
}

pub(crate) fn read_active_plan_checkpoint(
    summary_path: &Path,
    allowance_tokens: usize,
) -> Result<Option<Value>> {
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
    // 额度覆盖完整块：先扣掉 <active-plan-checkpoint> 包装再分配正文额度。
    let wrapper = "<active-plan-checkpoint>\n\n</active-plan-checkpoint>";
    let inner_tokens = if allowance_tokens == usize::MAX {
        usize::MAX
    } else {
        allowance_tokens
            .saturating_mul(3)
            .saturating_sub(wrapper.len())
            / 3
    };
    let content = bounded_derived_text(content.trim(), inner_tokens);
    Ok((!content.is_empty()).then(|| {
        json!({
            "role": "user",
            "internal": true,
            "content": format!("<active-plan-checkpoint>\n{content}\n</active-plan-checkpoint>"),
        })
    }))
}

/// 摘要请求的输出低于该值时不具备实用性：直接转应急，而不是发送几乎必然被截断的
/// 请求（本轮不引入“模型最小摘要 token 数”这类未经验证的档位）。
pub(crate) const MIN_PRACTICAL_SUMMARY_TOKENS: usize = 256;

/// 一次摘要请求的输出 cap：取配置上限与「输入 + 输出必须装进窗口」的较小者。
/// `None` 表示剩余空间不足以产出有实用价值的摘要（上层应转应急 checkpoint）。
pub(crate) fn summary_output_cap(config: &Config, input_tokens: usize) -> Option<i32> {
    let configured = compaction_max_output_tokens(config);
    if config.max_context_tokens == 0 {
        return Some(configured);
    }
    let available = config.max_context_tokens.saturating_sub(input_tokens);
    let cap = available.min(usize::try_from(configured).unwrap_or(usize::MAX));
    if cap < MIN_PRACTICAL_SUMMARY_TOKENS {
        return None;
    }
    Some(i32::try_from(cap).unwrap_or(i32::MAX))
}

/// 摘要输入是否超出 `窗口 - reserved_output`。`reserved_output` 由调用方按路径选择：
/// 缓存对齐候选按配置输出上限判断（装不下就退化到专用摘要路径），降噪/原始路径只需
/// 为一个有实用价值的最小输出留位置，具体 cap 由 [`summary_output_cap`] 动态给出。
pub(crate) fn summary_input_over_budget(
    config: &Config,
    messages: &[Value],
    tools: &[Value],
    system_prompt: &str,
    reserved_output: usize,
) -> Result<bool> {
    if config.max_context_tokens == 0 {
        return Ok(false);
    }
    let input_tokens =
        crate::llm::transport::estimate_openai_context_tokens(messages, tools, system_prompt)?;
    let input_limit = config.max_context_tokens.saturating_sub(reserved_output);
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
    if users_seen < COMPACTION_MIN_TAIL_USER_MESSAGES {
        // The tail-user guard is a hard requirement: refusing to compact
        // (cut=0 => "no safe boundary") is the only honest outcome when
        // satisfying it would drop every message.
        return 0;
    }
    // `cut` comes from a safe start or a real user message, so it is a safe
    // boundary; the assertion pins that invariant.
    debug_assert!(
        is_safe_context_start(&messages[cut]),
        "compaction cut point must land on a safe context start"
    );
    cut
}

/// Degraded cut point, used only as a last resort when the request is already
/// over the input budget and [`find_compaction_cut_point`] returned nothing: it
/// ignores the hot-tail token target and the "keep two verbatim user turns"
/// guard, folding everything that precedes the newest safe boundary. The folded
/// span is summarised like any other compaction, so the current request stays
/// recoverable as summary text instead of failing the whole turn.
pub(crate) fn find_degraded_compaction_cut_point(messages: &[Value]) -> usize {
    if messages.len() < 2 {
        return 0;
    }
    let candidate = messages.len() - 1;
    (1..=candidate)
        .rev()
        .find(|&index| is_safe_context_start(&messages[index]))
        .unwrap_or(0)
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
