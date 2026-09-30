use crate::agent::text::truncate_str;
use crate::context::AgentSharedContext;
use crate::llm::client::LlmModelTarget;
use crate::protocol::{Event, ToolCallEvent, UsageEvent};
use crate::session::store::{build_tool_call_summary, first_line};
use crate::sse::toolcall::build_tool_call_event;
use crate::tools::runner::ToolRunner;
use crate::ui::ToolResultDisplay;
use anyhow::Result;
use futures::StreamExt;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

mod format_recovery;
mod recovery;
mod stream;
mod tools;

/// 存储 LLM 流式响应阶段（Phase 1）的输出。
struct StreamOutput {
    text: String,
    thinking: String,
    calls: Vec<ToolCallEvent>,
    stop: String,
    usage: Option<UsageEvent>,
    /// The round must terminate as a format failure with no accepted
    /// candidate (protocol-damaged attempts already exhausted the window).
    format_abort: Option<String>,
    /// A discarded attempt in this round was response-protocol damaged.
    had_format_error: bool,
}

impl StreamOutput {
    fn format_abort(detail: String) -> Self {
        Self {
            text: String::new(),
            thinking: String::new(),
            calls: Vec::new(),
            stop: String::new(),
            usage: None,
            format_abort: Some(detail),
            had_format_error: true,
        }
    }
}

#[derive(Debug)]
struct ContextOverflowError {
    message: String,
}

impl std::fmt::Display for ContextOverflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ContextOverflowError {}

/// 用户在请求建立阶段（等待响应头 / 自定义 backend 的 `stream()`
/// 尚未返回）中断。调用方必须映射为 `TurnDecision::Interrupted`，
/// 不能降级为普通网络失败。
#[derive(Debug)]
pub(crate) struct TurnInterrupted;

impl std::fmt::Display for TurnInterrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("turn interrupted during request establishment")
    }
}

impl std::error::Error for TurnInterrupted {}

/// Per-user-input state. Every field is reset between inputs by replacing
/// the whole value with `Default::default()`, so adding a new field cannot be
/// forgotten in a hand-maintained reset list.
#[derive(Default)]
struct TurnLocalState {
    tool_call_count: u32,
    /// Set after a signal injection. The next tool batch must observe before mutating.
    signal_recovery_guard: bool,
    /// 恢复守卫连续拦截计数（达到 guard_max_blocks 后绕过守卫并强制证据注入）。
    guard_blocks: usize,
    /// 守卫已达上限被绕过：下一次决策强制注入证据（即使处于冷却）。
    guard_bypassed: bool,
    /// 本输入内 Warning 级响应的累计次数；连续两次触发策略重启。
    warning_count: usize,
    /// 本输入内已尝试的策略重启次数。
    replan_attempts: usize,
    /// 当前用户输入原文，供恢复任务报告引用。
    current_user_input: String,
    todo_final_reminder_sent: bool,
    todo_progress_reminder_sent: bool,
    successful_work_calls_since_todo_advance: u32,
    final_text: String,
    final_thinking: String,
    /// 当前用户输入内的 round 序号（1-based），用于恢复诊断记录。
    round: u32,
    /// 本输入内 scavenge 批次序号：回收调用的 id 必须跨轮唯一，
    /// 否则 conversation.jsonl 中会出现重复 tool_use_id 干扰配对修复。
    scavenge_seq: u32,
}

/// TurnExecutor runs a single "turn" of the agent loop:
///   Stream (LLM response) → Persist → Tools → Decide (continue/stop)
pub struct TurnExecutor {
    ctx: Arc<AgentSharedContext>,
    model_name: String,
    model_alias: Option<String>,
    tools: Arc<ToolRunner>,
    prefix: crate::agent::prefix::PrefixManager,
    compactor: crate::agent::compactor::TurnCompactor,
    signal_processor: crate::agent::tool_signals::ToolSignalProcessor,
    sub_agents: crate::agent::sub_coordinator::SubAgentCoordinator,
    local: TurnLocalState,
    /// 决策引擎（含冷却逻辑，由引擎内部管理）。
    decision_engine: crate::agent::decision::DecisionEngine,
    recovery_policy: crate::agent::recovery_policy::RecoveryPolicy,
    /// 恢复子代理配置副本，包含当前活动模型。
    sub_agent_config: crate::config::ResolvedConfig,
}

/// Turn-level side effects are already-formatting info messages.
pub type TurnEffect = &'static str;

#[derive(Debug, PartialEq)]
pub enum TurnDecision {
    Stop,
    Interrupted,
    MaxTurnsExceeded,
    Failed(String),
}

impl TurnExecutor {
    /// Build an executor for the session's configured model.
    pub fn new(ctx: Arc<AgentSharedContext>) -> Self {
        let resolved = crate::config::model_resolver(&ctx.config).resolve(&ctx.config.model);
        Self::new_for_model(ctx, resolved)
    }

    /// Build an executor for one resolved model target; this is the single
    /// construction path (no post-construction model override).
    pub(crate) fn new_for_model(
        ctx: Arc<AgentSharedContext>,
        resolved: crate::config::ResolvedModel,
    ) -> Self {
        let tools = Arc::new(ToolRunner::new(Arc::new(
            crate::context::ToolContext::from(ctx.as_ref()),
        )));
        let prefix = crate::agent::prefix::PrefixManager::new(ctx.clone());
        let mut sub_agent_config = ctx.config.clone();
        if let Some(alias) = resolved.alias.as_deref() {
            sub_agent_config
                .model_aliases
                .insert(alias.to_string(), resolved.actual.clone());
            sub_agent_config.model = alias.to_string();
        } else {
            sub_agent_config.model = resolved.actual.clone();
        }
        Self {
            ctx: ctx.clone(),
            model_name: resolved.actual,
            model_alias: resolved.alias,
            tools,
            prefix: prefix.clone(),
            compactor: crate::agent::compactor::TurnCompactor::new(ctx.clone(), prefix.clone()),
            signal_processor: crate::agent::tool_signals::ToolSignalProcessor::from_config(
                &ctx.config.signal,
            ),
            sub_agents: crate::agent::sub_coordinator::SubAgentCoordinator::new(ctx.clone()),
            sub_agent_config,
            local: TurnLocalState::default(),
            decision_engine: crate::agent::decision::DecisionEngine::from_config(
                &ctx.config.signal,
            ),
            recovery_policy: crate::agent::recovery_policy::RecoveryPolicy::from_resolved(
                &ctx.tool_capabilities,
                crate::tools::catalog::ToolCatalog::builtin()
                    .expect("built-in tool catalog was validated during context construction"),
            ),
        }
    }

    fn model_label(&self) -> &str {
        self.model_alias.as_deref().unwrap_or(&self.model_name)
    }

    /// Return the total number of tool calls made during this turn.
    pub fn tool_call_count(&self) -> u32 {
        self.local.tool_call_count
    }

    /// Number of tool calls that produced at least one tool_error signal.
    pub fn tool_error_count(&self) -> u32 {
        self.signal_processor.tool_error_count()
    }

    /// Model-output format failures observed in this turn (bad tool argument
    /// decoding); counted separately from execution failures.
    pub fn format_error_count(&self) -> u32 {
        self.signal_processor.format_error_count()
    }

    pub fn text(&self) -> &str {
        &self.local.final_text
    }

    pub fn thinking(&self) -> &str {
        &self.local.final_thinking
    }

    fn reset_local_state(&mut self, user_input: &str) {
        self.tools.reset_storm();
        self.compactor.reset();
        self.signal_processor.reset();
        self.decision_engine.reset();
        self.ctx
            .this_turn_image_ids
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.local = TurnLocalState {
            current_user_input: user_input.to_string(),
            ..TurnLocalState::default()
        };
    }
}

// `prepare_request` 的强制压缩循环不设次数上限：每次迭代必须严格降低投影估算
// （否则 break），有限整数度量自然终止；不得用隐藏常量给用户输入封顶。

/// provider overflow 后的请求内收缩上限：每次都必须让请求严格变小（本地估算说
/// 装得下、provider 却拒绝时，只靠本地闸门无法恢复）。
const MAX_OVERFLOW_SHRINK_ATTEMPTS: usize = 3;

impl TurnExecutor {
    /// Execute a full turn: send user input, stream response, execute tools, decide next.
    /// Phase 0 for one inner iteration: auto compaction, forced compaction and
    /// the hard input-budget check.
    ///
    /// Compaction is repeatable inside one user input: a projection that does
    /// not fit the request budget is compacted again until it fits, no further
    /// legal cut exists, or a compaction stops reducing it. The final check
    /// stays fail-closed — an over-budget request is never sent.
    async fn prepare_request(
        &mut self,
        messages: &mut Vec<serde_json::Value>,
        system_prompt: &mut String,
        tools_json: &mut Vec<serde_json::Value>,
    ) -> Result<(Vec<serde_json::Value>, usize)> {
        // Phase 0: 上下文压缩（auto 按压力阈值触发，可多次）。可选提前压缩失败不得
        // 阻断本来可以发送的请求：只有摘要不可用类错误被放行并继续。
        match self
            .try_compact("auto", messages, system_prompt, tools_json)
            .await
        {
            Ok(_) => {}
            Err(error) if crate::session::compaction::is_compaction_interrupted(&error) => {
                return Err(anyhow::Error::new(TurnInterrupted));
            }
            Err(error) if crate::session::compaction::is_summary_unavailable(&error) => {
                self.ctx
                    .display
                    .render_info("Context summary unavailable; continuing without compaction.");
            }
            Err(error) => return Err(error),
        }
        let input_limit = crate::session::compaction::request_input_limit(&self.ctx.config);
        let mut request_messages = self.project_request_messages(messages)?;
        let mut estimated_tokens = crate::llm::transport::estimate_openai_context_tokens(
            &request_messages,
            tools_json,
            system_prompt,
        )?;
        // 硬闸门：反复强制压缩，直到装得下、压无可压或压缩不再降低估算。
        let mut forced_attempts = 0usize;
        let mut last_detail: Option<String> = None;
        let mut summary_failure: Option<String> = None;
        while estimated_tokens > input_limit {
            let before = estimated_tokens;
            let compacted = match self
                .try_compact("preflight", messages, system_prompt, tools_json)
                .await
            {
                Ok((compacted, detail)) => {
                    last_detail = Some(detail);
                    compacted
                }
                Err(error) if crate::session::compaction::is_compaction_interrupted(&error) => {
                    return Err(anyhow::Error::new(TurnInterrupted));
                }
                Err(error) if crate::session::compaction::is_summary_unavailable(&error) => {
                    summary_failure = Some(format!("{error}"));
                    break;
                }
                Err(error) => return Err(error),
            };
            forced_attempts += 1;
            if !compacted {
                break;
            }
            request_messages = self.project_request_messages(messages)?;
            estimated_tokens = crate::llm::transport::estimate_openai_context_tokens(
                &request_messages,
                tools_json,
                system_prompt,
            )?;
            if estimated_tokens >= before {
                // 压缩没有带来净收益：同一投影上继续摘要不会改变结果。
                break;
            }
        }
        // 正常压缩（含 LLM 摘要）无法让请求装下时，退化为确定性应急 checkpoint：
        // 不调用 LLM，把可变历史折成有界摘录，保证上下文故障之后仍有继续路径。
        let mut emergency = false;
        if estimated_tokens > input_limit {
            let reason = summary_failure
                .clone()
                .or_else(|| last_detail.clone())
                .unwrap_or_else(|| "over_budget_without_cut".to_string());
            emergency = match self
                .commit_emergency_checkpoint(&reason, messages, system_prompt, tools_json, None)
                .await
            {
                Ok(committed) => committed,
                Err(error) if crate::session::compaction::is_compaction_interrupted(&error) => {
                    // 应急路径的取消/中断必须映射为 Interrupted，而不是普通失败。
                    return Err(anyhow::Error::new(TurnInterrupted));
                }
                Err(error) => return Err(error),
            };
            if emergency {
                request_messages = self.project_request_messages(messages)?;
                estimated_tokens = crate::llm::transport::estimate_openai_context_tokens(
                    &request_messages,
                    tools_json,
                    system_prompt,
                )?;
            }
        }
        if estimated_tokens > input_limit {
            anyhow::bail!(
                "context remains over the request input budget: estimated {estimated_tokens} tokens, \
                 limit {input_limit}; {} compaction(s) committed and {forced_attempts} forced attempt(s) \
                 in this input; emergency checkpoint {} (last: {}){}",
                self.compactor.compactions_this_turn(),
                if emergency {
                    "committed but still over budget"
                } else {
                    "unavailable"
                },
                summary_failure
                    .or(last_detail)
                    .unwrap_or_else(|| "none".to_string()),
                if emergency {
                    " (minimal working space unavailable)"
                } else {
                    ""
                }
            );
        }
        Ok((request_messages, estimated_tokens))
    }

    pub async fn execute(
        &mut self,
        user_input: &str,
        mut belief: Option<&mut crate::agent::belief::BeliefTracker>,
    ) -> Result<(TurnDecision, Vec<TurnEffect>)> {
        // A latched persistence fault means the session must not accept new
        // work: refuse before the model request, history append or tools.
        self.ctx.persistence_fault.check()?;
        // New user intent: compiler-enforced full local-state reset.
        self.reset_local_state(user_input);

        // A previous turn may have failed after changing plan.md but before
        // durably appending its tool result/transition. Replay or roll back
        // that journal before constructing any new model-visible history.
        self.tools.recover_plan_transactions().await?;

        let mut messages = self.ctx.compaction.active_messages().await?;
        self.reconcile_todo_state(&mut messages).await?;
        self.ctx.store.add_user(user_input).await?;
        self.ctx.stats.record_turn().await;
        self.ctx.log_event(crate::events::EventLog::UserInput {
            version: None,
            content: user_input.to_string(),
        });

        let mut turn = 0;
        let mut effects = Vec::new();
        let max_turns = self.ctx.max_turns() as usize;

        let (mut system_prompt, mut tools_json) = match self.ensure_prefix().await {
            Ok(prefix) => prefix,
            Err(error) if error.downcast_ref::<TurnInterrupted>().is_some() => {
                self.ctx.display.render_stop("interrupted");
                return Ok((TurnDecision::Interrupted, effects));
            }
            Err(error) => return Err(error),
        };
        messages = self.ctx.compaction.active_messages().await?;

        // Every user turn owns a fresh format window; compaction never clears
        // it and a sub-agent executor owns its own. State stays in memory.
        let recovery_policy = self.ctx.config.llm_recovery;
        let mut format_window = format_recovery::FormatRecoveryWindow::new(
            recovery_policy.format_window_size,
            recovery_policy.format_max_errors,
        );

        while turn < max_turns {
            turn += 1;
            self.local.round = turn as u32;

            let (mut request_messages, current_context_tokens) = match self
                .prepare_request(&mut messages, &mut system_prompt, &mut tools_json)
                .await
            {
                Ok(prepared) => prepared,
                Err(error) if error.downcast_ref::<TurnInterrupted>().is_some() => {
                    // Cancellation during request preparation (including a
                    // prefix snapshot commit waiting on the event log) keeps
                    // the "interrupted" classification.
                    self.ctx.display.render_stop("interrupted");
                    return Ok((TurnDecision::Interrupted, effects));
                }
                Err(error) => return Err(error),
            };

            // provider overflow 的请求内收缩额度只作用于**当前逻辑请求**（本 round）：
            // 长任务里每个 round 都应有自己的恢复机会，不能共享一个终身额度。
            let mut overflow_shrink_attempts = 0usize;
            // Phase 1: LLM 流式响应（同 round 内可含多次 attempt 重试）
            let stream_output = loop {
                match self
                    .stream_llm_response(
                        &format_window,
                        &request_messages,
                        &system_prompt,
                        &tools_json,
                        current_context_tokens,
                    )
                    .await
                {
                    Ok(output) => break output,
                    Err(error) if error.downcast_ref::<TurnInterrupted>().is_some() => {
                        // 建立阶段的中断必须先于 context-overflow 等
                        // 字符串式识别处理：用户中断不能伪装成网络失败。
                        self.ctx.display.render_stop("interrupted");
                        return Ok((TurnDecision::Interrupted, effects));
                    }
                    Err(error)
                        if error.downcast_ref::<ContextOverflowError>().is_some()
                            && overflow_shrink_attempts < MAX_OVERFLOW_SHRINK_ATTEMPTS =>
                    {
                        overflow_shrink_attempts += 1;
                        // 被拒请求的完整本地估算与固定前缀开销：可变额度折半作为
                        // 本次更小的目标（固定前缀不参与折半）。
                        let rejected_tokens =
                            crate::llm::transport::estimate_openai_context_tokens(
                                &request_messages,
                                &tools_json,
                                &system_prompt,
                            )?;
                        let prefix_tokens = crate::llm::transport::estimate_openai_context_tokens(
                            &[],
                            &tools_json,
                            &system_prompt,
                        )?;
                        let shrink_target = prefix_tokens
                            .saturating_add(rejected_tokens.saturating_sub(prefix_tokens) / 2);
                        let compacted = match self
                            .try_compact(
                                "overflow",
                                &mut messages,
                                &mut system_prompt,
                                &mut tools_json,
                            )
                            .await
                        {
                            Ok((compacted, _)) => compacted,
                            Err(error)
                                if crate::session::compaction::is_compaction_interrupted(
                                    &error,
                                ) =>
                            {
                                self.ctx.display.render_stop("interrupted");
                                return Ok((TurnDecision::Interrupted, effects));
                            }
                            Err(error)
                                if crate::session::compaction::is_summary_unavailable(&error) =>
                            {
                                self.ctx.display.render_info(
                                    "Context summary unavailable; falling back to an emergency checkpoint.",
                                );
                                false
                            }
                            Err(error) => return Err(error),
                        };
                        if compacted {
                            self.ctx.display.render_info(
                                "Context summary committed; verifying the request actually shrank.",
                            );
                        }
                        // 「完成一次摘要」不等于恢复成功：必须看重新投影后的真实大小。
                        request_messages = self.project_request_messages(&messages)?;
                        let mut retried_tokens =
                            crate::llm::transport::estimate_openai_context_tokens(
                                &request_messages,
                                &tools_json,
                                &system_prompt,
                            )?;
                        if retried_tokens >= rejected_tokens {
                            // 摘要不可用、没有收益或反而变大时，应急 checkpoint 是唯一
                            // 不依赖 LLM 的缩小手段（按折半目标构造）。
                            let committed = match self
                                .commit_emergency_checkpoint(
                                    "provider_overflow",
                                    &mut messages,
                                    &system_prompt,
                                    &tools_json,
                                    Some(shrink_target),
                                )
                                .await
                            {
                                Ok(committed) => committed,
                                Err(error)
                                    if crate::session::compaction::is_compaction_interrupted(
                                        &error,
                                    ) =>
                                {
                                    self.ctx.display.render_stop("interrupted");
                                    return Ok((TurnDecision::Interrupted, effects));
                                }
                                Err(error) => return Err(error),
                            };
                            if !committed {
                                return Err(error);
                            }
                            request_messages = self.project_request_messages(&messages)?;
                            retried_tokens = crate::llm::transport::estimate_openai_context_tokens(
                                &request_messages,
                                &tools_json,
                                &system_prompt,
                            )?;
                        }
                        if retried_tokens >= rejected_tokens {
                            // 没有变得更小：再发同样的请求只会再次被拒。
                            return Err(error);
                        }
                        self.ctx.display.render_info(&format!(
                            "Context overflow detected; shrank the request ({} -> {} tokens) and retrying.",
                            rejected_tokens, retried_tokens
                        ));
                    }
                    Err(error) => return Err(error),
                }
            };
            let StreamOutput {
                text,
                thinking,
                mut calls,
                mut stop,
                usage,
                format_abort,
                had_format_error: stream_format_error,
            } = stream_output;
            let mut had_format_error = stream_format_error;

            if let Some(detail) = format_abort {
                // Response damage exhausted the format window: the single
                // round-end commit happens here and the turn fails with the
                // stable reason prefix.
                let _ = format_window.commit(true);
                self.ctx.display.render_error(&detail);
                return Ok((TurnDecision::Failed(detail), effects));
            }

            if self.ctx.cancel.is_cancelled() || self.ctx.interrupt.load(Ordering::SeqCst) {
                self.ctx.display.render_stop("interrupted");
                return Ok((TurnDecision::Interrupted, effects));
            }

            // Phase 1b: 从 thinking/text 回收漏报的工具调用
            let recovered_calls;
            (calls, recovered_calls) = self.scavenge_calls(&thinking, &text, calls);
            if recovered_calls
                && !calls.is_empty()
                && matches!(stop.as_str(), "end_turn" | "stop" | "done")
            {
                stop = "tool_use".into();
            }

            // 结束判定顺序（不能只看 finish_reason）：拒绝 → 截断 →
            // 调用身份 → 可配对调用 → 无调用完成/不可确认。
            let kind = if stop == "interrupted" {
                RoundKind::Interrupted
            } else if is_terminal_stop(&stop) {
                RoundKind::Terminal
            } else if is_truncated_stop(&stop) {
                RoundKind::Truncated
            } else if let Some(reason) = validate_tool_call_identity(&calls) {
                RoundKind::IdentityInvalid(reason)
            } else if !calls.is_empty() {
                RoundKind::ConsumeCalls
            } else if matches!(stop.as_str(), "end_turn" | "stop" | "done") && !text.is_empty() {
                RoundKind::Complete
            } else {
                RoundKind::Unconfirmed
            };

            // Round body: persist/execute and compute this round's window
            // outcome. No branch continues on its own — the single round tail
            // below settles the window, runs the branch decisions and only then
            // refreshes the history.
            let window_error = match &kind {
                RoundKind::Interrupted => {
                    self.ctx.display.render_stop("interrupted");
                    return Ok((TurnDecision::Interrupted, effects));
                }
                RoundKind::Terminal => {
                    // Terminal stop: explicit provider error/refusal — the candidate
                    // calls never execute and the content is never re-prompted
                    // as a "format error".
                    self.persist_assistant(&text, &thinking, &[], &usage)
                        .await?;
                    self.record_agent_calibration(&usage);
                    self.local.final_text.push_str(&text);
                    self.local.final_thinking.push_str(&thinking);
                    self.ctx.display.render_stop(&stop);
                    return Ok((TurnDecision::Failed(format!("stop: {stop}")), effects));
                }
                RoundKind::Truncated => {
                    // Truncated candidate: discarded as a whole
                    // (text and every call, however complete it looks). Only
                    // the provider bill remains.
                    self.record_discarded_usage(&usage).await;
                    let diagnostic = format!(
                        "<output-truncated>The previous response hit the output limit (stop: {}) and was discarded without executing anything. Continue with one smaller, focused step.</output-truncated>",
                        truncate_str(&stop, 80)
                    );
                    self.ctx.store.add_runtime_user(&diagnostic).await?;
                    true
                }
                RoundKind::IdentityInvalid(reason) => {
                    // Identity-invalid batch: cannot be paired
                    // reliably. Drop every call; never write an illegal tool
                    // call or invent results for it.
                    self.persist_assistant(&text, &thinking, &[], &usage)
                        .await?;
                    self.record_agent_calibration(&usage);
                    self.local.final_text.push_str(&text);
                    self.local.final_thinking.push_str(&thinking);
                    let diagnostic = format!(
                        "<tool-call-format-error>The previous response could not be executed: {reason}. No tool was run. Resend the intended tool call(s) with an explicit tool name and one unique id per call.</tool-call-format-error>"
                    );
                    self.ctx.store.add_runtime_user(&diagnostic).await?;
                    true
                }
                RoundKind::Unconfirmed => {
                    // Unconfirmed response: empty/thinking-only body, tool_calls without
                    // calls, or empty/unknown reason — never a silent empty
                    // success, never a text-bearing "success" either.
                    self.persist_assistant(&text, &thinking, &[], &usage)
                        .await?;
                    self.record_agent_calibration(&usage);
                    self.local.final_text.push_str(&text);
                    self.local.final_thinking.push_str(&thinking);
                    let diagnostic = format!(
                        "<incomplete-response>The previous assistant response could not be confirmed as complete (stop reason: {:?}, no tool calls). State the next step explicitly or resend the intended tool call(s).</incomplete-response>",
                        truncate_str(&stop, 80)
                    );
                    self.ctx.store.add_runtime_user(&diagnostic).await?;
                    true
                }
                RoundKind::Complete => {
                    self.persist_assistant(&text, &thinking, &[], &usage)
                        .await?;
                    self.record_agent_calibration(&usage);
                    self.local.final_text.push_str(&text);
                    self.local.final_thinking.push_str(&thinking);
                    had_format_error
                }
                RoundKind::ConsumeCalls => {
                    self.persist_assistant(&text, &thinking, &calls, &usage)
                        .await?;
                    self.record_agent_calibration(&usage);
                    self.local.final_text.push_str(&text);
                    self.local.final_thinking.push_str(&thinking);
                    // Formal ToolCall display/events are deferred to the
                    // accepted response: discarded candidates never
                    // surface as executed calls.
                    self.announce_tool_calls(&calls);
                    // Real tool results are persisted before the single
                    // round-end window commit: an exhausted format budget
                    // must not roll back legal side effects.
                    had_format_error |= self
                        .execute_tools_inner(calls, belief.as_deref_mut(), &mut effects)
                        .await?;
                    if self.ctx.cancel.is_cancelled() || self.ctx.interrupt.load(Ordering::SeqCst) {
                        self.ctx.display.render_stop("interrupted");
                        return Ok((TurnDecision::Interrupted, effects));
                    }
                    had_format_error
                }
            };

            // Single round tail: one window slot, then the branch
            // decisions (which may append a todo reminder or `[trajectory]`
            // evidence), then a history refresh AFTER every append so the next
            // request sees diagnostics, tool results and injected state alike.
            if format_window.commit(window_error) {
                return Ok((
                    TurnDecision::Failed(self.format_exhausted_reason()),
                    effects,
                ));
            }
            match &kind {
                RoundKind::ConsumeCalls => {
                    let signal_enabled = self.ctx.config.signal_policy.enabled();
                    if let Some(decision) = self
                        .decide_signal_recovery(signal_enabled, belief.as_deref_mut())
                        .await?
                    {
                        return Ok((decision, effects));
                    }
                }
                RoundKind::Complete => {
                    if let Some(decision) = self
                        .decide_next(&stop, belief.as_deref_mut(), turn >= max_turns)
                        .await?
                    {
                        return Ok((decision, effects));
                    }
                }
                _ => {}
            }
            messages = self.ctx.compaction.active_messages().await?;
        }

        Ok((TurnDecision::MaxTurnsExceeded, effects))
    }

    /// Stable terminal reason for an exhausted format window.
    fn format_exhausted_reason(&self) -> String {
        let policy = self.ctx.config.llm_recovery;
        format!(
            "format_recovery_exhausted: more than {} format error(s) within the last {} round(s)",
            policy.format_max_errors, policy.format_window_size
        )
    }

    /// Update the provider prompt-usage calibration baseline for a finally
    /// accepted response. Session stats are recorded by `persist_assistant`;
    /// calibration must not be fed by discarded candidates.
    fn record_agent_calibration(&self, usage: &Option<UsageEvent>) {
        if let Some(usage) = usage {
            self.ctx.compaction.record_agent_usage(usage);
        }
    }

    /// Bill a discarded candidate: the provider bill still stands, but it must
    /// not calibrate later requests or dilute the format window.
    async fn record_discarded_usage(&self, usage: &Option<UsageEvent>) {
        if let Some(usage) = usage {
            self.ctx.stats.record_usage(usage).await;
        }
    }
}

/// End-decision shape for one accepted candidate.
enum RoundKind {
    Interrupted,
    Terminal,
    Truncated,
    IdentityInvalid(String),
    ConsumeCalls,
    Complete,
    Unconfirmed,
}

/// Explicit provider refusal/error stop reasons: report plainly, never induce a
/// resend as a format error.
fn is_terminal_stop(stop: &str) -> bool {
    matches!(stop, "content_filter" | "error")
}

fn is_truncated_stop(stop: &str) -> bool {
    matches!(stop, "length" | "max_tokens")
}

/// The whole candidate tool batch is unusable when calls cannot be paired
/// reliably: missing name, missing id, or duplicate ids.
fn validate_tool_call_identity(calls: &[ToolCallEvent]) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    for call in calls {
        if call.name.is_empty() {
            let id = if call.id.is_empty() {
                "(no id)"
            } else {
                &call.id
            };
            let detail = match &call.parse_error {
                Some(error) => format!(" (the payload is unparsable: {error})"),
                None => String::new(),
            };
            return Some(format!(
                "a candidate tool call ({id}) is missing a tool name{detail}"
            ));
        }
        if call.id.is_empty() {
            return Some(format!("tool call '{}' is missing a call id", call.name));
        }
        if !seen.insert(call.id.as_str()) {
            return Some(format!("tool call id '{}' appears more than once", call.id));
        }
    }
    None
}

fn is_context_overflow_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "context_length_exceeded",
        "maximum context length",
        "maximum context size",
        "context window exceeded",
        "exceeds the context window",
        "context length exceeded",
        "too many tokens",
        "prompt is too long",
        "input is too long",
        "max sequence length",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

pub(crate) fn positive_duration(seconds: i32) -> Option<Duration> {
    (seconds > 0).then(|| Duration::from_secs(seconds as u64))
}

fn blocked_by_signal_recovery(
    call: ToolCallEvent,
    content: String,
) -> crate::tools::runner::ToolExecution {
    crate::tools::runner::ToolExecution {
        tool_use_id: call.id,
        tool_name: call.name,
        tool_args: call.fields,
        content,
        conv_content: String::new(),
        spawns_sub_agent: false,
        sub_agent_prompt: None,
        sub_agent_fork: false,
        exit_code: None,
        status: crate::tools::metadata::ToolStatus::Blocked(
            crate::tools::metadata::ToolBlocker::RecoveryGuard,
        ),
        result_kind: crate::tools::metadata::ToolResultKind::Control,
        presentation: None,
        artifacts: Vec::new(),
        signals: Vec::new(),
        failure_source: None,
        plan_command: None,
        needs_finalization: false,
        state_metadata: None,
        image_attachment: None,
    }
}

#[cfg(test)]
#[path = "turn_tests.rs"]
mod tests;
