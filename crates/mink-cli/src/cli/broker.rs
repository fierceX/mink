use super::*;

pub(super) fn render_agent_event(display: &dyn Display, kind: AgentEventKind) {
    match kind {
        AgentEventKind::Thinking { content } => display.render_thinking(&content),
        AgentEventKind::Text { content } => display.render_text(&content),
        AgentEventKind::ToolCall {
            id,
            name,
            summary,
            input,
        } => display.render_tool_call(&ToolCallDisplay {
            tool_use_id: &id,
            tool_name: &name,
            summary: &summary,
            input: Some(&input),
        }),
        AgentEventKind::ToolResult {
            tool_use_id,
            tool_name,
            content_preview,
            content,
            status,
            exit_code,
            result_kind,
            presentation,
            artifacts,
        } => display.render_tool_result(&PresentedToolResultDisplay {
            base: ToolResultDisplay {
                tool_name: &tool_name,
                content_preview: &content_preview,
                content: &content,
                tool_use_id: tool_use_id.as_deref(),
                exit_code,
            },
            status,
            result_kind,
            presentation: presentation.as_ref(),
            artifacts: &artifacts,
        }),
        AgentEventKind::Signal {
            signal_kind,
            severity,
            message,
        } => display.render_signal(&signal_kind, severity, &message),
        AgentEventKind::Stop { reason } => display.render_stop(&reason),
        AgentEventKind::Retry => display.render_retry(),
        AgentEventKind::Error { message } => display.render_error(&message),
        AgentEventKind::Info { message } => display.render_info(&message),
        AgentEventKind::TitleUpdate { model, stats } => display.render_title_update(&model, &stats),
        AgentEventKind::SubAgentStatus {
            session_id,
            status,
            label,
            stop_reason,
            in_tokens,
            out_tokens,
        } => display.render_sub_agent_status_meta(
            &session_id,
            &status,
            label.as_deref(),
            stop_reason.as_deref(),
            in_tokens,
            out_tokens,
        ),
        AgentEventKind::SubAgentOutput {
            session_id,
            status,
            label,
            stop_reason,
            thinking,
            text,
            in_tokens,
            out_tokens,
        } => display.render_sub_agent_output_meta(
            &session_id,
            &status,
            label.as_deref(),
            stop_reason.as_deref(),
            &thinking,
            &text,
            in_tokens,
            out_tokens,
        ),
        AgentEventKind::Prompt => display.render_prompt(),
        AgentEventKind::ClearLine => display.render_clear_line(),
        AgentEventKind::GoalUpdated { .. }
        | AgentEventKind::GoalFinished { .. }
        | AgentEventKind::TurnStarted
        | AgentEventKind::ConversationCommitted { .. }
        | AgentEventKind::Final { .. } => {}
    }
}

pub(super) async fn run_turn_rendered(
    handle: &AgentRuntimeHandle,
    input: String,
    display: &dyn Display,
) -> RuntimeResult<TurnOutcome> {
    let mut stream = handle.stream_turn(input)?;
    while let Some(event) = stream.recv().await {
        render_agent_event(display, event.kind);
    }
    stream.outcome().await
}

#[cfg(feature = "tui")]
pub(super) fn goal_boundary_signal(message: &serde_json::Value) -> Option<crate::tui::TuiSignal> {
    match crate::runtime::session::SessionReader::decode_goal_boundary(message)? {
        crate::runtime::session::GoalBoundaryEvent::Card(parsed) => {
            Some(crate::tui::TuiSignal::GoalCard(parsed))
        }
        crate::runtime::session::GoalBoundaryEvent::Ended { goal_id, revision } => {
            Some(crate::tui::TuiSignal::GoalEnded { goal_id, revision })
        }
    }
}

#[cfg(feature = "tui")]
async fn render_tui_stream(
    mut stream: crate::runtime::AgentEventStream,
    display: &dyn Display,
    signals: &mpsc::UnboundedSender<crate::tui::TuiSignal>,
) -> RuntimeResult<TurnOutcome> {
    let turn_id = stream.turn_id().to_string();
    while let Some(event) = stream.recv().await {
        match event.kind {
            AgentEventKind::ConversationCommitted { message, .. } => {
                if let Some(guidance) =
                    crate::runtime::session::SessionReader::decode_applied_guidance(&message)
                {
                    let _ = signals.send(crate::tui::TuiSignal::GuidanceApplied {
                        input_id: guidance.input_id,
                        text: guidance.text,
                    });
                } else if let Some(signal) = goal_boundary_signal(&message) {
                    // A goal boundary committed by the normal turn path (draft
                    // discussion) renders as the same card a goal turn does.
                    let _ = signals.send(signal);
                }
            }
            // Round stops do not determine the authoritative turn outcome.
            AgentEventKind::Stop { .. } | AgentEventKind::Final { .. } => {}
            kind => render_agent_event(display, kind),
        }
    }
    let outcome = stream.outcome().await;
    let _ = signals.send(crate::tui::TuiSignal::TurnFinished {
        turn_id,
        status: outcome
            .as_ref()
            .map(|o| o.status)
            .unwrap_or(crate::runtime::TurnStatus::Failed),
    });
    outcome
}

pub(crate) fn start_runtime_broker(
    handle: AgentRuntimeHandle,
    display: Arc<dyn Display>,
) -> mpsc::UnboundedSender<RuntimeCmd> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (work_tx, mut work_rx) = mpsc::unbounded_channel();
    let work_handle = handle.clone();
    let control_display = display.clone();
    tokio::spawn(async move {
        while let Some(command) = work_rx.recv().await {
            let result = match command {
                #[cfg(feature = "tui")]
                RuntimeCmd::RenderTuiStream { stream, signals } => {
                    render_tui_stream(stream, display.as_ref(), &signals)
                        .await
                        .map(|_| ())
                }
                RuntimeCmd::Run { input, done } => {
                    let result = run_turn_rendered(&work_handle, input, display.as_ref()).await;
                    if let Err(error) = &result {
                        display.render_error(&error.to_string());
                    }
                    if let Some(done) = done {
                        let _ = done.send(result);
                    }
                    continue;
                }
                RuntimeCmd::Goal(command) => {
                    crate::local::execute_goal_command(&work_handle, command)
                        .await
                        .map(|message| display.render_info(&message))
                        .map_err(|e| crate::runtime::RuntimeError::Command(e.to_string()))
                }
                RuntimeCmd::Compact => work_handle.compact().await.map(|outcome| match outcome {
                    crate::runtime::CompactOutcome::Compacted { reason } => {
                        display.render_info(&format!("Context compacted ({reason})."))
                    }
                    crate::runtime::CompactOutcome::Skipped { reason } => {
                        display.render_info(&format!("Compaction skipped: {reason}"))
                    }
                }),
                RuntimeCmd::SetModel(model) => {
                    match work_handle.set_model_with_outcome(model).await {
                        Ok(outcome) => {
                            // 控制操作没有 turn emitter：成功提示与标题快照在适配层渲染，
                            // 模型解析（别名 → 标签）仍归 core。
                            display.render_info(&format!("Switched to {} model.", outcome.label));
                            display.render_title_update(&outcome.label, &outcome.stats);
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
                RuntimeCmd::Interrupt => unreachable!("interrupts bypass the work queue"),
                RuntimeCmd::Exit => unreachable!("exit bypasses the work queue"),
            };
            if let Err(error) = result {
                display.render_error(&error.to_string());
            }
        }
    });
    tokio::spawn(async move {
        while let Some(command) = rx.recv().await {
            match command {
                RuntimeCmd::Exit => {
                    handle.interrupt_current_turn();
                    break;
                }
                RuntimeCmd::Interrupt => handle.interrupt_current_turn(),
                RuntimeCmd::Goal(command)
                    if matches!(
                        command,
                        crate::local::GoalCommand::Create(_)
                            | crate::local::GoalCommand::Human(_)
                            | crate::local::GoalCommand::Verified(_)
                            | crate::local::GoalCommand::Refine(_)
                    ) =>
                {
                    let _ = work_tx.send(RuntimeCmd::Goal(command));
                }
                RuntimeCmd::Goal(command) => {
                    let handle = handle.clone();
                    let display = control_display.clone();
                    tokio::spawn(async move {
                        let result = crate::local::execute_goal_command(&handle, command).await;
                        match result {
                            Ok(message) => display.render_info(&message),
                            Err(error) => display.render_error(&error.to_string()),
                        }
                    });
                }
                command => {
                    if work_tx.send(command).is_err() {
                        break;
                    }
                }
            }
        }
    });
    tx
}

pub(super) struct GoalTerminalSink {
    pub(super) turns: bool,
    pub(super) display: Arc<dyn Display>,
    #[cfg(feature = "tui")]
    pub(super) signals: Option<mpsc::UnboundedSender<crate::tui::TuiSignal>>,
}
#[async_trait::async_trait]
impl crate::runtime::EventSink for GoalTerminalSink {
    async fn on_event(&self, event: crate::runtime::AgentEvent) -> Result<(), String> {
        let goal_turn = self.turns && event.turn_id.as_ref().is_some_and(|id| id.is_goal_turn());
        let goal_notice = event.kind.is_goal_notice();
        // Lifecycle events drive the TUI status marker and detail view, so they
        // pass on the non-turn sink. The turn sink already renders its own
        // notices and must not double-report them.
        let lifecycle = matches!(
            &event.kind,
            AgentEventKind::GoalUpdated { .. } | AgentEventKind::GoalFinished { .. }
        );
        if lifecycle {
            #[cfg(feature = "tui")]
            if !self.turns
                && let Some(signals) = &self.signals
                && let AgentEventKind::GoalUpdated { goal } = &event.kind
            {
                let _ = signals.send(crate::tui::TuiSignal::GoalUpdated(Box::new(
                    crate::ui::GoalDisplay::from(goal.as_ref()),
                )));
            }
            render_agent_event(self.display.as_ref(), event.kind);
            return Ok(());
        }
        #[cfg(feature = "tui")]
        if self.turns
            && let AgentEventKind::ConversationCommitted { message, .. } = &event.kind
            && let Some(signal) = goal_boundary_signal(message)
        {
            // Goal boundary rows arrive on the goal-turn sink; the session sink
            // sees the same events and must not mint a second card.
            if let Some(signals) = &self.signals {
                let _ = signals.send(signal);
            }
        }
        if !goal_turn && (!goal_notice || self.turns) {
            return Ok(());
        }
        #[cfg(feature = "tui")]
        if let Some(signals) = &self.signals {
            match &event.kind {
                AgentEventKind::TurnStarted => {
                    if let Some(turn_id) = &event.turn_id {
                        let _ = signals.send(crate::tui::TuiSignal::GoalTurnStarted {
                            turn_id: turn_id.to_string(),
                        });
                    }
                }
                AgentEventKind::Final { outcome } => {
                    let _ = signals.send(crate::tui::TuiSignal::TurnFinished {
                        turn_id: outcome.turn_id.to_string(),
                        status: outcome.status,
                    });
                }
                AgentEventKind::ConversationCommitted { message, .. } => {
                    if let Some(guidance) =
                        crate::runtime::session::SessionReader::decode_applied_guidance(message)
                    {
                        let _ = signals.send(crate::tui::TuiSignal::GuidanceApplied {
                            input_id: guidance.input_id,
                            text: guidance.text,
                        });
                    }
                }
                AgentEventKind::Stop { .. } => return Ok(()),
                _ => {}
            }
        }
        render_agent_event(self.display.as_ref(), event.kind);
        Ok(())
    }
}
