use super::*;

impl super::TurnExecutor {
    /// 从 thinking/text 中回收漏报的工具调用。
    pub(super) fn scavenge_calls(
        &mut self,
        thinking: &str,
        text: &str,
        mut calls: Vec<ToolCallEvent>,
    ) -> (Vec<ToolCallEvent>, bool) {
        if thinking.is_empty() && text.is_empty() {
            return (calls, false);
        }
        let (scavenged, notes) = crate::repair::scavenge_combined(
            if thinking.is_empty() {
                None
            } else {
                Some(thinking)
            },
            if text.is_empty() { None } else { Some(text) },
            4,
        );
        self.local.scavenge_seq += 1;
        let mut recovered = false;
        for sc in &scavenged {
            let cid = format!("scavenged_{}_{}", self.local.scavenge_seq, calls.len());
            // A candidate whose arguments were declared but could not be parsed
            // (wrapper JSON damage or a `function.arguments` string) is already
            // known-bad: build the degraded candidate directly with the true
            // parse error instead of re-parsing a placeholder.
            if let Some(error) = &sc.parse_error {
                let degraded =
                    crate::sse::toolcall::degraded_tool_call(&sc.name, &cid, &sc.arguments, error);
                let duplicate = calls.iter().any(|c| {
                    c.name == degraded.name && c.storm_identity() == degraded.storm_identity()
                });
                if !duplicate {
                    self.ctx.log_event(crate::events::EventLog::Scavenge {
                        note: format!("recovered damaged tool call {}", sc.name),
                    });
                    calls.push(degraded);
                    recovered = true;
                }
                continue;
            }
            match build_tool_call_event(&sc.name, &cid, &sc.arguments) {
                Ok(call) => {
                    let duplicate = calls.iter().any(|c| {
                        c.name == call.name && c.storm_identity() == call.storm_identity()
                    });
                    if !duplicate {
                        self.ctx.log_event(crate::events::EventLog::Scavenge {
                            note: format!("recovered tool call {}", call.name),
                        });
                        calls.push(call);
                        recovered = true;
                    }
                }
                Err(error) => {
                    // A registered tool name with unusable arguments is a model
                    // format error, not noise: surface the degraded candidate so
                    // the round reports a ModelFormat tool result and feeds the
                    // format window. Plain JSON examples never reach this arm
                    // (scavenge only extracts tool-call shapes).
                    let degraded = crate::sse::toolcall::degraded_tool_call(
                        &sc.name,
                        &cid,
                        &sc.arguments,
                        &format!("{error:#}"),
                    );
                    let duplicate = calls.iter().any(|c| {
                        c.name == degraded.name && c.storm_identity() == degraded.storm_identity()
                    });
                    if !duplicate {
                        self.ctx.log_event(crate::events::EventLog::Scavenge {
                            note: format!("recovered unparsable tool call {}", degraded.name),
                        });
                        calls.push(degraded);
                        recovered = true;
                    }
                }
            }
        }
        for note in &notes {
            self.ctx
                .log_event(crate::events::EventLog::Scavenge { note: note.clone() });
        }
        (calls, recovered)
    }

    /// Formal announcement of accepted tool calls: display and the
    /// `tool_call` event are emitted only after the complete response has been
    /// judged, so a discarded candidate never surfaces as an executed call.
    pub(super) fn announce_tool_calls(&self, calls: &[ToolCallEvent]) {
        for call in calls {
            self.ctx.log_event(crate::events::EventLog::ToolCall {
                version: None,
                name: call.name.clone(),
                id: call.id.clone(),
                input: call.input_json.clone(),
            });
            let summary = build_tool_call_summary(&call.name, &call.fields);
            self.ctx
                .display
                .render_tool_call(&crate::ui::ToolCallDisplay {
                    tool_use_id: &call.id,
                    tool_name: &call.name,
                    summary: &summary,
                    input: Some(&call.input_json),
                });
        }
    }

    /// Phase 2: 持久化 assistant 消息 + 用量统计。
    pub(super) async fn persist_assistant(
        &self,
        text: &str,
        thinking: &str,
        calls: &[ToolCallEvent],
        usage: &Option<UsageEvent>,
    ) -> Result<()> {
        self.ctx.store.add_assistant(text, thinking, calls).await?;
        if let Some(u) = usage {
            self.ctx.stats.record_usage(u).await;
        }
        Ok(())
    }

    /// 执行一轮中的所有工具调用（Phase 3）。
    /// 处理 Plan 状态转换、子代理生成与收集、结果定稿、信号采集和持久化。
    /// 返回本批是否包含模型格式失败（参数解码失败）——同一 round 的多个
    /// 格式错只计一次。
    pub(super) async fn execute_tools_inner(
        &mut self,
        calls: Vec<ToolCallEvent>,
        mut belief: Option<&mut crate::agent::belief::BeliefTracker>,
        effects: &mut Vec<TurnEffect>,
    ) -> Result<bool> {
        self.local.tool_call_count += calls.len() as u32;
        let (calls_to_execute, mut guarded_results) = self.apply_signal_recovery_guard(calls);
        let executed = if calls_to_execute.is_empty() {
            Ok(Vec::new())
        } else {
            self.tools.execute_all(calls_to_execute.clone()).await
        };
        // A fatal infrastructure error keeps every real result produced so
        // far; only calls that never ran are recorded as explicitly not
        // executed (tool_call/result protocol stays complete). The error is
        // re-raised after persistence so the turn still stops.
        let (mut executed_results, fatal) = match executed {
            Ok(results) => (results, None),
            Err(crate::tools::runner::FatalBatchError { error, completed }) => {
                (completed, Some(error))
            }
        };
        let executed_count = executed_results.len();
        if let Some(error) = &fatal {
            for call in calls_to_execute.iter().skip(executed_count) {
                executed_results.push(crate::tools::runner::failed_tool_result(
                    call.id.clone(),
                    call.name.clone(),
                    call.fields.clone(),
                    format!("not executed: {error:#}"),
                ));
            }
        }
        guarded_results.append(&mut executed_results);
        let results = guarded_results;

        // Plan hand-off happens in place on the executed results: the tool
        // already bound its journal; Confirm/Clear record the transition and
        // keep their original effects text. No intermediate Vec/handler layer.
        let mut plan_transitions = Vec::new();
        let mut results = results;
        for result in &mut results {
            let Some(command) = result.plan_command.take() else {
                continue;
            };
            match command {
                crate::tools::plan::PlanCommand::SetDraft => {}
                crate::tools::plan::PlanCommand::Confirm => {
                    effects.push("Plan confirmed.");
                    plan_transitions.push((result.tool_use_id.clone(), command));
                }
                crate::tools::plan::PlanCommand::Clear => {
                    effects.push("Plan cleared.");
                    plan_transitions.push((result.tool_use_id.clone(), command));
                }
            }
        }

        let mut processed_results = self
            .sub_agents
            .process(results, &self.sub_agent_config)
            .await;
        self.tools.finalize_deferred_results(&mut processed_results);
        for result in &mut processed_results {
            let model_label = self.model_label().to_string();
            self.signal_processor
                .process(result, belief.as_deref_mut(), &self.ctx, &model_label)
                .await;
        }

        self.ctx.store.add_tool_results(&processed_results).await?;
        for (tool_use_id, command) in plan_transitions {
            self.tools
                .finish_plan_transition(&tool_use_id, command)
                .await?;
        }
        // A state store may have hit an unrecoverable publish fault while
        // executing these tools: the session must stop instead of letting
        // the model continue from a possibly divergent in-memory view.
        self.ctx.persistence_fault.check()?;
        self.observe_todo_progress(&processed_results);
        self.maybe_append_todo_progress_reminder().await?;

        for r in &processed_results {
            let preview = if r.tool_name == "Edit" {
                r.content.clone()
            } else if r.tool_name == "Read" || r.tool_name == "Write" {
                first_line(&r.content).to_string() + "\n"
            } else {
                truncate_str(&r.content, 200) + "\n"
            };
            self.ctx.log_event(crate::events::EventLog::ToolResult {
                version: Some(2),
                tool_use_id: r.tool_use_id.clone(),
                name: r.tool_name.clone(),
                content: r.content.clone(),
                status: r.status,
                exit_code: r.exit_code,
                result_kind: r.result_kind,
                presentation: r.presentation.clone(),
                artifacts: r.artifacts.clone(),
            });
            self.ctx
                .display
                .render_tool_result(&crate::ui::PresentedToolResultDisplay {
                    base: ToolResultDisplay {
                        tool_name: &r.tool_name,
                        content_preview: &preview,
                        content: &r.content,
                        tool_use_id: Some(&r.tool_use_id),
                        exit_code: r.exit_code,
                    },
                    status: r.status,
                    result_kind: r.result_kind,
                    presentation: r.presentation.as_ref(),
                    artifacts: &r.artifacts,
                });
        }
        if let Some(error) = fatal {
            return Err(error);
        }
        // Model-output format failures in this batch (bad argument decoding)
        // count once for the round; real execution failures follow their own
        // signal path.
        let had_format_error = processed_results.iter().any(|result| {
            result.failure_source == Some(crate::tools::metadata::ToolFailureSource::ModelFormat)
        });
        if had_format_error {
            // Separate diagnostic counter: format provenance never feeds the
            // tool-failure signal path.
            self.ctx.log_event(crate::events::EventLog::LlmRecovery {
                round: Some(self.local.round),
                attempt: 0,
                category: "tool_format".into(),
                retries: 0,
                window_errors: self.format_error_count() as usize,
                terminal: None,
            });
        }
        Ok(had_format_error)
    }

    fn observe_todo_progress(&mut self, results: &[crate::tools::runner::ToolExecution]) {
        for result in results {
            if !result.succeeded() {
                continue;
            }
            if result.tool_name == "TodoAdvance" {
                self.local.successful_work_calls_since_todo_advance = 0;
                continue;
            }
            if matches!(
                result.tool_name.as_str(),
                "Bash" | "Python" | "PythonSandbox" | "Write" | "Edit" | "TodoWrite" | "SubAgent"
            ) {
                self.local.successful_work_calls_since_todo_advance = self
                    .local
                    .successful_work_calls_since_todo_advance
                    .saturating_add(1);
            }
        }
    }

    pub(super) async fn maybe_append_todo_progress_reminder(&mut self) -> Result<()> {
        if self.local.todo_progress_reminder_sent
            || self.local.successful_work_calls_since_todo_advance < 8
            || !self
                .ctx
                .todo_store
                .snapshot()
                .items
                .iter()
                .any(|item| item.status == crate::session::todo::TodoStatus::InProgress)
        {
            return Ok(());
        }
        let Some(provider) = self.ctx.todo_advance_provider() else {
            return Ok(());
        };
        self.ctx
            .store
            .add_runtime_user(&format!(
                "<todo-progress-reminder>Active todo work has continued across several successful operations without a progress transition. Reassess the active batch and call {provider} if any item should be completed, paused, or otherwise advanced.</todo-progress-reminder>"
            ))
            .await?;
        self.local.todo_progress_reminder_sent = true;
        Ok(())
    }
}
