use crate::agent::belief::BeliefTracker;
use crate::agent::turn::{TurnDecision, TurnEffect, TurnExecutor};
use crate::context::AgentSharedContext;
use crate::errors;
use crate::session::usage::{UsageRecord, UsageSummary};
use anyhow::Result;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

/// OrchActor is the central orchestrator: receives user inputs,
/// dispatches them to TurnExecutor, and manages the lifecycle.
pub struct OrchActor {
    ctx: Arc<AgentSharedContext>,
    cmd_rx: mpsc::UnboundedReceiver<OrchCmd>,
    belief: BeliefTracker,
    forced_model: Option<String>,
}

/// Commands received by the orchestrator.
pub enum OrchCmd {
    UserInput {
        input: String,
        turn_id: crate::runtime::TurnId,
        emitter: std::sync::Arc<crate::runtime::TurnEventEmitter>,
        done: oneshot::Sender<TurnRunResult>,
    },
    SetModel {
        model: String,
        done: oneshot::Sender<anyhow::Result<crate::runtime::ModelSwitchOutcome>>,
    },
    Compact {
        done: oneshot::Sender<anyhow::Result<crate::runtime::CompactOutcome>>,
    },
}

#[derive(Debug, Clone)]
pub struct TurnRunResult {
    pub billing_turn_id: String,
    pub status: TurnStatus,
    pub tool_call_count: u32,
    pub tool_error_count: u32,
    pub error: Option<String>,
    pub usage_records: Vec<UsageRecord>,
    pub usage: UsageSummary,
    pub text: String,
    pub thinking: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Ok,
    Failed,
    Interrupted,
    MaxTurnsExceeded,
}

impl TurnStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnStatus::Ok => "ok",
            TurnStatus::Failed => "failed",
            TurnStatus::Interrupted => "interrupted",
            TurnStatus::MaxTurnsExceeded => "max_turns_exceeded",
        }
    }
}

impl TurnRunResult {
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            billing_turn_id: String::new(),
            status: TurnStatus::Failed,
            tool_call_count: 0,
            tool_error_count: 0,
            error: Some(error.into()),
            usage_records: Vec::new(),
            usage: UsageSummary::default(),
            text: String::new(),
            thinking: String::new(),
        }
    }

    fn from_decision(decision: &TurnDecision, executor: &TurnExecutor) -> Self {
        let status = match decision {
            TurnDecision::Stop => TurnStatus::Ok,
            TurnDecision::Interrupted => TurnStatus::Interrupted,
            TurnDecision::MaxTurnsExceeded => TurnStatus::MaxTurnsExceeded,
            TurnDecision::Failed(_) => TurnStatus::Failed,
        };
        let error = match decision {
            TurnDecision::Failed(msg) => Some(msg.clone()),
            TurnDecision::Interrupted => Some("interrupted".to_string()),
            TurnDecision::MaxTurnsExceeded => {
                Some("max_turns exhausted before end_turn".to_string())
            }
            _ => None,
        };
        Self {
            billing_turn_id: String::new(),
            status,
            tool_call_count: executor.tool_call_count(),
            tool_error_count: executor.tool_error_count(),
            error,
            usage_records: Vec::new(),
            usage: UsageSummary::default(),
            text: executor.text().to_string(),
            thinking: executor.thinking().to_string(),
        }
    }
}

impl OrchActor {
    /// 当前活跃投影（含即将出现的 TodoSync）是否超过主请求输入预算。
    async fn projection_over_budget(
        ctx: &AgentSharedContext,
        shape: crate::session::compaction::MainRequestShape<'_>,
    ) -> Result<bool> {
        let limit = crate::session::compaction::request_input_limit(&ctx.config);
        if limit == usize::MAX {
            return Ok(false);
        }
        let projected = ctx.compaction.active_messages().await?;
        let snapshot = ctx.todo_store.snapshot();
        let todo_candidate = crate::agent::compactor::todo_candidate_state(ctx, &snapshot);
        let tokens = crate::session::compaction::estimate_candidate_tokens(
            shape,
            &projected,
            todo_candidate.as_ref(),
        )?;
        Ok(tokens > limit)
    }
}

impl OrchActor {
    pub fn new(ctx: Arc<AgentSharedContext>, cmd_rx: mpsc::UnboundedReceiver<OrchCmd>) -> Self {
        let (window, alpha, beta) = (
            ctx.config.signal.window_size,
            ctx.config.signal.alpha_prior,
            ctx.config.signal.beta_prior,
        );
        Self {
            ctx,
            cmd_rx,
            belief: BeliefTracker::new_with_priors(window, alpha, beta),
            forced_model: None,
        }
    }

    /// Run the orchestrator loop until shutdown.
    pub async fn run(mut self) -> Result<()> {
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(OrchCmd::UserInput {
                        input,
                        turn_id,
                        emitter,
                        done,
                    }) => {
                        emitter.emit(crate::runtime::AgentEventKind::TurnStarted);
                        self.ctx.log_event(crate::events::EventLog::RuntimeTurnStarted {
                            turn_id: turn_id.to_string(),
                        });
                        let result = self.handle_user_input(input).await;
                        let _ = done.send(result);
                    }
                    Some(OrchCmd::SetModel { model, done }) => {
                        let result = self.handle_model_command(&model).await;
                        let _ = done.send(result);
                    }
                    Some(OrchCmd::Compact { done }) => {
                        let outcome = match self.handle_compact().await {
                            Ok(outcome) => Ok(outcome),
                            Err(error) => {
                                // 失败只交付一次原始诊断：可恢复错误与取消不再结束 actor；
                                // 闩锁会话由 handle_compact 入口检查拒绝后续命令。
                                self.ctx.log_event(crate::events::EventLog::Compact {
                                    version: Some(2),
                                    trigger: "manual".into(),
                                    result: format!("failed: {error:#}"),
                                });
                                self.ctx.display.render_error(&format!("Compact failed: {error}"));
                                Err(error)
                            }
                        };
                        self.ctx.display.render_stop("end_turn");
                        if let Err(error) = self.ctx.flush_event_log().await {
                            self.ctx
                                .display
                                .render_error(&format!("Event log flush failed: {error}"));
                        }
                        let _ = done.send(outcome);
                    }
                    None => break,
                },
                _ = self.ctx.cancel.cancelled() => {
                    self.ctx.display.render_info("Shutting down...");
                    break;
                }
            }
        }

        if let Err(error) = self.ctx.flush_event_log().await {
            self.ctx
                .display
                .render_error(&format!("Event log flush failed: {error}"));
        }
        let _ = self.ctx.stats.flush().await;
        Ok(())
    }

    /// Manual compaction as a self-contained operation: the actor loop only
    /// receives the command, delivers the result exactly once and keeps
    /// running. Recoverable failures and cancellations reach the caller with
    /// their original diagnostic; a latched session refuses new work here
    /// (before any prefix/history/LLM access) instead of dying with a
    /// channel-closed error.
    async fn handle_compact(&mut self) -> Result<crate::runtime::CompactOutcome> {
        self.ctx.persistence_fault.check()?;
        self.ctx.display.render_info("Compressing...");
        let active_model = self.resolve_active();

        // manual 与每次真实请求同源：无条件经唯一的 PrefixManager 获取前缀形状
        // （缓存命中校验、依赖指纹与宿主 PrefixSource 都在其中）。前缀失败原样
        // 返回调用方，不得降级为“无预算验收”继续压缩。
        let (system_prompt, tools) = crate::agent::prefix::PrefixManager::new(self.ctx.clone())
            .ensure()
            .await
            .map_err(|error| {
                // 前缀构建阶段的取消/中断与压缩中断同义（沿用既有错误文案）。
                if error
                    .downcast_ref::<crate::agent::turn::TurnInterrupted>()
                    .is_some()
                {
                    crate::session::compaction::CompactionInterrupted::error()
                } else {
                    error
                }
            })?;
        let shape = crate::session::compaction::MainRequestShape {
            system_prompt: &system_prompt,
            tools: &tools,
        };
        let target = crate::llm::client::LlmModelTarget::new(
            &active_model.actual,
            active_model.alias.as_deref(),
        );
        let todo_snapshot = self.ctx.todo_store.snapshot();
        let todo_candidate =
            crate::agent::compactor::todo_candidate_state(&self.ctx, &todo_snapshot);
        let mut result = self
            .ctx
            .compaction
            .evaluate_and_compact_with_prefix(
                "manual",
                0,
                target,
                None,
                None,
                // manual 不在某个用户输入内：不携带本轮序号，也不带
                // 当前请求原文（不得伪造）。
                None,
                Some(shape),
                todo_candidate,
            )
            .await;
        // 手动压缩的失败分流与 turn 一致：摘要侧不可用时，投影本来就
        // 超预算就转确定性应急 checkpoint（明确原因），投影仍能发送就返回
        // 清晰的 Skipped，不伪装摘要成功；取消/持久化错误原样传播。
        let mut emergency_committed = false;
        let summary_error = match &result {
            Err(error) if crate::session::compaction::is_summary_unavailable(error) => {
                Some(format!("{error:#}"))
            }
            _ => None,
        };
        {
            let needs_emergency = match &result {
                Ok((compacted, _)) => {
                    !*compacted && Self::projection_over_budget(&self.ctx, shape).await?
                }
                Err(error) if crate::session::compaction::is_summary_unavailable(error) => {
                    Self::projection_over_budget(&self.ctx, shape).await?
                }
                Err(_) => false,
            };
            if needs_emergency {
                let reason = if summary_error.is_some() {
                    "manual_summary_unavailable"
                } else {
                    "manual_projection_over_budget"
                };
                emergency_committed = crate::agent::compactor::commit_emergency_checkpoint(
                    &self.ctx,
                    reason,
                    shape,
                    None,
                    None,
                    &self.ctx.compaction.active_messages().await?,
                )
                .await?;
                if emergency_committed {
                    self.ctx
                        .display
                        .render_info("Manual compaction fell back to an emergency checkpoint.");
                    result = Ok((true, "manual emergency checkpoint committed".to_string()));
                }
            }
        }
        if !emergency_committed && let Some(error) = summary_error {
            result = Ok((false, format!("summary unavailable: {error}")));
        }

        match result {
            Ok((true, _reason)) => {
                if let Some(summary) = self.ctx.compaction.read_summary().await {
                    let trimmed = summary.trim();
                    if !trimmed.is_empty() {
                        self.ctx.display.render_text(trimmed);
                        if !trimmed.ends_with('\n') {
                            self.ctx.display.render_text("\n");
                        }
                    }
                }
                self.refresh_title().await;
                Ok(crate::runtime::CompactOutcome::Compacted { reason: _reason })
            }
            Ok((false, reason)) => {
                self.ctx
                    .display
                    .render_info(&format!("Compact skipped: {reason}"));
                // 跳过也要留痕：摘要不可用等失败不能只在终端出现。
                self.ctx.log_event(crate::events::EventLog::Compact {
                    version: Some(2),
                    trigger: "manual".into(),
                    result: format!("skipped: {reason}"),
                });
                Ok(crate::runtime::CompactOutcome::Skipped { reason })
            }
            Err(error) => Err(error),
        }
    }
    async fn handle_user_input(&mut self, input: String) -> TurnRunResult {
        let started_at = Instant::now();
        let billing_turn_id = self.ctx.usage.begin_turn();
        // Session-level publish fault: refuse the operation up front instead
        // of requesting the model, touching history or executing tools.
        if let Err(error) = self.ctx.persistence_fault.check() {
            self.ctx.display.render_error(&format!("{error:#}"));
            self.refresh_title().await;
            let result = self.finish_usage(
                TurnRunResult::failed(format!("{error:#}")),
                &billing_turn_id,
            );
            self.log_turn_final(&result, started_at.elapsed().as_millis() as u64)
                .await;
            return result;
        }
        // 跨轮重复失败可累积升级，单次偶然失败自然消退。
        self.belief.decay(self.ctx.config.signal.decay_per_input);
        self.refresh_title().await;
        let (model, mut executor) = self.prepare_turn();

        let result = match executor.execute(&input, Some(&mut self.belief)).await {
            Ok((decision, effects)) => {
                self.post_process_turn(decision, effects, &executor, &model)
                    .await
            }
            Err(e) => self.handle_turn_error(e, &executor, &model).await,
        };

        let result = self.finish_usage(result, &billing_turn_id);
        self.refresh_title().await;
        self.log_turn_final(&result, started_at.elapsed().as_millis() as u64)
            .await;
        result
    }

    fn finish_usage(&self, mut result: TurnRunResult, billing_turn_id: &str) -> TurnRunResult {
        result.billing_turn_id = billing_turn_id.to_string();
        match self.ctx.usage.records_for(billing_turn_id) {
            Ok(records) => {
                result.usage = UsageSummary::from_records(&records);
                result.usage_records = records;
            }
            Err(error) => {
                self.ctx.display.render_error(&format!(
                    "Failed to read usage records for turn {billing_turn_id}: {error}"
                ));
            }
        }
        self.ctx.usage.end_turn(billing_turn_id);
        result
    }

    fn prepare_turn(&mut self) -> (String, TurnExecutor) {
        let resolved = self.resolve_active();

        self.ctx.log_event(crate::events::EventLog::TurnStart {
            model: resolved.actual.clone(),
            model_alias: resolved.alias.clone(),
            belief: self.belief.belief(),
            forced_model: self.forced_model.clone(),
        });

        let model = resolved.actual.clone();
        let executor = TurnExecutor::new_for_model(self.ctx.clone(), resolved);
        (model, executor)
    }

    async fn post_process_turn(
        &mut self,
        decision: TurnDecision,
        effects: Vec<TurnEffect>,
        executor: &TurnExecutor,
        model: &str,
    ) -> TurnRunResult {
        for effect in &effects {
            self.ctx.display.render_info(effect);
        }

        self.log_turn_tracking(executor, &decision, model);

        if let TurnDecision::Failed(ref msg) = decision
            && msg != "interrupted"
        {
            self.ctx.display.render_error(msg);
        }
        if decision == TurnDecision::MaxTurnsExceeded {
            self.ctx
                .display
                .render_error("max_turns exhausted before end_turn");
        }
        TurnRunResult::from_decision(&decision, executor)
    }

    fn log_turn_tracking(&self, executor: &TurnExecutor, decision: &TurnDecision, model: &str) {
        let decision_str = match decision {
            TurnDecision::Stop => "Stop",
            TurnDecision::Interrupted => "Interrupted",
            TurnDecision::MaxTurnsExceeded => "MaxTurnsExceeded",
            TurnDecision::Failed(_) => "Failed",
        };
        self.ctx.log_event(crate::events::EventLog::TurnTracking {
            version: None,
            decision: decision_str.into(),
            tool_call_count: executor.tool_call_count(),
            tool_error_count: executor.tool_error_count(),
            belief: self.belief.belief(),
            model: model.into(),
        });
    }

    async fn log_turn_final(&self, result: &TurnRunResult, elapsed_ms: u64) {
        self.ctx.log_event(crate::events::EventLog::TurnFinal {
            billing_turn_id: result.billing_turn_id.clone(),
            status: result.status.as_str().into(),
            tool_call_count: result.tool_call_count,
            tool_error_count: result.tool_error_count,
            elapsed_ms,
            error: result.error.clone(),
            usage: result.usage.clone(),
        });
        if let Err(error) = self.ctx.flush_event_log().await {
            self.ctx
                .display
                .render_error(&format!("Event log flush failed: {error}"));
        }
    }

    async fn handle_turn_error(
        &mut self,
        e: anyhow::Error,
        executor: &TurnExecutor,
        model: &str,
    ) -> TurnRunResult {
        let info = errors::classify_anyhow(&e);
        let error = format!("{e}");
        self.ctx.log_event(crate::events::EventLog::TurnError {
            error: error.clone(),
            category: format!("{:?}", info.category),
            severity: Some(format!("{:?}", info.severity)),
            belief: Some(self.belief.belief()),
            model: Some(model.into()),
            elapsed_ms: None,
            idle_ms: None,
        });
        if info.severity == errors::ErrorSeverity::Fatal {
            self.ctx.display.render_error(&format!("Fatal error: {e}"));
        } else {
            self.ctx
                .display
                .render_error(&format!("Turn execution error: {e}"));
        }
        TurnRunResult {
            billing_turn_id: String::new(),
            status: TurnStatus::Failed,
            tool_call_count: executor.tool_call_count(),
            tool_error_count: executor.tool_error_count(),
            error: Some(error),
            usage_records: Vec::new(),
            usage: UsageSummary::default(),
            text: executor.text().to_string(),
            thinking: executor.thinking().to_string(),
        }
    }

    async fn refresh_title(&self) {
        let label = self.active_model_label();
        crate::ui::render_title_snapshot(&self.ctx, &label, self.belief.belief()).await;
    }

    async fn handle_model_command(
        &mut self,
        model: &str,
    ) -> anyhow::Result<crate::runtime::ModelSwitchOutcome> {
        let model = model.trim();
        if model.is_empty() {
            self.ctx
                .display
                .render_error("Model name must not be empty.");
            anyhow::bail!("Model name must not be empty.");
        }
        // Model switch is gated by the frozen session capability snapshot
        // (v7 §3.3): an Unsupported session accepts any model but stays
        // text-only; an image-capable session requires an exact capability
        // fingerprint match.
        let candidate = crate::capabilities::model_capabilities::SessionModelCapabilities::resolve(
            model,
            &self.ctx.config,
            self.ctx.llm_backend.as_ref(),
        );
        if !self.ctx.model_capabilities.is_compatible_with(&candidate) {
            self.ctx.display.render_error(&format!(
                "Model switch rejected: this session was initialized with image capability {}. The selected model has incompatible capabilities. Start a new session to use that model.",
                self.ctx.model_capabilities.capability_fingerprint
            ));
            anyhow::bail!(
                "model switch rejected: capability fingerprint {} does not match the session's frozen {}",
                candidate.capability_fingerprint,
                self.ctx.model_capabilities.capability_fingerprint
            );
        }
        if model == self.ctx.config.model {
            self.forced_model = None;
        } else {
            self.forced_model = Some(model.to_string());
        }
        self.ctx.compaction.clear_prompt_usage();
        let resolved = self.resolve_active();
        let label = resolved.label;
        self.ctx
            .display
            .render_info(&format!("Switched to {label} model."));
        // 控制操作没有 turn emitter，标题更新会落入空处；把快照随结果返回，
        // 由 CLI/TUI 适配层自行渲染（不在适配层重新解析模型别名）。
        let stats = crate::ui::title_snapshot(&self.ctx, self.belief.belief()).await;
        self.ctx.display.render_title_update(&label, &stats);
        Ok(crate::runtime::ModelSwitchOutcome { label, stats })
    }

    fn resolve_active(&self) -> crate::config::ResolvedModel {
        let requested = if let Some(forced) = self.forced_model.as_deref() {
            forced
        } else {
            &self.ctx.config.model
        };
        crate::config::model_resolver(&self.ctx.config).resolve(requested)
    }

    fn active_model_label(&self) -> String {
        self.resolve_active().label
    }
}

pub fn new_orchestrator(
    ctx: Arc<AgentSharedContext>,
) -> (OrchActor, mpsc::UnboundedSender<OrchCmd>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let actor = OrchActor::new(ctx, cmd_rx);
    (actor, cmd_tx)
}
