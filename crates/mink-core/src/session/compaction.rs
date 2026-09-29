use crate::config::ResolvedConfig as Config;
use crate::llm::client::{LlmBackend, LlmCacheProjection, LlmModelTarget, LlmPurpose, LlmRequest};
use crate::protocol::{ErrorEvent, Event, StopEvent, TextEvent, UsageEvent};
use crate::session::compaction_input;
use crate::session::event_log::EventLogWriter;
use crate::session::stats::StatsTracker;
use crate::session::store::ConversationStore;
use crate::session::usage::{UsageJournal, UsageKind};
use crate::ui::Display;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub(super) const COMPACTION_INSTRUCTION: &str = "Merge the conversation history above into one checkpoint. Preserve current user goals, constraints, decisions, progress, blockers, file changes, commands, errors, pending work, and exact identifiers. An earlier <compacted-summary>, if present, is established background and must be merged with the newer history. Output these seven non-empty fields: Task focus:, Latest request:, Progress:, Errors:, Decisions:, Tool evidence:, Reflections:. Write (none) for any field without content. Start directly with Task focus:, do not use code fences, do not continue the task, and do not call tools.";

const FALLBACK_SYSTEM_PROMPT: &str = "Summarize coding-agent history for a later model. Preserve user goals, constraints, decisions, progress, blockers, file changes, commands, errors, pending work, and exact identifiers. Do not continue the task.";

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

/// 主请求的形状（真实 system prompt 与 tools）。候选验收与应急 checkpoint 都必须按它
/// 估算实际请求大小，而不是内部 raw JSON 的粗略估算。
#[derive(Clone, Copy)]
pub(crate) struct MainRequestShape<'a> {
    pub system_prompt: &'a str,
    pub tools: &'a [Value],
}

impl MainRequestShape<'_> {
    fn estimate(&self, messages: &[Value]) -> Result<usize> {
        crate::llm::transport::estimate_openai_context_tokens(
            messages,
            self.tools,
            self.system_prompt,
        )
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
pub(crate) fn candidate_todo_sync(
    candidate: &[Value],
    todo: Option<&TodoCandidateState<'_>>,
) -> Option<Value> {
    let todo = todo?;
    let visible = crate::session::todo::visible_revision(candidate).unwrap_or(0);
    if visible >= todo.snapshot.revision {
        return None;
    }
    Some(crate::session::todo::sync_message_bounded(
        todo.snapshot,
        todo.read_provider,
        todo.allowance_tokens,
    ))
}

/// 活跃窗口里是否存在未配对的工具调用/结果（正式历史协议损坏）。
/// 应急 checkpoint 折叠整段历史，任何未完成交换都必须原样失败而不是被摘录掩盖。
fn unpaired_tool_exchange(messages: &[Value]) -> Option<String> {
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
}

/// 应急摘录的材料块，按优先级保留；[`Self::shrink`] 每次严格减少一块内容。
#[derive(Default)]
struct EmergencyBlocks {
    marker: String,
    request: Option<String>,
    previous: Option<String>,
    recent: Option<String>,
    artifacts: Option<String>,
}

impl EmergencyBlocks {
    fn render(&self) -> String {
        let mut out = self.marker.clone();
        for (title, block) in [
            ("latest user request", &self.request),
            ("previous checkpoint (may be partial)", &self.previous),
            ("recent tool evidence", &self.recent),
            ("available artifacts", &self.artifacts),
        ] {
            if let Some(block) = block.as_deref().filter(|b| !b.trim().is_empty()) {
                out.push_str("\n\n[");
                out.push_str(title);
                out.push_str("]\n");
                out.push_str(block);
            }
        }
        out
    }

    /// 严格减少一格内容：先丢低优先级块，再把请求摘录减半，最后只剩最小头部。
    /// 返回 false 表示已经到地板（标记 + 最小请求头部），无法继续缩小。
    fn shrink(&mut self) -> bool {
        if self.artifacts.take().is_some() {
            return true;
        }
        if self.recent.take().is_some() {
            return true;
        }
        if self.previous.take().is_some() {
            return true;
        }
        if let Some(request) = self.request.take() {
            let shrunk = shrink_excerpt(&request, request_head_floor(&request));
            if let Some(shrunk) = shrunk {
                self.request = Some(shrunk);
                return true;
            }
            self.request = Some(request);
            return false;
        }
        false
    }
}

/// 头部+尾部裁剪：保留 `head` 个字符的头、`tail` 个字符的尾，中间标注省略量。
/// 省略标记计入额度，且**结果必须严格短于输入**，否则返回 `None`（否则外层收缩循环
/// 会在同一个文本上无限重试）。`floor_chars` 是保留的真实字符下限；到达下限仍无法变短
/// 就停止收缩。按 UTF-8 边界裁剪。
fn shrink_excerpt(text: &str, floor_chars: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    if len == 0 {
        return None;
    }
    let floor = floor_chars.clamp(1, len.saturating_sub(1).max(1));
    let mut keep = (len / 2).max(floor);
    loop {
        let head = (keep / 2).max(1).min(len);
        let tail = keep.saturating_sub(head).min(len.saturating_sub(head));
        let omitted = len.saturating_sub(head + tail);
        let mut out: String = chars[..head].iter().collect();
        out.push_str(&format!("[…{omitted} chars omitted…]"));
        if tail > 0 {
            out.extend(chars[len - tail..].iter());
        }
        if out.chars().count() < len {
            return Some(out);
        }
        if keep <= floor {
            return None;
        }
        keep = (keep / 2).max(floor);
    }
}

/// 请求摘录的地板长度（标记之外必须留下的最小真实目标）。
fn request_head_floor(text: &str) -> usize {
    64.min(text.chars().count())
}

/// 单块正文的通用裁剪上限（用于初始构造，避免把整段历史塞进摘录）。
const EMERGENCY_BLOCK_CHARS: usize = 1_200;

/// 本次压缩失败是否可以转向应急 checkpoint（仅摘要不可用类）。
pub(crate) fn is_summary_unavailable(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<SummaryUnavailable>().is_some())
}

/// 本次压缩失败是否由取消/中断造成（必须映射为 Interrupted）。
pub(crate) fn is_compaction_interrupted(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<CompactionInterrupted>().is_some())
}

#[derive(Debug, Clone)]
struct LatestAgentRequest {
    model: String,
    source_fingerprint: String,
    local_tokens: usize,
    projection_generation: u64,
    backend_name: String,
    source_system_prompt: String,
    source_tools: Vec<Value>,
    projection: Option<CacheProjectionSnapshot>,
}

#[derive(Debug, Clone)]
struct PromptUsageBaseline {
    request: Arc<LatestAgentRequest>,
    provider_prompt_tokens: usize,
}

#[derive(Debug, Default)]
struct PromptUsageState {
    latest_request: Option<Arc<LatestAgentRequest>>,
    baseline: Option<PromptUsageBaseline>,
}

/// Long-lived provider projection metadata. Message bodies are represented by
/// hashes so request-time image data URLs can never survive across requests.
#[derive(Debug, Clone)]
struct CacheProjectionSnapshot {
    model: String,
    system_prompt: String,
    tools: Vec<Value>,
    message_hashes: Vec<[u8; 32]>,
}

#[derive(Debug)]
struct PressureDecision {
    source: &'static str,
    effective_tokens: usize,
    provider_baseline_tokens: Option<usize>,
}

#[derive(Debug, Clone)]
struct SummaryInputMeta {
    input_mode: &'static str,
    aligned_messages: usize,
    aligned_estimated_tokens: usize,
    reduced_suffix_messages: usize,
    fallback_reason: Option<String>,
}

struct SummaryRequestInput {
    system_prompt: String,
    tools: Vec<Value>,
    messages: Vec<Value>,
    meta: SummaryInputMeta,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CompactionState {
    active_start: usize,
    summary: String,
}

pub struct CompactionEngine {
    store: Arc<ConversationStore>,
    summary_path: PathBuf,
    state_path: PathBuf,
    api_url: String,
    api_key: String,
    llm_backend: Arc<dyn LlmBackend>,
    config: Config,
    stats: Arc<StatsTracker>,
    usage: Arc<UsageJournal>,
    session_id: String,
    display: Arc<dyn Display>,
    cancel: crate::cancel::CancellationToken,
    interrupt: Arc<AtomicBool>,
    state: RwLock<std::result::Result<CompactionState, String>>,
    compact_lock: tokio::sync::Mutex<()>,
    memo_epoch: Arc<AtomicU64>,
    projection_generation: AtomicU64,
    prompt_usage: Mutex<PromptUsageState>,
    projection_dirty: AtomicBool,
    event_log_writer: Option<EventLogWriter>,
    /// Shared session publish-fault latch for context-state and projection
    /// writes.
    fault: crate::session::persistence::PersistenceFault,
    /// Messages skipped by startup boundary repair; carried into the next
    /// summarization input so they are not silently lost.
    startup_repair_loss: Mutex<Vec<Value>>,
}

pub(crate) use crate::session::compaction_cut::*;

/// 单行字段清洗（事件 result 串使用）：折叠空白并限长。
fn sanitize_field(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join("_");
    truncate_chars(&collapsed, max_chars)
}

/// 按字符（UTF-8 边界安全）截断，超出时标注省略量。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let head = max_chars.saturating_sub(16).max(1);
    let mut out: String = text.chars().take(head).collect();
    out.push_str(&format!("[…{} chars omitted…]", count - head));
    out
}

/// 活跃窗口里最后一条真实 user 消息的正文（结构性判断，不解析展示文本）。
fn last_real_user_text(active: &[Value]) -> Option<String> {
    active
        .iter()
        .rev()
        .find(|message| is_real_user_message(message))
        .and_then(|message| message.get("content").and_then(Value::as_str))
        .map(str::to_string)
}

/// 最近已完成工具交换的结构化事实（工具名 / 调用 ID / 结果头尾摘录）。
/// 不做展示文本反解，也不推断成功与否。
fn recent_evidence(active: &[Value]) -> Option<String> {
    const MAX_EXCHANGES: usize = 6;
    let mut lines: Vec<String> = Vec::new();
    let mut calls = 0usize;
    for message in active.iter().rev() {
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                    let id = block.get("id").and_then(Value::as_str).unwrap_or("?");
                    let input = block
                        .get("input")
                        .map(|value| value.to_string())
                        .unwrap_or_default();
                    lines.push(format!(
                        "- call {name} ({id}): {}",
                        truncate_chars(&input, 200)
                    ));
                    calls += 1;
                    if calls >= MAX_EXCHANGES {
                        break;
                    }
                }
                Some("tool_result") => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    let text = block
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    lines.push(format!("- result {id}: {}", truncate_chars(text, 240)));
                }
                _ => {}
            }
        }
        if calls >= MAX_EXCHANGES {
            break;
        }
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

/// 应急摘录材料：固定标记 → 当前用户请求 → 上一份摘要片段 → 最近工具证据。
/// 全部来自真实数据；不读取 artifact 索引，因此不产生任何可能失效的引用。
fn emergency_blocks(
    reason: &str,
    active: &[Value],
    previous_summary: &str,
    current_user_input: Option<&str>,
    artifact_refs: &[String],
) -> EmergencyBlocks {
    EmergencyBlocks {
        marker: format!(
            "[emergency-context-excerpt] This runtime-generated checkpoint replaces the earlier span of this conversation without a model summary (reason: {}). The full history is still stored in the session records; this is a lossy excerpt, not a completion signal, and details may be omitted.",
            sanitize_field(reason, 96)
        ),
        request: current_user_input
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string)
            .or_else(|| last_real_user_text(active))
            .map(|text| truncate_chars(text.trim(), EMERGENCY_BLOCK_CHARS)),
        previous: Some(previous_summary.trim())
            .filter(|summary| !summary.is_empty())
            .map(|summary| truncate_chars(summary, EMERGENCY_BLOCK_CHARS / 2)),
        recent: recent_evidence(active),
        artifacts: (!artifact_refs.is_empty()).then(|| {
            artifact_refs
                .iter()
                .map(|id| format!("- artifact://{id}"))
                .collect::<Vec<_>>()
                .join("\n")
        }),
    }
}

impl CompactionEngine {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Arc<ConversationStore>,
        summary_path: PathBuf,
        api_url: String,
        config: &Config,
        stats: Arc<StatsTracker>,
        usage: Arc<UsageJournal>,
        session_id: String,
        display: Arc<dyn Display>,
        cancel: crate::cancel::CancellationToken,
        interrupt: Arc<AtomicBool>,
        llm_backend: Arc<dyn LlmBackend>,
        event_log_writer: Option<EventLogWriter>,
    ) -> Result<Self> {
        let state_path = summary_path.with_file_name("context-state.json");
        let state = load_state(&state_path)?;
        let expected_projection = format!("{}\n", state.summary);
        let projection_matches = std::fs::read(&summary_path)
            .is_ok_and(|content| content == expected_projection.as_bytes());
        if !projection_matches {
            // Startup-only projection repair: a failure aborts construction
            // before the engine is usable, so no shared latch is involved.
            crate::session::atomic_file::atomic_replace(
                &summary_path,
                expected_projection.as_bytes(),
            )?;
        }
        Ok(Self {
            store,
            summary_path,
            state_path,
            api_url,
            api_key: config.api_key.clone(),
            llm_backend,
            config: config.clone(),
            stats,
            usage,
            session_id,
            display,
            cancel,
            interrupt,
            state: RwLock::new(Ok(state)),
            compact_lock: tokio::sync::Mutex::new(()),
            memo_epoch: Arc::new(AtomicU64::new(0)),
            projection_generation: AtomicU64::new(0),
            prompt_usage: Mutex::new(PromptUsageState::default()),
            projection_dirty: AtomicBool::new(false),
            event_log_writer,
            fault: Default::default(),
            startup_repair_loss: Mutex::new(Vec::new()),
        })
    }

    /// Attach the session-wide publish-fault latch (see
    /// [`crate::session::persistence`]).
    pub(crate) fn with_fault(
        mut self,
        fault: crate::session::persistence::PersistenceFault,
    ) -> Self {
        self.fault = fault;
        self
    }

    /// Shared epoch counter for read memos: any committed compaction invalidates
    /// all in-session read memos (the model's context no longer holds the
    /// previously read content, so "reuse" responses would be misleading).
    pub fn memo_epoch(&self) -> Arc<AtomicU64> {
        self.memo_epoch.clone()
    }

    pub fn current_summary(&self) -> Result<Option<String>> {
        let state = self.current_state()?;
        Ok((!state.summary.trim().is_empty()).then_some(state.summary))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_agent_request(
        &self,
        model: &str,
        source_fingerprint: &str,
        local_tokens: usize,
        backend_name: String,
        source_system_prompt: String,
        source_tools: Vec<Value>,
        projection: Option<LlmCacheProjection>,
    ) {
        let request = Arc::new(LatestAgentRequest {
            model: model.to_string(),
            source_fingerprint: source_fingerprint.to_string(),
            local_tokens,
            projection_generation: self.projection_generation.load(Ordering::SeqCst),
            backend_name,
            source_system_prompt,
            source_tools,
            projection: projection.map(CacheProjectionSnapshot::from_projection),
        });
        self.prompt_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .latest_request = Some(request);
    }

    pub(crate) fn record_agent_usage(&self, usage: &UsageEvent) {
        if usage.input_tokens < 0
            || usage.cache_read_input_tokens < 0
            || usage.cache_creation_input_tokens < 0
        {
            return;
        }
        let Some(provider_prompt_tokens) = usage
            .input_tokens
            .checked_add(usage.cache_read_input_tokens)
            .and_then(|value| value.checked_add(usage.cache_creation_input_tokens))
            .and_then(|value| usize::try_from(value).ok())
        else {
            return;
        };
        let mut state = self
            .prompt_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(request) = state.latest_request.clone() else {
            return;
        };
        if request.projection_generation != self.projection_generation.load(Ordering::SeqCst) {
            return;
        }
        state.baseline = Some(PromptUsageBaseline {
            request,
            provider_prompt_tokens,
        });
    }

    pub(crate) fn clear_prompt_usage(&self) {
        *self
            .prompt_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = PromptUsageState::default();
    }

    pub async fn validate_startup(&self) -> Result<()> {
        let state = self.current_state()?;
        if state.active_start == 0 {
            return Ok(());
        }
        let active = self.store.lines_from(state.active_start).await?;
        if active.is_empty() {
            return Ok(());
        }
        if active.first().is_some_and(is_safe_context_start) {
            return Ok(());
        }
        // Older builds could persist a cut on a runtime-injected user message.
        // Repair by moving the boundary forward to the next safe start (or to
        // an empty active tail) instead of refusing to open the session.
        let repaired_relative = (0..active.len())
            .find(|&index| is_safe_context_start(&active[index]))
            .unwrap_or(active.len());
        let repaired_start = state.active_start.saturating_add(repaired_relative);
        let skipped: Vec<Value> = active[..repaired_relative].to_vec();
        if !skipped.is_empty() {
            // Visibility first: the skipped range is recorded even when the
            // process dies before the next summarization.
            self.write_event(crate::events::EventLog::Compact {
                version: Some(1),
                trigger: "startup_repair".into(),
                result: format!(
                    "skipped {} message(s) from index {} to {} (unsafe legacy boundary)",
                    skipped.len(),
                    state.active_start,
                    repaired_start
                ),
            });
            *self
                .startup_repair_loss
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = skipped;
        }
        let next = CompactionState {
            active_start: repaired_start,
            summary: state.summary,
        };
        self.commit_state(next).await?;
        Ok(())
    }

    pub async fn read_summary(&self) -> Option<String> {
        self.current_summary()
            .ok()
            .flatten()
            .map(|summary| strip_dsml_tags(summary.trim()))
    }

    pub async fn active_messages(&self) -> Result<Vec<Value>> {
        let state = self.current_state()?;
        let mut messages = self.store.lines_from(state.active_start).await?;
        self.prepend_dynamic_checkpoints(&mut messages, &state)?;
        Ok(messages)
    }

    fn prepend_dynamic_checkpoints(
        &self,
        messages: &mut Vec<Value>,
        state: &CompactionState,
    ) -> Result<()> {
        if state.active_start == 0 {
            return Ok(());
        }
        let mut checkpoints = self.checkpoint_prefix(&state.summary)?;
        checkpoints.append(messages);
        *messages = checkpoints;
        Ok(())
    }

    /// 动态 checkpoint 前缀（摘要 + 权威 plan 派生片段）。权威投影与候选验收必须共用
    /// 同一实现，否则候选预算与实际请求会漂移。
    fn checkpoint_prefix(&self, summary: &str) -> Result<Vec<Value>> {
        let mut checkpoints = Vec::new();
        if !summary.trim().is_empty() {
            checkpoints.push(compacted_summary_message(summary));
        }
        if let Some(plan) =
            read_active_plan_checkpoint(&self.summary_path, derived_display_tokens(&self.config))?
        {
            checkpoints.push(plan);
        }
        Ok(checkpoints)
    }

    /// 应急路径的取消/中断检查：runtime cancel 与用户 interrupt 都必须中断提交。
    fn check_emergency_cancelled(&self) -> Result<()> {
        if self.cancel.is_cancelled() || self.interrupt.load(Ordering::SeqCst) {
            return Err(CompactionInterrupted::error());
        }
        Ok(())
    }

    /// 确定性应急 checkpoint：不调用任何 LLM，把可变历史折成有界摘录并提交。
    ///
    /// 只在正常压缩无法让请求装下时由上层调用。`Ok(true)` 表示已提交（调用方必须重新
    /// 投影与重估）；`Ok(false)` 表示连最小摘录都装不下（最小工作空间不可用），调用方
    /// 应以明确诊断 fail-closed。取消、持久化 fault 与正式历史损坏原样传播。
    pub(crate) async fn commit_emergency_checkpoint(
        &self,
        reason: &str,
        ctx: &EmergencyCheckpointContext<'_>,
    ) -> Result<bool> {
        self.fault.check()?;
        self.check_emergency_cancelled()?;
        let _guard = self.compact_lock.lock().await;
        // 等锁期间可能已经取消/中断：拿到锁后必须复检。
        self.check_emergency_cancelled()?;
        let state = self.current_state()?;
        let active = self.store.lines_from(state.active_start).await?;
        // 折叠整段历史前必须确认没有未完成的工具交换：协议损坏不能被摘录掩盖。
        if let Some(problem) = unpaired_tool_exchange(&active) {
            return Err(anyhow::anyhow!(
                "refusing emergency checkpoint: the active history has an incomplete tool exchange ({problem})"
            ));
        }
        let mut blocks = emergency_blocks(
            reason,
            &active,
            &state.summary,
            ctx.current_user_input,
            &ctx.artifact_refs,
        );
        let input_limit = ctx
            .target_limit
            .unwrap_or_else(|| request_input_limit(&self.config));
        loop {
            self.check_emergency_cancelled()?;
            let summary = blocks.render();
            let mut candidate = self.checkpoint_prefix(&summary)?;
            if let Some(todo_sync) = candidate_todo_sync(&candidate, ctx.todo.as_ref()) {
                candidate.push(todo_sync);
            }
            let tokens = ctx.main_request.estimate(&candidate)?;
            if input_limit == usize::MAX || tokens <= input_limit {
                // 提交前最后一次取消/中断复检：取消优先于提交。
                self.check_emergency_cancelled()?;
                // 折掉全部可变历史（尾部为空）；权威 history 只追加、不重写。
                let next = CompactionState {
                    active_start: state.active_start + active.len(),
                    summary: summary.clone(),
                };
                self.commit_state(next).await?;
                *self
                    .startup_repair_loss
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Vec::new();
                let result = format!(
                    "compacted_at_trigger=emergency_kept=0_input_reduction={}_cut=all_mode=emergency_emergency_reason={}",
                    self.config.context_compact_input_reduction,
                    sanitize_field(reason, 96),
                );
                if self.config.log_events {
                    self.write_event(crate::events::EventLog::Compact {
                        version: Some(2),
                        trigger: "emergency".into(),
                        result,
                    });
                }
                self.display.render_info(
                    "Context emergency checkpoint committed (lossy excerpt; details may be omitted).",
                );
                return Ok(true);
            }
            if !blocks.shrink() {
                return Ok(false);
            }
        }
    }

    pub async fn evaluate_and_compact(
        &self,
        trigger: &str,
        context_tokens: usize,
        target: LlmModelTarget<'_>,
    ) -> Result<(bool, String)> {
        self.evaluate_and_compact_with_prefix(
            trigger,
            context_tokens,
            target,
            None,
            None,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // 主请求形状与诊断序号都是必要输入
    pub(crate) async fn evaluate_and_compact_with_prefix(
        &self,
        trigger: &str,
        context_tokens: usize,
        target: LlmModelTarget<'_>,
        source_fingerprint: Option<&str>,
        current_projection: Option<&LlmCacheProjection>,
        // Ordinal of this compaction inside the current user input (1-based),
        // recorded in the `compact` event. `None` for out-of-turn callers.
        compaction_ordinal: Option<usize>,
        // 主请求的真实形状。`Some` 时在提交前做完整投影预算验收（候选装不下则不
        // 提交、报 `SummaryUnavailable`，由上层转应急）；`None`（manual 等旧入口）
        // 保持原有宽松行为。
        main_request: Option<MainRequestShape<'_>>,
        // todo 候选状态：候选折走 revision 时必须把预演 TodoSync 计入验收。
        todo_state: Option<TodoCandidateState<'_>>,
    ) -> Result<(bool, String)> {
        // Latched session: refuse before any summary request or state write.
        self.fault.check()?;
        let _guard = self.compact_lock.lock().await;
        if self.config.max_context_tokens == 0 && matches!(trigger, "auto" | "preflight") {
            return Ok((false, "automatic compaction disabled".into()));
        }
        let pressure = self.pressure_decision(
            trigger,
            context_tokens,
            target.model,
            source_fingerprint,
            current_projection,
        );
        let threshold_tokens = compaction_trigger_tokens(&self.config);
        self.log_compaction_check(trigger, &pressure, context_tokens, threshold_tokens);
        if !is_forced_trigger(trigger) && pressure.effective_tokens < threshold_tokens {
            return Ok((false, "below threshold".into()));
        }

        let state = self.current_state()?;
        let active = self.store.lines_from(state.active_start).await?;
        if active.is_empty() {
            return Ok((false, "empty".into()));
        }

        let tail_target = self.config.context_compact_tail_tokens.max(1);
        let mut cut = find_compaction_cut_point(&active, tail_target);
        // 最后手段（仅在请求已经超出输入预算时启用）：严格规则找不到任何边界
        // ——热尾部目标吞掉整个窗口，或窗口只剩最近两个用户轮次——时，退化为
        // 「折到最新安全边界之前」，折叠内容照常进摘要，当前请求以摘要文本保留，
        // 而不是让整个 turn fail-closed。
        let mut degraded_cut = false;
        if cut == 0 && context_tokens > request_input_limit(&self.config) {
            cut = find_degraded_compaction_cut_point(&active);
            degraded_cut = cut > 0;
        }
        if cut == 0 {
            return Ok((false, "no safe boundary".into()));
        }

        let kept = &active[cut..];
        let total_tokens = estimate_messages_tokens(&active);
        let kept_tokens = estimate_messages_tokens(kept);
        let saved_tokens = total_tokens.saturating_sub(kept_tokens);
        // 最小收益检查：auto 仍要求 ≥10%，避免小上下文的无意义压缩；
        // 强制触发（preflight / overflow）在请求已经超预算时只要真能省就压——
        // 否则「需要的削减量小于 10% 阈值」会变成无法恢复的必然失败。
        let savings_sufficient = if is_forced_trigger(trigger) {
            saved_tokens > 0
        } else {
            (saved_tokens as u128) * 10 >= total_tokens as u128
        };
        if total_tokens == 0 || !savings_sufficient {
            return Ok((false, "savings too small".into()));
        }

        let (summary, summary_meta) = self
            .run_summary_call(
                &active,
                cut,
                (!state.summary.is_empty()).then_some(state.summary.as_str()),
                state.active_start > 0,
                target,
            )
            .await?;
        if self.interrupt.load(Ordering::SeqCst) {
            return Err(CompactionInterrupted::error());
        }

        // 候选发布前完整预算验收：摘要 + 动态 checkpoint + 保留尾部必须能装进主请求
        // 预算，否则不提交（上层可转确定性应急），避免「摘要成功但请求反而变大」在
        // 外层以失败收场。真实的请求形状由调用方给出。
        if let Some(shape) = main_request {
            let mut candidate = self.checkpoint_prefix(&summary)?;
            candidate.extend_from_slice(kept);
            // 候选会折走最新 todo revision：验收必须包含随之出现的 TodoSync，否则
            // 提交后 `reconcile_todo_state` 追加同步消息会再次超窗。
            if let Some(todo_sync) = candidate_todo_sync(&candidate, todo_state.as_ref()) {
                candidate.push(todo_sync);
            }
            let candidate_tokens = shape.estimate(&candidate)?;
            let input_limit = request_input_limit(&self.config);
            if input_limit != usize::MAX && candidate_tokens > input_limit {
                return Err(SummaryUnavailable::error(format!(
                    "compaction candidate remains over the request input budget: {candidate_tokens} > {input_limit} tokens"
                )));
            }
        }

        self.validate_conversation_messages(kept, target.model)?;
        let next = CompactionState {
            active_start: state.active_start + cut,
            summary: summary.clone(),
        };
        self.commit_state(next).await?;
        // The committed summary consumed the startup-repair loss (every
        // summary input includes the current loss); only a successful commit
        // may clear it, so a failed/interrupted attempt keeps it for retry.
        *self
            .startup_repair_loss
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Vec::new();
        let result = format!(
            "compacted_at_trigger={trigger}_kept={}_input_reduction={}_input_mode={}_aligned_messages={}_aligned_estimated_tokens={}_reduced_suffix_messages={}_fallback_reason={}{}{}",
            kept.len(),
            self.config.context_compact_input_reduction,
            summary_meta.input_mode,
            summary_meta.aligned_messages,
            summary_meta.aligned_estimated_tokens,
            summary_meta.reduced_suffix_messages,
            summary_meta.fallback_reason.as_deref().unwrap_or("none"),
            compaction_ordinal
                .map(|ordinal| format!("_compactions_this_turn={ordinal}"))
                .unwrap_or_default(),
            if degraded_cut { "_cut=degraded" } else { "" },
        );
        let result = format!("{result}_mode=summary");
        if self.config.log_events {
            self.write_event(crate::events::EventLog::Compact {
                version: Some(2),
                trigger: trigger.to_string(),
                result: result.clone(),
            });
        }
        Ok((true, result))
    }

    fn pressure_decision(
        &self,
        trigger: &str,
        local_tokens: usize,
        model: &str,
        source_fingerprint: Option<&str>,
        current_projection: Option<&LlmCacheProjection>,
    ) -> PressureDecision {
        if trigger == "preflight" {
            return PressureDecision {
                source: "local_preflight",
                effective_tokens: local_tokens,
                provider_baseline_tokens: None,
            };
        }
        if trigger != "auto" {
            return PressureDecision {
                source: "local_fallback",
                effective_tokens: local_tokens,
                provider_baseline_tokens: None,
            };
        }
        let state = self
            .prompt_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let generation = self.projection_generation.load(Ordering::SeqCst);
        let Some(baseline) = state.baseline.as_ref().filter(|baseline| {
            baseline.request.model == model
                && source_fingerprint
                    .is_some_and(|fingerprint| fingerprint == baseline.request.source_fingerprint)
                && baseline.request.projection_generation == generation
                && self.llm_backend.prompt_usage_calibration_safe()
                && baseline.request.projection.as_ref().is_none_or(|previous| {
                    current_projection
                        .is_some_and(|current| provider_projection_extends(previous, current))
                })
        }) else {
            return PressureDecision {
                source: "local_fallback",
                effective_tokens: local_tokens,
                provider_baseline_tokens: None,
            };
        };
        let calibrated = (baseline.provider_prompt_tokens as i128) + (local_tokens as i128)
            - (baseline.request.local_tokens as i128);
        PressureDecision {
            source: "provider_calibrated",
            effective_tokens: usize::try_from(calibrated.max(0)).unwrap_or(usize::MAX),
            provider_baseline_tokens: Some(baseline.provider_prompt_tokens),
        }
    }

    async fn commit_state(&self, state: CompactionState) -> Result<()> {
        let data = serde_json::to_vec_pretty(&state)?;
        let state_path = self.state_path.clone();
        let fault = self.fault.clone();
        tokio::task::spawn_blocking(move || {
            crate::session::persistence::publish_state(&state_path, &data, &fault)
        })
        .await??;
        *self
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("context state lock poisoned"))? = Ok(state.clone());
        self.store.prune_cache_before(state.active_start).await;
        let summary_path = self.summary_path.clone();
        let summary = format!("{}\n", state.summary);
        let fault = self.fault.clone();
        let projection = tokio::task::spawn_blocking(move || {
            crate::session::persistence::publish_state(&summary_path, summary.as_bytes(), &fault)
        })
        .await;
        // The authoritative context-state is committed, but a failed derived
        // projection must surface as an error instead of silently returning
        // success: the dirty flag only marks it for a later retry.
        let projection_result = match projection {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(join_error) => Err(anyhow::anyhow!(
                "summary projection write task failed: {join_error}"
            )),
        };
        self.projection_dirty
            .store(projection_result.is_err(), Ordering::SeqCst);
        self.memo_epoch.fetch_add(1, Ordering::SeqCst);
        self.projection_generation.fetch_add(1, Ordering::SeqCst);
        projection_result
    }

    pub async fn flush_projection(&self) -> Result<()> {
        if !self.projection_dirty.load(Ordering::SeqCst) {
            return Ok(());
        }
        let state = self.current_state()?;
        let summary_path = self.summary_path.clone();
        let summary = format!("{}\n", state.summary);
        let fault = self.fault.clone();
        tokio::task::spawn_blocking(move || {
            crate::session::persistence::publish_state(&summary_path, summary.as_bytes(), &fault)
        })
        .await??;
        self.projection_dirty.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn current_state(&self) -> Result<CompactionState> {
        self.state
            .read()
            .map_err(|_| anyhow::anyhow!("context state lock poisoned"))?
            .clone()
            .map_err(anyhow::Error::msg)
    }

    fn validate_conversation_messages(&self, messages: &[Value], model: &str) -> Result<()> {
        let _ = crate::llm::transport::build_openai_body(
            model,
            messages,
            &[],
            "",
            effective_max_tokens(&self.config),
        )?;
        Ok(())
    }

    async fn run_summary_call(
        &self,
        active: &[Value],
        cut: usize,
        previous_summary: Option<&str>,
        history_already_compacted: bool,
        target: LlmModelTarget<'_>,
    ) -> Result<(String, SummaryInputMeta)> {
        let request_cancel = self.cancel.linked_child_token();
        let watcher_cancel = request_cancel.clone();
        let cleanup_cancel = request_cancel.clone();
        let interrupt = self.interrupt.clone();
        let watcher = tokio::spawn(async move {
            loop {
                if interrupt.load(Ordering::SeqCst) {
                    watcher_cancel.cancel();
                    return;
                }
                tokio::select! {
                    _ = watcher_cancel.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
                }
            }
        });
        let result = self
            .run_summary_call_with_cancel(
                active,
                cut,
                previous_summary,
                history_already_compacted,
                target,
                request_cancel,
            )
            .await;
        watcher.abort();
        cleanup_cancel.cancel();
        if self.interrupt.load(Ordering::SeqCst) {
            return Err(CompactionInterrupted::error());
        }
        result
    }

    async fn run_summary_call_with_cancel(
        &self,
        active: &[Value],
        cut: usize,
        previous_summary: Option<&str>,
        history_already_compacted: bool,
        target: LlmModelTarget<'_>,
        request_cancel: crate::cancel::CancellationToken,
    ) -> Result<(String, SummaryInputMeta)> {
        use crate::llm::recovery::{self, RequestRetryState, RequestTerminal, UpstreamFailureKind};

        let input = self.build_summary_input(
            active,
            cut,
            previous_summary,
            history_already_compacted,
            target,
        )?;
        let SummaryRequestInput {
            system_prompt,
            tools,
            messages,
            meta,
        } = input;

        // The summary request uses the same request-retry budget and backoff
        // as the main agent, but keeps its own consumption loop: a completed
        // yet unusable summary (empty output, tool call, invalid stop reason)
        // is retried with a purpose correction appended to this temporary
        // request only — never written to the parent conversation.
        let mut retry = RequestRetryState::new(&self.config.llm_recovery);
        // Same deadline for establishment, consumption and every wait.
        let request_deadline = retry.deadline();
        let mut attempt: u32 = 0;
        let mut correction: Option<String> = None;
        loop {
            if request_cancel.is_cancelled() {
                return Err(CompactionInterrupted::error());
            }
            self.fault.check()?;
            if retry.deadline_expired() {
                return Err(SummaryUnavailable::error(format!(
                    "{}: compaction summary deadline exceeded",
                    RequestTerminal::Timeout.message()
                )));
            }
            let attempt_cancel = request_cancel.linked_child_token();
            let mut attempt_messages = messages.clone();
            if let Some(correction) = &correction {
                attempt_messages.push(json!({
                    "role": "user",
                    "internal": true,
                    "content": correction,
                }));
            }
            // 每次 attempt 都重新估算输入并给出动态输出 cap：纠错文本、图片降级或
            // 路径切换都可能改变输入长度，过期的预算检查会发出装不下的请求。
            let attempt_input_tokens = crate::llm::transport::estimate_openai_context_tokens(
                &attempt_messages,
                &tools,
                &system_prompt,
            )?;
            let summary_output_cap = match summary_output_cap(&self.config, attempt_input_tokens) {
                Some(cap) => cap,
                None => {
                    return Err(SummaryUnavailable::error(format!(
                        "compaction summary leaves no practical output budget: input {attempt_input_tokens} tokens of window {}",
                        self.config.max_context_tokens
                    )));
                }
            };
            // Cache-aligned summaries deliberately retain the Agent tool
            // schemas so the provider can reuse the immutable request prefix.
            // Those tools are alignment-only: compaction never executes them.
            let request = LlmRequest {
                purpose: LlmPurpose::Compaction,
                model: target.model.to_string(),
                model_alias: target.alias.map(str::to_string),
                api_url: self.api_url.clone(),
                api_key: self.api_key.clone(),
                system_prompt: system_prompt.clone(),
                messages: attempt_messages,
                tools: tools.clone(),
                max_tokens: summary_output_cap,
                cancel: attempt_cancel.clone(),
                verbose: self.config.verbose,
                display: self.display.clone(),
            };
            let capture = self.usage.capture(
                self.usage
                    .scope(UsageKind::Compaction, self.session_id.clone()),
                target.model.to_string(),
            );
            // First-event and idle deadlines mirror the main request path:
            // the first-event budget covers establishment plus the wait for
            // the first event of this attempt, the idle budget covers gaps
            // between real events. Retry notifications are not progress.
            let attempt_started = std::time::Instant::now();
            let first_event_timeout =
                crate::agent::turn::positive_duration(self.config.llm_first_event_timeout_secs);
            let idle_timeout =
                crate::agent::turn::positive_duration(self.config.llm_idle_timeout_secs);
            let opened = tokio::select! {
                opened = crate::llm::client::guarded_open_stream(
                    &self.llm_backend,
                    request,
                    capture,
                ) => opened,
                _ = request_cancel.cancelled() => return Err(CompactionInterrupted::error()),
                _ = recovery::wait_until(request_deadline) => {
                    return Err(SummaryUnavailable::error(format!(
                        "{}: compaction summary deadline exceeded before the request was established",
                        RequestTerminal::Timeout.message()
                    )));
                }
                _ = wait_first_event_deadline(attempt_started, first_event_timeout) => {
                    let failure = SummaryRetry {
                        retry_after: None,
                        message: format!(
                            "compaction summary first event timeout after {} seconds",
                            first_event_timeout.map_or(0, |timeout| timeout.as_secs())
                        ),
                    };
                    attempt_cancel.cancel();
                    self.wait_summary_retry(&mut retry, attempt, &failure, &request_cancel)
                        .await?;
                    attempt += 1;
                    continue;
                }
            };
            let response = match opened {
                Ok(response) => response,
                Err(error) => {
                    // Close this failed attempt before classifying/backing off:
                    // a custom backend may still hold the attempt token (F7).
                    attempt_cancel.cancel();
                    if request_cancel.is_cancelled() {
                        return Err(CompactionInterrupted::error());
                    }
                    let failure = classify_summary_failure(&error);
                    if let Some(failure) = failure {
                        self.wait_summary_retry(&mut retry, attempt, &failure, &request_cancel)
                            .await?;
                        attempt += 1;
                        continue;
                    }
                    return Err(SummaryUnavailable::error(format!("{error:#}")));
                }
            };

            let consumption = self
                .consume_summary_stream(
                    response.events,
                    &request_cancel,
                    request_deadline,
                    attempt_started,
                    first_event_timeout,
                    idle_timeout,
                )
                .await?;
            attempt_cancel.cancel();
            match consumption {
                SummaryConsumption::Complete {
                    output,
                    stop_reason,
                    invalid_tool_call,
                } => {
                    let summary = strip_dsml_tags(&output);
                    let issue = if let Some((name, id)) = invalid_tool_call {
                        Some(format!(
                            "compaction attempted invalid tool call {name} ({id})"
                        ))
                    } else if !matches!(stop_reason.as_str(), "stop" | "end_turn") {
                        Some(format!("invalid stop reason {stop_reason:?}"))
                    } else if summary.trim().is_empty() {
                        Some("empty response".to_string())
                    } else {
                        None
                    };
                    match issue {
                        None => return Ok((summary.trim().to_string(), meta)),
                        Some(issue) => {
                            // The correction lives only in this temporary
                            // summary request tail.
                            let failure = SummaryRetry {
                                retry_after: None,
                                message: issue.clone(),
                            };
                            self.wait_summary_retry(&mut retry, attempt, &failure, &request_cancel)
                                .await?;
                            attempt += 1;
                            correction = Some(format!(
                                "<summary-retry>Your previous response was rejected: {issue}. Output only the seven required summary fields now, starting directly with Task focus:.</summary-retry>"
                            ));
                            continue;
                        }
                    }
                }
                SummaryConsumption::ProviderError(error) => {
                    let kind = recovery::classify_provider_error(
                        error.provider_code.as_deref(),
                        error.status,
                    );
                    let message = format!("failed to generate context summary: {}", error.message);
                    if kind != UpstreamFailureKind::Recoverable {
                        return Err(SummaryUnavailable::error(message));
                    }
                    let failure = SummaryRetry {
                        retry_after: None,
                        message,
                    };
                    self.wait_summary_retry(&mut retry, attempt, &failure, &request_cancel)
                        .await?;
                    attempt += 1;
                    continue;
                }
                SummaryConsumption::StreamError(error) => {
                    let failure = classify_summary_failure(&error);
                    if let Some(failure) = failure {
                        self.wait_summary_retry(&mut retry, attempt, &failure, &request_cancel)
                            .await?;
                        attempt += 1;
                        continue;
                    }
                    return Err(SummaryUnavailable::error(format!("{error:#}")));
                }
                SummaryConsumption::Deadline => {
                    return Err(SummaryUnavailable::error(format!(
                        "{}: compaction summary deadline exceeded while streaming",
                        RequestTerminal::Timeout.message()
                    )));
                }
                SummaryConsumption::Interrupted => return Err(CompactionInterrupted::error()),
            }
        }
    }

    /// Consume one compaction attempt without committing anything. `deadline`
    /// is the optional total request deadline shared with establishment and
    /// the retry waits; `attempt_started`/`first_event_timeout`/`idle_timeout`
    /// are the per-attempt first-event and idle deadlines.
    async fn consume_summary_stream(
        &self,
        mut stream: crate::llm::client::LlmEventStream,
        request_cancel: &crate::cancel::CancellationToken,
        deadline: Option<std::time::Instant>,
        attempt_started: std::time::Instant,
        first_event_timeout: Option<std::time::Duration>,
        idle_timeout: Option<std::time::Duration>,
    ) -> Result<SummaryConsumption> {
        let mut output = String::new();
        let mut stop_reason = String::new();
        let mut invalid_tool_call = None;
        // Retry notifications do not count as the first event, nor do they
        // refresh the idle clock (mirrors the main request path).
        let mut saw_any_event = false;
        let mut last_event_at = std::time::Instant::now();
        loop {
            if let Some(consumption) = summary_wait_timeout(
                saw_any_event,
                attempt_started,
                last_event_at,
                first_event_timeout,
                idle_timeout,
            ) {
                return Ok(consumption);
            }
            let event = tokio::select! {
                event = futures::StreamExt::next(&mut stream) => event,
                _ = request_cancel.cancelled() => return Ok(SummaryConsumption::Interrupted),
                _ = crate::llm::recovery::wait_until(deadline) => {
                    return Ok(SummaryConsumption::Deadline)
                }
                // Deterministic wake-up so a pending stream still hits the
                // deadline checks above (a stream of events arriving faster
                // than this tick cannot starve them either: the checks run on
                // the loop path).
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => continue,
            };
            let Some(event) = event else { break };
            match event {
                Ok(Event::Text(TextEvent { content })) => {
                    saw_any_event = true;
                    last_event_at = std::time::Instant::now();
                    output.push_str(&content);
                }
                Ok(Event::Usage(usage)) => {
                    saw_any_event = true;
                    last_event_at = std::time::Instant::now();
                    self.log_compact_event(&usage);
                    self.stats.record_compact(&usage).await;
                }
                Ok(Event::UsageUnavailable) => {}
                Ok(Event::Error(error)) => return Ok(SummaryConsumption::ProviderError(error)),
                Ok(Event::Stop(StopEvent { reason })) => {
                    stop_reason = reason;
                    break;
                }
                Ok(Event::ToolCall(call)) if invalid_tool_call.is_none() => {
                    saw_any_event = true;
                    last_event_at = std::time::Instant::now();
                    invalid_tool_call = Some((call.name, call.id));
                }
                Ok(Event::Retry(_)) => {
                    // The backend restarted this logical request: partial
                    // output from the aborted attempt must not leak. The
                    // notification is not progress and does not refresh the
                    // idle clock.
                    output.clear();
                    stop_reason.clear();
                    invalid_tool_call = None;
                }
                Ok(_) => {
                    saw_any_event = true;
                    last_event_at = std::time::Instant::now();
                }
                Err(error) => return Ok(SummaryConsumption::StreamError(error)),
            }
        }
        Ok(SummaryConsumption::Complete {
            output,
            stop_reason,
            invalid_tool_call,
        })
    }

    /// Bounded wait before retrying one compaction attempt; fails with the
    /// stable terminal reason when the budget/deadline is exhausted.
    async fn wait_summary_retry(
        &self,
        retry: &mut crate::llm::recovery::RequestRetryState,
        attempt: u32,
        failure: &SummaryRetry,
        request_cancel: &crate::cancel::CancellationToken,
    ) -> Result<()> {
        use crate::llm::recovery::{self, RequestTerminal};
        if !retry.can_retry() {
            return Err(SummaryUnavailable::error(format!(
                "{}: {}",
                RequestTerminal::RetryExhausted.message(),
                failure.message
            )));
        }
        let wait = match retry.retry_wait(attempt, failure.retry_after, recovery::jitter_fraction())
        {
            Ok(wait) => wait,
            Err(_) => {
                return Err(SummaryUnavailable::error(format!(
                    "{}: {}",
                    RequestTerminal::Timeout.message(),
                    failure.message
                )));
            }
        };
        retry.note_retry();
        tokio::select! {
            _ = tokio::time::sleep(wait) => Ok(()),
            _ = request_cancel.cancelled() => Err(CompactionInterrupted::error()),
        }
    }

    fn build_summary_input(
        &self,
        active: &[Value],
        cut: usize,
        previous_summary: Option<&str>,
        history_already_compacted: bool,
        target: LlmModelTarget<'_>,
    ) -> Result<SummaryRequestInput> {
        let dropped = &active[..cut];
        let mut source_messages = Vec::new();
        if let Some(summary) = previous_summary.filter(|summary| !summary.trim().is_empty()) {
            source_messages.push(compacted_summary_message(summary));
        }
        if history_already_compacted
            && let Some(plan) = read_active_plan_checkpoint(
                &self.summary_path,
                derived_display_tokens(&self.config),
            )?
        {
            source_messages.push(plan);
        }
        // Startup boundary repair removed these messages from the projection;
        // include them in every summary attempt until one actually commits.
        // Taking them here would re-lose them when the request fails.
        let repair_loss = self
            .startup_repair_loss
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        source_messages.extend(repair_loss.iter().cloned());
        let dynamic_prefix_len = source_messages.len();
        source_messages.extend(crate::llm::image_projection::project_consumed_attachments(
            active,
        ));
        let source_prefix_len = dynamic_prefix_len.saturating_add(cut);

        let latest = self
            .prompt_usage
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .latest_request
            .clone();
        let fallback_reason;
        if let Some(latest) = latest {
            let aligned = (|| -> Result<SummaryRequestInput, String> {
                if latest.model != target.model {
                    return Err("model_changed".into());
                }
                if latest.backend_name != self.llm_backend.name() {
                    return Err("cache_domain_changed".into());
                }
                if latest.projection_generation != self.projection_generation.load(Ordering::SeqCst)
                {
                    return Err("projection_generation_changed".into());
                }
                let Some(recent_projection) = latest.projection.as_ref() else {
                    return Err("backend_projection_unavailable".into());
                };
                let source_request = LlmRequest {
                    purpose: LlmPurpose::Agent,
                    model: target.model.to_string(),
                    model_alias: target.alias.map(str::to_string),
                    api_url: self.api_url.clone(),
                    api_key: self.api_key.clone(),
                    system_prompt: latest.source_system_prompt.clone(),
                    messages: source_messages.clone(),
                    tools: latest.source_tools.clone(),
                    max_tokens: effective_max_tokens(&self.config),
                    cancel: self.cancel.clone(),
                    verbose: self.config.verbose,
                    display: self.display.clone(),
                };
                let Some(candidate) = self
                    .llm_backend
                    .cache_projection(&source_request, source_prefix_len)
                else {
                    return Err("backend_projection_unavailable".into());
                };
                if candidate.model != recent_projection.model {
                    return Err("model_changed".into());
                }
                if candidate.system_prompt != recent_projection.system_prompt
                    || candidate.tools != recent_projection.tools
                {
                    return Err("system_tools_changed".into());
                }
                if candidate.messages.len() != source_prefix_len {
                    return Err("source_boundary_unproven".into());
                }
                let candidate_hashes = message_hashes(&candidate.messages);
                let aligned_messages = candidate_hashes
                    .iter()
                    .zip(&recent_projection.message_hashes)
                    .take_while(|(left, right)| left == right)
                    .count();
                let aligned_messages = rollback_incomplete_tool_exchange_boundary(
                    &candidate.messages,
                    aligned_messages,
                );
                if aligned_messages == 0 {
                    return Err("no_safe_message_prefix".into());
                }
                let mut messages = candidate.messages[..aligned_messages].to_vec();
                let suffix = &candidate.messages[aligned_messages..];
                let reduced_suffix_messages = suffix.len();
                if !suffix.is_empty() {
                    if self.config.context_compact_input_reduction {
                        messages.push(json!({
                            "role": "user",
                            "internal": true,
                            "content": format!(
                                "<compaction-uncached-suffix>\n{}\n</compaction-uncached-suffix>",
                                compaction_input::reduce_for_summary(suffix)
                            ),
                        }));
                    } else {
                        let mut raw_suffix = suffix.to_vec();
                        crate::llm::image_projection::degrade_images_for_summary(&mut raw_suffix);
                        messages.extend(raw_suffix);
                    }
                }
                let aligned_estimated_tokens =
                    crate::llm::transport::estimate_openai_context_tokens(
                        &candidate.messages[..aligned_messages],
                        &candidate.tools,
                        &candidate.system_prompt,
                    )
                    .map_err(|error| format!("aligned_estimate_failed:{error}"))?;
                messages.push(compaction_instruction_message());
                if summary_input_over_budget(
                    &self.config,
                    &messages,
                    &candidate.tools,
                    &candidate.system_prompt,
                    usize::try_from(compaction_max_output_tokens(&self.config)).unwrap_or(0),
                )
                .map_err(|error| format!("aligned_estimate_failed:{error}"))?
                {
                    return Err("aligned_input_over_budget".into());
                }
                Ok(SummaryRequestInput {
                    system_prompt: candidate.system_prompt,
                    tools: candidate.tools,
                    messages,
                    meta: SummaryInputMeta {
                        input_mode: if aligned_messages == candidate.messages.len() {
                            "cache_aligned"
                        } else {
                            "partial_aligned"
                        },
                        aligned_messages,
                        aligned_estimated_tokens,
                        reduced_suffix_messages,
                        fallback_reason: None,
                    },
                })
            })();
            match aligned {
                Ok(input) => return Ok(input),
                Err(reason) => fallback_reason = Some(reason),
            }
        } else {
            fallback_reason = Some("no_recent_agent_request".into());
        }

        let mut history = Vec::new();
        if let Some(summary) = previous_summary.filter(|summary| !summary.trim().is_empty()) {
            history.push(compacted_summary_message(summary));
        }
        // Also present in the fallback (no recent agent request): startup
        // repair losses must reach the summary whichever path is taken.
        history.extend(repair_loss);
        history.extend(crate::llm::image_projection::project_consumed_attachments(
            dropped,
        ));
        crate::llm::image_projection::degrade_images_for_summary(&mut history);
        let input_mode = if self.config.context_compact_input_reduction {
            "reduced"
        } else {
            "raw"
        };
        let mut messages = if self.config.context_compact_input_reduction {
            vec![json!({
                "role": "user",
                "internal": true,
                "content": compaction_input::reduce_for_summary(&history),
            })]
        } else {
            history
        };
        messages.push(compaction_instruction_message());
        if summary_input_over_budget(
            &self.config,
            &messages,
            &[],
            FALLBACK_SYSTEM_PROMPT,
            MIN_PRACTICAL_SUMMARY_TOKENS,
        )? {
            return Err(SummaryUnavailable::error(
                "compaction summary input exceeds configured budget",
            ));
        }
        Ok(SummaryRequestInput {
            system_prompt: FALLBACK_SYSTEM_PROMPT.to_string(),
            tools: Vec::new(),
            messages,
            meta: SummaryInputMeta {
                input_mode,
                aligned_messages: 0,
                aligned_estimated_tokens: 0,
                reduced_suffix_messages: dropped.len(),
                fallback_reason,
            },
        })
    }

    fn log_compaction_check(
        &self,
        trigger: &str,
        pressure: &PressureDecision,
        local_tokens: usize,
        threshold_tokens: usize,
    ) {
        if !self.config.log_events {
            return;
        }
        self.write_event(crate::events::EventLog::CompactionCheck {
            trigger: trigger.to_string(),
            pressure_source: pressure.source.to_string(),
            local_tokens,
            provider_baseline_tokens: pressure.provider_baseline_tokens,
            calibrated_tokens: (pressure.source == "provider_calibrated")
                .then_some(pressure.effective_tokens),
            threshold_tokens,
            projection_generation: self.projection_generation.load(Ordering::SeqCst),
        });
    }

    fn write_event(&self, event: crate::events::EventLog) {
        let Ok(line) = serde_json::to_string(&event) else {
            return;
        };
        if let Some(writer) = &self.event_log_writer {
            writer.send(line);
        }
    }

    fn log_compact_event(&self, usage: &UsageEvent) {
        if !self.config.log_events {
            return;
        }
        self.write_event(crate::events::EventLog::usage(usage, "compact"));
    }
}

pub(crate) fn prefix_fingerprint(system_prompt: &str, tools: &[Value]) -> String {
    crate::session::prefix::ImmutablePrefix::compute_fingerprint(system_prompt, tools, None)
}

/// Outcome of consuming one compaction attempt.
enum SummaryConsumption {
    Complete {
        output: String,
        stop_reason: String,
        invalid_tool_call: Option<(String, String)>,
    },
    ProviderError(ErrorEvent),
    StreamError(anyhow::Error),
    /// The optional total request deadline elapsed while the response was in
    /// flight; the logical compaction request is over.
    Deadline,
    Interrupted,
}

/// One retryable compaction failure: a typed upstream failure or an unusable
/// completed summary (empty output, tool call, invalid stop reason).
struct SummaryRetry {
    retry_after: Option<std::time::Duration>,
    message: String,
}

/// Sleep until the attempt's first-event deadline; without a configured
/// timeout this future never resolves (uniform `select!` shape).
async fn wait_first_event_deadline(
    attempt_started: std::time::Instant,
    first_event_timeout: Option<std::time::Duration>,
) {
    match first_event_timeout {
        Some(timeout) => tokio::time::sleep_until((attempt_started + timeout).into()).await,
        None => std::future::pending::<()>().await,
    }
}

/// Per-attempt first-event / idle deadline check shared by the compaction
/// consumption loop. Returns a retryable stream failure once a deadline is
/// exceeded so the existing retry budget applies.
fn summary_wait_timeout(
    saw_any_event: bool,
    attempt_started: std::time::Instant,
    last_event_at: std::time::Instant,
    first_event_timeout: Option<std::time::Duration>,
    idle_timeout: Option<std::time::Duration>,
) -> Option<SummaryConsumption> {
    let now = std::time::Instant::now();
    if !saw_any_event {
        if let Some(timeout) = first_event_timeout
            && now.duration_since(attempt_started) >= timeout
        {
            return Some(SummaryConsumption::StreamError(anyhow::Error::new(
                crate::llm::recovery::LlmUpstreamError::recoverable(format!(
                    "compaction summary first event timeout after {} seconds",
                    timeout.as_secs()
                )),
            )));
        }
        return None;
    }
    if let Some(timeout) = idle_timeout
        && now.duration_since(last_event_at) >= timeout
    {
        return Some(SummaryConsumption::StreamError(anyhow::Error::new(
            crate::llm::recovery::LlmUpstreamError::recoverable(format!(
                "compaction summary idle timeout after {} seconds without events",
                timeout.as_secs()
            )),
        )));
    }
    None
}

/// Classify a compaction attempt failure. `None` means permanent/fatal (the
/// caller returns the original error).
fn classify_summary_failure(error: &anyhow::Error) -> Option<SummaryRetry> {
    use crate::llm::recovery::{UpstreamFailureKind, find_upstream_failure};
    let upstream = find_upstream_failure(error)?;
    match upstream.kind() {
        UpstreamFailureKind::Permanent => None,
        UpstreamFailureKind::Recoverable | UpstreamFailureKind::ProtocolDamaged => {
            Some(SummaryRetry {
                retry_after: upstream.retry_after(),
                message: format!("{error:#}"),
            })
        }
    }
}

fn provider_projection_extends(
    previous: &CacheProjectionSnapshot,
    current: &LlmCacheProjection,
) -> bool {
    previous.model == current.model
        && previous.system_prompt == current.system_prompt
        && previous.tools == current.tools
        && previous.message_hashes.len() <= current.messages.len()
        && previous
            .message_hashes
            .iter()
            .zip(message_hashes(&current.messages))
            .all(|(left, right)| left == &right)
}

impl CacheProjectionSnapshot {
    fn from_projection(projection: LlmCacheProjection) -> Self {
        Self {
            model: projection.model,
            system_prompt: projection.system_prompt,
            tools: projection.tools,
            message_hashes: message_hashes(&projection.messages),
        }
    }
}

fn load_state(path: &Path) -> Result<CompactionState> {
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
pub(super) const COMPACTION_MIN_TAIL_USER_MESSAGES: usize = 2;

/// Engine-injected user-role messages that must not count as real user
/// constraints for the compaction guard. The primary signal is the `internal`
/// (or `_mink`) metadata flag set by `add_runtime_user` / todo sync; the
/// string markers below remain as a defensive fallback for historical or
/// third-party messages that predate the flag.
pub(super) const RUNTIME_INJECTED_MARKERS: &[&str] = &[
    "<todo-progress-reminder>",
    "<todo-final-reminder>",
    "<todo-sync",
    "[System note:",
];

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
