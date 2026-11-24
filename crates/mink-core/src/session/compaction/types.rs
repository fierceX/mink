use super::*;

/// 摘要侧「本次压缩不可用」：摘要构建/调用/响应验收/候选预算验收失败。上层可以据此
/// 转向确定性应急 checkpoint，而不是让整个 turn 失败。
///
/// fault、取消、正式历史协议损坏**不属于**这一类，它们必须原样传播。
#[derive(Debug)]
pub(crate) struct SummaryUnavailable {
    pub reason: String,
}

impl SummaryUnavailable {
    pub(crate) fn error(reason: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            reason: reason.into(),
        })
    }
}

impl std::fmt::Display for SummaryUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for SummaryUnavailable {}

/// 压缩过程中被取消/中断（用户 interrupt、runtime cancel）。上层必须把它映射为
/// `TurnDecision::Interrupted`，而不是普通失败。
#[derive(Debug)]
pub(crate) struct CompactionInterrupted;

impl CompactionInterrupted {
    pub(crate) fn error() -> anyhow::Error {
        anyhow::Error::new(Self)
    }
}

impl std::fmt::Display for CompactionInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("compaction interrupted")
    }
}

impl std::error::Error for CompactionInterrupted {}

/// 共享 round 绝对期限耗尽：这是主请求终态 timeout，区别于摘要服务自身的失败，
/// 上层不得据此转应急 checkpoint。
#[derive(Debug)]
pub(crate) struct CompactionDeadlineExpired;

impl CompactionDeadlineExpired {
    pub(crate) fn error() -> anyhow::Error {
        anyhow::Error::new(Self)
    }
}

impl std::fmt::Display for CompactionDeadlineExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request_timeout: compaction deadline exceeded")
    }
}

impl std::error::Error for CompactionDeadlineExpired {}

/// 共享绝对期限是否已耗尽（`None` 表示未配置，永不判定耗尽）。
pub(crate) fn shared_deadline_expired(deadline: Option<std::time::Instant>) -> bool {
    deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline)
}

/// 共享期限耗尽时，摘要侧的 `SummaryUnavailable` 必须定型为主请求终态 timeout：
/// 否则上层会把它当作可转应急的摘要服务失败，超时后仍发布有损 checkpoint。
pub(super) fn map_shared_deadline(
    error: anyhow::Error,
    round_deadline: Option<std::time::Instant>,
) -> anyhow::Error {
    if shared_deadline_expired(round_deadline) && is_summary_unavailable(&error) {
        CompactionDeadlineExpired::error()
    } else {
        error
    }
}

/// 错误链中是否存在共享期限耗尽。
pub(crate) fn is_compaction_deadline_expired(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<CompactionDeadlineExpired>().is_some())
}

/// 主请求的形状（真实 system prompt 与 tools）。候选验收与应急 checkpoint 都必须按它
/// 估算实际请求大小，而不是内部 raw JSON 的粗略估算。
#[derive(Clone, Copy)]
pub(crate) struct MainRequestShape<'a> {
    pub system_prompt: &'a str,
    pub tools: &'a [Value],
    /// Already converted tools from the active round projection. `None` keeps
    /// legacy callers working by converting `tools` again.
    pub openai_tools: Option<&'a [Value]>,
}

impl MainRequestShape<'_> {
    pub(super) fn estimate(&self, messages: &[Value]) -> Result<usize> {
        match self.openai_tools {
            Some(openai_tools) => {
                let converted = crate::llm::transport::convert_messages_to_openai(messages)?;
                crate::llm::transport::estimate_openai_converted(
                    &converted,
                    openai_tools,
                    self.system_prompt,
                )
            }
            None => crate::llm::transport::estimate_openai_context_tokens(
                messages,
                self.tools,
                self.system_prompt,
            ),
        }
    }
}

/// 候选投影用的 todo 来源：引擎按**压缩后的候选消息**判断是否需要预演 TodoSync
/// （压缩会把最新 revision 折走，按压缩前历史判断会漏掉同步消息）。
pub(crate) struct TodoCandidateState<'a> {
    pub snapshot: &'a crate::session::todo::TodoSnapshot,
    pub read_provider: &'a str,
    pub allowance_tokens: usize,
}

/// 按候选投影决定是否需要补一条预演 TodoSync：候选里已经看不到目标 revision 时必须
/// 带上（`_mink.todo_revision` 是判断依据，与 `reconcile_todo_state` 同源）。
/// 历史 revision 领先权威文件或元数据损坏时 fail closed，不得静默按旧快照同步。
pub(crate) fn candidate_todo_sync(
    candidate: &[Value],
    todo: Option<&TodoCandidateState<'_>>,
) -> Result<Option<Value>> {
    let Some(todo) = todo else {
        return Ok(None);
    };
    let visible = crate::session::todo::visible_revision(candidate)?;
    if visible > todo.snapshot.revision {
        anyhow::bail!(
            "todo conversation revision {visible} is newer than persisted revision {}; refusing to compact",
            todo.snapshot.revision
        );
    }
    if visible == todo.snapshot.revision {
        return Ok(None);
    }
    Ok(Some(crate::session::todo::sync_message_bounded(
        todo.snapshot,
        todo.read_provider,
        todo.allowance_tokens,
    )))
}

/// 候选验收与真实请求使用同一投影：预演 TodoSync → 完整请求投影（已消费图片降为文本
/// 引用等）→ 按主请求形状估算。候选与发送不允许走两套计费口径。
pub(crate) fn estimate_candidate_tokens(
    shape: MainRequestShape<'_>,
    candidate: &[Value],
    todo: Option<&TodoCandidateState<'_>>,
) -> Result<usize> {
    let mut projected = candidate.to_vec();
    if let Some(todo_sync) = candidate_todo_sync(&projected, todo)? {
        projected.push(todo_sync);
    }
    let projected = crate::session::plan::project_full_request(&projected)?;
    shape.estimate(&projected)
}

/// 活跃窗口里是否存在未配对的工具调用/结果（正式历史协议损坏）。
/// 应急 checkpoint 折叠整段历史，任何未完成交换都必须原样失败而不是被摘录掩盖。
pub(super) fn unpaired_tool_exchange(messages: &[Value]) -> Option<String> {
    let mut pending: Vec<String> = Vec::new();
    for message in messages {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                    if id.is_empty() {
                        return Some("tool call without id".to_string());
                    }
                    pending.push(id.to_string());
                }
                Some("tool_result") => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    match pending.iter().position(|pending_id| pending_id == id) {
                        Some(position) => {
                            pending.remove(position);
                        }
                        None => {
                            return Some(format!("tool result {id} has no matching call"));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    pending
        .first()
        .map(|id| format!("tool call {id} has no result"))
}

/// 应急 checkpoint 的请求上下文：全部来自调用方的真实数据（turn 输入、todo 候选状态、
/// 主请求形状），引擎不自行读取历史以外的东西。
pub(crate) struct EmergencyCheckpointContext<'a> {
    pub main_request: MainRequestShape<'a>,
    /// 更小的目标预算（token）。provider 报 overflow 时上层传入「固定前缀 + 可变额度
    /// 折半」的目标：本地估算说装得下、provider 却拒绝时，只靠本地闸门无法恢复。
    pub target_limit: Option<usize>,
    /// 当前用户请求原文；manual 等没有当前输入时为 `None`（不得伪造）。
    pub current_user_input: Option<&'a str>,
    /// todo 来源（快照 + 读 provider + 展示额度）：同步需求按候选投影判断。
    pub todo: Option<TodoCandidateState<'a>>,
    /// 折叠历史里真实出现、且仍存在于 artifact 索引中的引用（不得虚构 URL）。
    pub artifact_refs: Vec<String>,
    /// 共享 round 绝对期限：耗尽时应急必须 fail closed，不得发布新的有损 checkpoint。
    pub deadline: Option<std::time::Instant>,
}

/// 本次压缩失败是否可以转向应急 checkpoint（仅摘要不可用类）。
pub(crate) fn is_summary_unavailable(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<SummaryUnavailable>().is_some())
}

/// 摘要不可用失败的有界原因码：只映射已知分类，不把 provider 长正文带进模型提示。
pub(crate) fn summary_unavailable_reason(error: &anyhow::Error) -> Option<&'static str> {
    let message = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<SummaryUnavailable>())
        .map(ToString::to_string)?;
    Some(if message.starts_with("request_retry_exhausted") {
        "retry_exhausted"
    } else if message.starts_with("request_timeout") {
        "timeout"
    } else if message.contains("budget") || message.contains("leaves no practical output") {
        "budget"
    } else if message.contains("empty response")
        || message.contains("invalid stop reason")
        || message.contains("invalid tool call")
    {
        "invalid_output"
    } else if message.starts_with("failed to generate context summary") {
        "upstream"
    } else {
        "unavailable"
    })
}

/// 本次压缩失败是否由取消/中断造成（必须映射为 Interrupted）。
pub(crate) fn is_compaction_interrupted(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<CompactionInterrupted>().is_some())
}

#[derive(Debug)]
pub(super) struct PressureDecision {
    pub(super) source: &'static str,
    pub(super) effective_tokens: usize,
    pub(super) provider_baseline_tokens: Option<usize>,
}

#[derive(Debug, Clone)]
pub(super) struct SummaryInputMeta {
    pub(super) input_mode: &'static str,
    pub(super) aligned_messages: usize,
    pub(super) aligned_estimated_tokens: usize,
    pub(super) reduced_suffix_messages: usize,
    pub(super) fallback_reason: Option<String>,
}

pub(super) struct SummaryRequestInput {
    pub(super) system_prompt: String,
    pub(super) tools: Vec<Value>,
    pub(super) messages: Vec<Value>,
    pub(super) meta: SummaryInputMeta,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct CompactionState {
    pub(super) active_start: usize,
    pub(super) summary: String,
}

pub(super) fn load_state(path: &Path) -> Result<CompactionState> {
    if !path.exists() {
        return Ok(CompactionState::default());
    }
    let data = std::fs::read(path)?;
    if data.iter().all(u8::is_ascii_whitespace) {
        return Ok(CompactionState::default());
    }
    Ok(serde_json::from_slice(&data)?)
}

/// Minimum number of real user messages that must survive a compaction in the
/// active tail (preferred over the pure token budget, bounded by history size).
pub(crate) const COMPACTION_MIN_TAIL_USER_MESSAGES: usize = 2;

/// Engine-injected user-role messages that must not count as real user
/// constraints for the compaction guard. The primary signal is the `internal`
/// (or `_mink`) metadata flag set by `add_runtime_user` / todo sync; the
/// string markers below remain as a defensive fallback for historical or
/// third-party messages that predate the flag.
pub(crate) const RUNTIME_INJECTED_MARKERS: &[&str] = &[
    "<todo-progress-reminder>",
    "<todo-final-reminder>",
    "<todo-sync",
    "[System note:",
];
