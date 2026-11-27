const MAX_REPLAN_ATTEMPTS_PER_INPUT: usize = 1;

use super::*;
use crate::agent::failure::TurnFailureReason;

impl super::TurnExecutor {
    pub(super) fn apply_signal_recovery_guard(
        &mut self,
        calls: Vec<ToolCallEvent>,
    ) -> (Vec<ToolCallEvent>, Vec<crate::tools::runner::ToolExecution>) {
        if !self.local.signal_recovery_guard || calls.is_empty() {
            return (calls, Vec::new());
        }

        // Calls already confirmed as pure model-format decode failures (or
        // carrying an explicit parse error) are dispatched so the runner
        // reports the format failure directly. They do not consume the guard:
        // it keeps waiting for the next valid call instead of rewriting a
        // known format problem into a blocked execution.
        let (known_format, rest): (Vec<_>, Vec<_>) = calls
            .into_iter()
            .partition(|call| self.tools.is_format_confirmed(call));
        if rest.is_empty() {
            return (known_format, Vec::new());
        }

        let mut iter = rest.into_iter();
        let first = iter
            .next()
            .expect("signal recovery guard already checked calls is non-empty");
        let call_context = crate::tools::semantic_capabilities::CapabilityCallContext {
            tool_name: &first.name,
            input: &first.input_json,
            resource_router: &self.ctx.tools.resource_router,
            filesystem_backend: self.ctx.tools.tool_resolution_context.filesystem_backend(),
        };
        let decision = self.recovery_policy.classify_first_call(&call_context);
        if let crate::agent::recovery_policy::RecoveryFirstCallDecision::Blocked(guidance) =
            decision
        {
            let guidance_content = match guidance.validate(&self.ctx.tools.tool_surface) {
                Ok(()) => guidance.content,
                Err(error) => {
                    self.ctx
                        .log_event(crate::events::EventLog::SignalRecoveryGuard {
                            action: "invalid_guidance_fallback".into(),
                            tool: first.name.clone(),
                            tool_use_id: first.id.clone(),
                            reason: format!("{error:#}"),
                            guard_blocks: self.local.guard_blocks,
                        });
                    "Please inspect the current state directly before continuing.".to_string()
                }
            };
            let max_blocks = self.ctx.config.signal.guard_max_blocks;
            if self.local.guard_blocks < max_blocks {
                self.local.guard_blocks += 1;
                self.ctx
                    .log_event(crate::events::EventLog::SignalRecoveryGuard {
                        action: "blocked_non_inspection".into(),
                        tool: first.name.clone(),
                        tool_use_id: first.id.clone(),
                        reason: guidance_content.clone(),
                        guard_blocks: self.local.guard_blocks,
                    });
                let blocked: Vec<crate::tools::runner::ToolExecution> = std::iter::once(first)
                    .chain(iter)
                    .map(|call| blocked_by_signal_recovery(call, guidance_content.clone()))
                    .collect();
                return (known_format, blocked);
            }
            // 连续拦截达到 guard_max_blocks：绕过守卫放行调用，并强制下一次
            // 决策注入证据。事件 action 与实际行为保持一致（bypassed 而非 blocked）。
            self.local.signal_recovery_guard = false;
            self.local.guard_blocks = 0;
            self.local.guard_bypassed = true;
            self.ctx
                .log_event(crate::events::EventLog::SignalRecoveryGuard {
                    action: "bypassed_max_blocks".into(),
                    tool: first.name.clone(),
                    tool_use_id: first.id.clone(),
                    reason: guidance_content,
                    guard_blocks: self.local.guard_blocks,
                });
            let mut allowed = known_format;
            allowed.push(first);
            allowed.extend(iter);
            return (allowed, Vec::new());
        }

        self.local.signal_recovery_guard = false;
        self.local.guard_blocks = 0;
        let mut allowed = known_format;
        allowed.push(first);
        allowed.extend(iter);
        (allowed, Vec::new())
    }

    /// A shared worktree has no portable compare-and-replace operation against
    /// external editors. Return the relevant edit evidence and let the next
    /// read/inspect step establish which changes are still appropriate.
    async fn suggest_edit_recovery(&self) -> Result<()> {
        let paths = self
            .signal_processor
            .evidence()
            .edited_paths_since(self.ctx.config.signal.seq_window);
        if paths.is_empty() {
            return Ok(());
        }
        let mut note = String::from(
            "[edit-recovery] Workspace preserved. Re-read the following paths and compare their recent Edit diffs before deciding whether to revert individual changes:",
        );
        let budget = self.ctx.config.signal.evidence_max_chars;
        for path in paths {
            if note.len() >= budget {
                break;
            }
            let full = self.ctx.paths.cwd.join(&path);
            let hint = self
                .ctx
                .tools
                .snapshots
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recovery_hint(&full, budget.saturating_sub(note.len()));
            note.push_str(&format!("\n- {path:?}"));
            if let Some(hint) = hint {
                note.push_str(&hint);
            }
        }
        self.ctx
            .stores
            .store
            .add_runtime_user(&truncate_str(
                &note,
                self.ctx.config.signal.evidence_max_chars,
            ))
            .await?;
        self.ctx
            .io
            .display
            .render_info("Workspace preserved; recent edits queued for review.");
        Ok(())
    }

    /// 只继承有界任务报告（目标/编辑路径/失败证据），用全新上下文重新规划。
    /// 成功返回子代理文本；不可用或失败返回 None（调用方降级为既有响应）。
    /// `belief` 为当前信念度，如实写入报告（禁止用占位值误导子代理）。
    async fn run_replan(&mut self, belief: f64) -> Result<Option<String>> {
        if !self.ctx.config.signal_policy.allows_restart() || self.tools.goal_read_only() {
            return Ok(None);
        }
        let s = self.ctx.config.signal.clone();
        if self.local.replan_attempts >= MAX_REPLAN_ATTEMPTS_PER_INPUT {
            return Ok(None);
        }
        if !self.ctx.tools.tool_surface.has("SubAgent") {
            return Ok(None);
        }
        self.local.replan_attempts += 1;
        let mut child_cfg = self.sub_agent_config.clone();
        child_cfg.max_turns = s.replan_max_turns;
        child_cfg.max_tokens = child_cfg.max_tokens.min(s.replan_token_budget);
        let session_id = format!("replan_{}", crate::session::paths::chrono_session_id());
        let report = self.build_replan_report(belief);
        let executor = match crate::agent::sub_executor::SubAgentExecutor::new_child(
            self.ctx.clone(),
            session_id.clone(),
            false,
            child_cfg,
            self.ctx.cancel.linked_child_token(),
            None,
            None,
            crate::agent::sub_executor::SubAgentOrigin::Replan,
        )
        .await
        {
            Ok(executor) => executor,
            Err(error) => {
                self.ctx
                    .log_critical_event(crate::events::EventLog::SignalReplanError {
                        attempts: self.local.replan_attempts,
                        session_id: session_id.clone(),
                        error: error.to_string(),
                    })
                    .await?;
                self.ctx.io.display.render_info(&format!(
                    "Signal replan unavailable: {error}; falling back to evidence/guard",
                ));
                return Ok(None);
            }
        };
        let result = executor.execute(report).await;
        self.ctx
            .log_critical_event(crate::events::EventLog::SignalReplan {
                attempts: self.local.replan_attempts,
                session_id: session_id.clone(),
                status: result.status.as_str().to_string(),
                text_len: result.text.len(),
            })
            .await?;
        if !matches!(
            result.status,
            crate::agent::sub_executor::SubAgentStatus::Succeeded
        ) || result.text.trim().is_empty()
        {
            return Ok(None);
        }
        let injection = format!(
            "[replan] A fresh sub-agent re-analyzed the task with a clean context and produced \
             this revised plan. Treat it as new evidence, not a user request.\n{}",
            result.text
        );
        self.ctx.stores.store.add_runtime_user(&injection).await?;
        self.ctx
            .io
            .display
            .render_info("Signal replan: fresh sub-agent produced a revised plan");
        Ok(Some(result.text))
    }

    /// 构造有界恢复任务报告；不携带父会话历史。
    fn build_replan_report(&self, belief: f64) -> String {
        let budget = self.ctx.config.signal.evidence_max_chars.min(2_000);
        let evidence = self.signal_processor.evidence().render(budget, belief).text;
        let paths = self.signal_processor.evidence().edited_paths.join(", ");
        format!(
            "You are re-planning for a parent coding agent that is stuck in a failure loop.\n\
             Original user goal: {}\n\
             Files edited this turn: {}\n\
             Failure evidence:\n{}\n\
             Produce a short revised plan: diagnose the most likely root cause, then list the \
             next 3 concrete verification-first steps. Output the plan only; do not continue \
             the parent's work.",
            self.local.current_user_input,
            if paths.is_empty() { "(none)" } else { &paths },
            evidence,
        )
    }

    /// Phase 4: 根据 stop reason 决策本轮是否结束。
    /// 返回 `Some(TurnDecision)` 表示需要从 execute() 返回，`None` 表示继续循环。
    /// `is_last_turn` 表示当前已经是最后一个允许的 LLM 轮次；此时 todo 提醒
    /// 只记录、不再强制发起一次额外的 LLM 请求。
    pub(super) async fn decide_next(
        &mut self,
        stop: &str,
        belief: Option<&mut crate::agent::belief::BeliefTracker>,
        is_last_turn: bool,
    ) -> Result<Option<TurnDecision>> {
        // 更新标题栏信念度
        let _ = self.ctx.stores.stats.flush_if_dirty().await;
        let current_belief = belief.as_ref().map_or(0.0, |bt| bt.belief());
        crate::ui::render_title_snapshot(&self.ctx, self.model_label(), current_belief).await;

        match stop {
            "tool_use" | "tool_calls" => {
                // DecisionEngine 决策是否注入（含内部冷却逻辑）
                let signal_enabled = self.ctx.config.signal_policy.enabled();
                if let Some(decision) = self.decide_signal_recovery(signal_enabled, belief).await? {
                    return Ok(Some(decision));
                }
                Ok(None) // 继续循环
            }
            "end_turn" | "stop" | "done" => {
                if !self.local.todo_final_reminder_sent
                    && self
                        .ctx
                        .stores
                        .todo_store
                        .snapshot()
                        .items
                        .iter()
                        .any(|item| item.status == crate::session::todo::TodoStatus::InProgress)
                    && let Some(provider) = self.ctx.todo_advance_provider()
                {
                    self.ctx
                        .stores.store
                        .add_runtime_user(&format!(
                            "<todo-final-reminder>Todo items remain in_progress. Before finishing, call {provider} to complete verified work or pause work that is no longer active. If the work is blocked and should remain active, state that explicitly.</todo-final-reminder>"
                        ))
                        .await?;
                    self.local.todo_final_reminder_sent = true;
                    if !is_last_turn {
                        return Ok(None);
                    }
                }
                self.ctx.io.display.render_stop(stop);
                Ok(Some(TurnDecision::Stop))
            }
            "error" | "max_tokens" | "length" => {
                self.ctx.io.display.render_stop(stop);
                Ok(Some(TurnDecision::Failed {
                    reason: TurnFailureReason::ProviderStopped,
                    detail: format!("stop: {stop}"),
                }))
            }
            _ => {
                self.ctx.io.display.render_stop(stop);
                Ok(Some(TurnDecision::Stop))
            }
        }
    }

    pub(super) async fn decide_signal_recovery(
        &mut self,
        signal_enabled: bool,
        belief: Option<&mut crate::agent::belief::BeliefTracker>,
    ) -> Result<Option<TurnDecision>> {
        if !signal_enabled {
            return Ok(None);
        }
        if let Some(bt) = belief {
            let b = bt.belief();
            let hard = self.signal_processor.hard_failures() as usize;
            let soft = self.signal_processor.soft_failures() as usize;
            let decision = if self.local.guard_bypassed {
                // 守卫达到上限被绕过：强制一次证据注入，即使处于冷却。
                self.local.guard_bypassed = false;
                crate::agent::decision::Decision::Inject(
                    crate::agent::decision::RecoveryDirective {
                        severity: crate::agent::decision::RecoverySeverity::Warning,
                    },
                )
            } else {
                self.decision_engine.decide_with_signals(b, hard, soft)
            };
            match decision {
                crate::agent::decision::Decision::Inject(directive) => {
                    let budget = self.ctx.config.signal.evidence_max_chars;
                    let batch = self.signal_processor.evidence().render(budget, b);
                    let fresh = self.signal_processor.evidence().is_fresh(batch.hash);
                    if fresh {
                        self.signal_processor
                            .evidence_mut()
                            .mark_injected(batch.hash);
                    }
                    let text = if fresh {
                        batch.text
                    } else {
                        format!(
                            "[trajectory]
- no new evidence since the last injection
[detector] belief {b:.2} (reference only)"
                        )
                    };
                    self.ctx
                        .io
                        .display
                        .render_info(&format!("Injecting trajectory evidence (belief {b:.2})"));
                    self.ctx.stores.store.add_runtime_user(&text).await?;
                    let policy = self.ctx.config.signal_policy;
                    // Warning 级在 state-ops 及以上策略提示核对近期编辑。
                    if directive.severity == crate::agent::decision::RecoverySeverity::Warning
                        && policy.allows_state_ops()
                    {
                        self.suggest_edit_recovery().await?;
                    }
                    // 同一输入内连续第二次 Warning 时用 fresh 子代理重新规划；
                    // 成功后注入新计划并跳过守卫。
                    let is_warning =
                        directive.severity == crate::agent::decision::RecoverySeverity::Warning;
                    if is_warning {
                        self.local.warning_count += 1;
                    }
                    let mut replanned = false;
                    if is_warning && self.local.warning_count >= 2 && policy.allows_restart() {
                        replanned = self.run_replan(b).await?.is_some();
                        if replanned {
                            bt.reset();
                            self.local.warning_count = 0;
                            self.local.guard_blocks = 0;
                        }
                    }
                    // 恢复守卫降级为状态门：仅在 Warning 级启用且未成功重规划时。
                    self.local.signal_recovery_guard =
                        is_warning && !replanned && policy.allows_state_ops();
                }
                crate::agent::decision::Decision::Abort => {
                    let policy = self.ctx.config.signal_policy;
                    // 非交互环境在 restart 及以上策略先尝试重启。
                    if policy.allows_restart()
                        && !self.ctx.config.interactive
                        && self.run_replan(b).await?.is_some()
                    {
                        // 策略重启成功：重置信念并继续本轮（新证据基线）。
                        // 同时清除恢复守卫和绕过标记，避免旧计划被上一次的守卫继续拦截。
                        bt.reset();
                        self.local.warning_count = 0;
                        self.local.guard_blocks = 0;
                        self.local.signal_recovery_guard = false;
                        self.local.guard_bypassed = false;
                        self.ctx.io.display.render_info(&format!(
                            "DecisionEngine: belief {b:.2} — retrying with a fresh replan instead of handing over",
                        ));
                        return Ok(None);
                    }
                    // 用户接管仅在 full 策略启用；较低策略直接失败。
                    if !policy.allows_handover() {
                        self.ctx
                            .io
                            .display
                            .render_error(&format!("DecisionEngine: aborting (belief {b:.2})."));
                        return Ok(Some(TurnDecision::Failed {
                            reason: TurnFailureReason::SignalAborted,
                            detail: "aborted by DecisionEngine".into(),
                        }));
                    }
                    let budget = self.ctx.config.signal.evidence_max_chars;
                    let batch = self.signal_processor.evidence().render(budget, b);
                    self.ctx
                        .log_critical_event(crate::events::EventLog::SignalHandover {
                            belief: b,
                            edited_paths: self.signal_processor.evidence().edited_paths.clone(),
                            evidence: batch.text.clone(),
                            options: vec![
                                "retry".into(),
                                "rollback_and_retry".into(),
                                "replan".into(),
                                "abandon".into(),
                            ],
                        })
                        .await?;
                    self.ctx.io.display.render_error(&format!(
                        "DecisionEngine: handing over (belief {b:.2}).\n{}",
                        batch.text
                    ));
                    return Ok(Some(TurnDecision::Failed {
                        reason: TurnFailureReason::SignalHandover,
                        detail:
                            "signal handover: reliability belief fell below the abort threshold"
                                .into(),
                    }));
                }
                crate::agent::decision::Decision::None => {}
            }
        }
        Ok(None)
    }
}
