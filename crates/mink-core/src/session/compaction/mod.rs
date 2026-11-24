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

mod emergency;
#[cfg(test)]
pub(super) use emergency::{EMERGENCY_REQUEST_BUDGET_BYTES, last_real_user_text};
use emergency::{emergency_blocks, sanitize_field};

mod cache;
mod summary;
mod types;

#[cfg(test)]
use cache::CacheProjectionSnapshot;
use cache::PromptUsageState;
pub(crate) use cache::prefix_fingerprint;
use cache::provider_projection_extends;
pub(crate) use summary::COMPACTION_INSTRUCTION;
#[allow(unused_imports)]
pub(crate) use types::{
    COMPACTION_MIN_TAIL_USER_MESSAGES, CompactionDeadlineExpired, CompactionInterrupted,
    EmergencyCheckpointContext, MainRequestShape, RUNTIME_INJECTED_MARKERS, SummaryUnavailable,
    TodoCandidateState, candidate_todo_sync, estimate_candidate_tokens,
    is_compaction_deadline_expired, is_compaction_interrupted, is_summary_unavailable,
    shared_deadline_expired, summary_unavailable_reason,
};
use types::{
    CompactionState, PressureDecision, SummaryInputMeta, SummaryRequestInput, load_state,
    map_shared_deadline, unpaired_tool_exchange,
};

pub struct CompactionEngine {
    goal_store: Option<Arc<crate::session::goal::GoalStore>>,
    review_session: Option<Arc<crate::runtime::goal::review::GoalReviewSession>>,
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
    interrupt: crate::cancel::InterruptSignal,
    state: RwLock<std::result::Result<CompactionState, String>>,
    compact_lock: tokio::sync::Mutex<()>,
    memo_epoch: Arc<AtomicU64>,
    projection_generation: AtomicU64,
    prompt_usage: Mutex<PromptUsageState>,
    projection_dirty: AtomicBool,
    event_log_writer: Option<EventLogWriter>,
    /// Optional live observer for the durable event vocabulary. Kept beside the
    /// writer so compaction events use the same outlet as the rest of the
    /// session instead of bypassing the sink.
    session_event_sink: Option<Arc<dyn crate::runtime::SessionEventSink>>,
    /// Shared session publish-fault latch for context-state and projection
    /// writes.
    fault: crate::session::persistence::PersistenceFault,
    /// Messages skipped by startup boundary repair; carried into the next
    /// summarization input so they are not silently lost.
    startup_repair_loss: Mutex<Vec<Value>>,
}

pub(crate) use crate::session::compaction_cut::*;

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
        interrupt: crate::cancel::InterruptSignal,
        llm_backend: Arc<dyn LlmBackend>,
        event_log_writer: Option<EventLogWriter>,
        session_event_sink: Option<Arc<dyn crate::runtime::SessionEventSink>>,
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
            goal_store: None,
            review_session: None,
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
            session_event_sink,
            fault: Default::default(),
            startup_repair_loss: Mutex::new(Vec::new()),
        })
    }

    /// Attach the current goal's bounded checkpoint projection.
    pub(crate) fn with_goal_store(
        mut self,
        store: Option<Arc<crate::session::goal::GoalStore>>,
    ) -> Self {
        self.goal_store = store;
        self
    }
    /// Attach the parent budget owner when this compactor belongs to a Goal reviewer.
    pub(crate) fn with_review_session(
        mut self,
        review: Option<Arc<crate::runtime::goal::review::GoalReviewSession>>,
    ) -> Self {
        self.review_session = review;
        self
    }
    /// Attach the session-wide publish-fault latch.
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
        self.commit_state(next, false).await?;
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

    /// 动态 checkpoint 前缀（摘要 + 权威 plan/goal 派生片段）。权威投影与候选验收必须共用
    /// 同一实现，否则候选预算与实际请求会漂移。
    ///
    /// 只有压缩后的窗口才挂这些 checkpoint：未压缩时请求就是历史本身，
    /// goal 的每轮事实走尾部追加的 round prompt，绝不改写已经发出的前缀。
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
        if let Some(goals) = &self.goal_store
            && let Some(checkpoint) = goals.checkpoint(
                crate::runtime::goal::goal_projection_chars(self.config.max_context_tokens),
            )?
        {
            checkpoints.push(checkpoint);
        }
        Ok(checkpoints)
    }

    /// 应急路径的取消/中断检查：runtime cancel 与用户 interrupt 都必须中断提交。
    fn check_emergency_cancelled(&self) -> Result<()> {
        if self.cancel.is_cancelled() || self.interrupt.is_set() {
            return Err(CompactionInterrupted::error());
        }
        Ok(())
    }

    /// 应急路径的共享期限检查：期限耗尽时不得发布新的有损 checkpoint。
    /// 取消优先于期限（调用方先调 [`Self::check_emergency_cancelled`]）。
    fn check_emergency_deadline(&self, deadline: Option<std::time::Instant>) -> Result<()> {
        if shared_deadline_expired(deadline) {
            return Err(CompactionDeadlineExpired::error());
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
        self.check_emergency_deadline(ctx.deadline)?;
        let _guard = self.compact_lock.lock().await;
        // 等锁期间可能已经取消/中断或超过共享期限：拿到锁后必须复检。
        self.check_emergency_cancelled()?;
        self.check_emergency_deadline(ctx.deadline)?;
        let state = self.current_state()?;
        let active = self.store.lines_from(state.active_start).await?;
        // 折叠整段历史前必须确认没有未完成的工具交换：协议损坏不能被摘录掩盖。
        if let Some(problem) = unpaired_tool_exchange(&active) {
            return Err(anyhow::anyhow!(
                "refusing emergency checkpoint: the active history has an incomplete tool exchange ({problem})"
            ));
        }
        let repair_loss = self
            .startup_repair_loss
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let mut blocks = emergency_blocks(
            reason,
            &active,
            &state.summary,
            ctx.current_user_input,
            &ctx.artifact_refs,
            &repair_loss,
        );
        let input_limit = ctx
            .target_limit
            .unwrap_or_else(|| request_input_limit(&self.config));
        // Pre-read authoritative plan & goal checkpoints once outside the shrink
        // loop instead of re-reading `plan.md` from disk on every iteration.
        let static_tail_checkpoints = {
            let mut extra = Vec::new();
            if let Some(plan) = read_active_plan_checkpoint(
                &self.summary_path,
                derived_display_tokens(&self.config),
            )? {
                extra.push(plan);
            }
            if let Some(goals) = &self.goal_store
                && let Some(checkpoint) = goals.checkpoint(
                    crate::runtime::goal::goal_projection_chars(self.config.max_context_tokens),
                )?
            {
                extra.push(checkpoint);
            }
            extra
        };
        loop {
            self.check_emergency_cancelled()?;
            self.check_emergency_deadline(ctx.deadline)?;
            let summary = blocks.render();
            // 候选与真实请求同一投影（含预演 TodoSync）；应急折叠后尾部为空。
            let mut candidate = Vec::with_capacity(1 + static_tail_checkpoints.len());
            if !summary.trim().is_empty() {
                candidate.push(compacted_summary_message(&summary));
            }
            candidate.extend(static_tail_checkpoints.iter().cloned());
            let tokens =
                estimate_candidate_tokens(ctx.main_request, &candidate, ctx.todo.as_ref())?;
            if input_limit == usize::MAX || tokens <= input_limit {
                // 提交前最后一次取消/中断/期限复检：取消优先于提交。
                self.check_emergency_cancelled()?;
                self.check_emergency_deadline(ctx.deadline)?;
                // 折掉全部可变历史（尾部为空）；权威 history 只追加、不重写。
                let next = CompactionState {
                    active_start: state.active_start + active.len(),
                    summary: summary.clone(),
                };
                self.commit_state(next, true).await?;
                let result = format!(
                    "compacted_at_trigger=emergency_kept=0_input_reduction={}_cut=all_mode=emergency_emergency_reason={}",
                    self.config.context_compact_input_reduction,
                    sanitize_field(reason, 96),
                );
                self.write_event(crate::events::EventLog::Compact {
                    version: Some(2),
                    trigger: "emergency".into(),
                    result,
                });
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

    /// Legacy relaxed entry point (no main-request shape): test-only. Production
    /// callers must use [`Self::evaluate_and_compact_with_prefix`] so candidate
    /// acceptance and manual compaction share the real request budget.
    #[cfg(test)]
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
        compaction_ordinal: Option<usize>,
        main_request: Option<MainRequestShape<'_>>,
        todo_state: Option<TodoCandidateState<'_>>,
    ) -> Result<(bool, String)> {
        self.evaluate_and_compact_with_prefix_bounded(
            trigger,
            context_tokens,
            target,
            source_fingerprint,
            current_projection,
            compaction_ordinal,
            main_request,
            todo_state,
            None,
        )
        .await
    }

    /// [`Self::evaluate_and_compact_with_prefix`] 带 round 绝对期限：摘要请求取
    /// 「自身预算与剩余总期限的较小值」，同一逻辑请求内的 overflow 恢复不得延长总期限。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn evaluate_and_compact_with_prefix_bounded(
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
        round_deadline: Option<std::time::Instant>,
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

        let limit = request_input_limit(&self.config);
        let tail_target = self.planned_tail_target(main_request)?;
        let mut cut = find_compaction_cut_point(&active, tail_target);
        // 预算可行性选择（仅在请求已经超出输入预算时启用）：
        // - 严格切点（含保留最近两条真实 user 的守卫）的候选连「固定前缀 + 派生展示 +
        //   预演 TodoSync + 最小摘要位」都放不下时，不再直接报 SummaryUnavailable/
        //   转应急，而是按安全边界退化为「折到最新安全边界之前」（`_cut=degraded`）。
        // - 未超预算时仍保护 user 守卫，交给候选验收报错。
        // - degraded 只能在安全边界真的更靠后时生效，不额外丢失逐字上下文。
        let mut degraded_cut = false;
        let over_budget = limit != usize::MAX && context_tokens > limit;
        if over_budget {
            let strict_fits = match main_request {
                Some(shape) if cut > 0 => {
                    // 复用真实候选前缀（含 active-plan checkpoint）与摘要消息包装，
                    // 再为最小可用摘要正文预留空间：否则「裸尾部」看似可行，加上
                    // Plan 包装后才发现超额，白白丢掉可行的 degraded 摘要机会。
                    let mut candidate = self.checkpoint_prefix("")?;
                    candidate.push(compacted_summary_message(
                        &"x".repeat(MIN_PRACTICAL_SUMMARY_TOKENS.saturating_mul(3)),
                    ));
                    candidate.extend_from_slice(&active[cut..]);
                    let candidate_tokens =
                        estimate_candidate_tokens(shape, &candidate, todo_state.as_ref())?;
                    candidate_tokens <= limit
                }
                _ => cut > 0,
            };
            if !strict_fits {
                let degraded = find_degraded_compaction_cut_point(&active);
                if degraded > cut {
                    cut = degraded;
                    degraded_cut = true;
                }
            }
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
                round_deadline,
            )
            .await
            .map_err(|error| map_shared_deadline(error, round_deadline))?;
        if self.interrupt.is_set() {
            return Err(CompactionInterrupted::error());
        }

        // 候选发布前完整预算验收：摘要 + 动态 checkpoint + 保留尾部必须能装进主请求
        // 预算，否则不提交（上层可转确定性应急），避免「摘要成功但请求反而变大」在
        // 外层以失败收场。真实的请求形状由调用方给出。
        let mut net_after: Option<usize> = None;
        if let Some(shape) = main_request {
            let mut candidate = self.checkpoint_prefix(&summary)?;
            candidate.extend_from_slice(kept);
            // 候选会折走最新 todo revision：验收必须包含随之出现的 TodoSync，并与
            // 真实请求共用完整投影（图片单次消费等），否则提交后消息会再次超窗。
            let candidate_tokens =
                estimate_candidate_tokens(shape, &candidate, todo_state.as_ref())?;
            net_after = Some(candidate_tokens);
            let input_limit = request_input_limit(&self.config);
            if input_limit != usize::MAX && candidate_tokens > input_limit {
                return Err(SummaryUnavailable::error(format!(
                    "compaction candidate remains over the request input budget: {candidate_tokens} > {input_limit} tokens"
                )));
            }
        }
        // 净收益门控：不得发布比原请求更大的候选（auto 要求 ≥10% 净下降）。
        if let Some(after) = net_after
            && !net_benefit_sufficient(trigger, context_tokens, after)
        {
            return Ok((
                false,
                format!("summary has no net benefit: {context_tokens} -> {after} tokens"),
            ));
        }

        self.validate_conversation_messages(kept, target.model)?;
        // 发布前共享期限复检：摘要生成可能跨过 round 期限，超时后不得再提交。
        if shared_deadline_expired(round_deadline) {
            return Err(CompactionDeadlineExpired::error());
        }
        let next = CompactionState {
            active_start: state.active_start + cut,
            summary: summary.clone(),
        };
        self.commit_state(next, true).await?;
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
        self.write_event(crate::events::EventLog::Compact {
            version: Some(2),
            trigger: trigger.to_string(),
            result: result.clone(),
        });
        Ok((true, result))
    }

    /// 热尾部软目标：`min(配置 tail, 可用空间)`。可用空间从主请求输入预算中扣除
    /// 固定前缀、派生展示预留与目标摘要空间；没有主请求形状或无界窗口时用配置值。
    fn planned_tail_target(&self, main_request: Option<MainRequestShape<'_>>) -> Result<usize> {
        let configured = self.config.context_compact_tail_tokens.max(1);
        let Some(shape) = main_request else {
            return Ok(configured);
        };
        let limit = request_input_limit(&self.config);
        if limit == usize::MAX {
            return Ok(configured);
        }
        let fixed = shape.estimate(&[])?;
        let derived = derived_display_tokens(&self.config);
        let summary_space =
            usize::try_from(compaction_max_output_tokens(&self.config)).unwrap_or(0);
        let available = limit
            .saturating_sub(fixed)
            .saturating_sub(derived)
            .saturating_sub(summary_space);
        Ok(configured.min(available))
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

    async fn commit_state(&self, state: CompactionState, clear_repair_loss: bool) -> Result<()> {
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
        // The authoritative `context-state.json` is now durably committed with
        // the summary that consumed `startup_repair_loss`; clear it here so a
        // failure writing the derived `summary.md` cannot re-inject the same
        // loss on the next compaction. Startup repair commits pass `false`
        // because they only record the loss for the next summarization.
        if clear_repair_loss {
            *self
                .startup_repair_loss
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Vec::new();
        }
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

    fn log_compaction_check(
        &self,
        trigger: &str,
        pressure: &PressureDecision,
        local_tokens: usize,
        threshold_tokens: usize,
    ) {
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
        // Observability is independent of disk logging: a host that runs with
        // `log_events=false` still receives compaction diagnostics through the
        // sink. The writer remains the optional durable outlet.
        if let Some(sink) = &self.session_event_sink {
            let session_id = self.session_id.as_str();
            if sink.wants(session_id, &event) {
                sink.on_session_event(session_id, &event);
            }
        }
        if !self.config.log_events {
            return;
        }
        let Some(writer) = &self.event_log_writer else {
            return;
        };
        let Ok(line) = serde_json::to_string(&event) else {
            return;
        };
        writer.send_best_effort(line);
    }

    fn log_compact_event(&self, usage: &UsageEvent) {
        self.write_event(crate::events::EventLog::usage(usage, "compact"));
    }
}

#[cfg(test)]
#[path = "../compaction_tests.rs"]
mod tests;
