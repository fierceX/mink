use crate::config::TuiMode;
use crate::tui::notify::TaskNotificationKind;
use crate::tui::sanitize::{sanitize_tui_stream_chunk, sanitize_tui_text};
use crate::tui::state::{SubAgentDetail, TranscriptItem, TranscriptKind, TuiState, WorkState};
use crate::ui::{ArtifactDisplay, ToolPresentation, ToolResultKind};
use crate::ui::{StatsSnapshot, SubAgentStreamKind};
#[cfg(test)]
use tokio::sync::mpsc;

#[derive(Debug, Clone, Default)]
pub enum TuiSignal {
    GoalTurnStarted {
        turn_id: String,
    },
    /// Authoritative goal snapshot mirrored into the read-only status marker
    /// and goal view. The runtime owns the contract; the UI never edits it.
    GoalUpdated(Box<crate::ui::GoalDisplay>),
    /// A goal boundary typed by `mink-goal` (`_mink.goal_message`). Rendered as a
    /// transcript card from the structured payload; live and replay share it.
    GoalCard(Box<mink_goal::GoalMessage>),
    /// The terminal goal facts row appended when a goal ends.
    GoalEnded {
        goal_id: String,
        revision: u64,
    },
    GuidanceApplied {
        input_id: String,
        text: String,
    },
    TurnFinished {
        turn_id: String,
        status: crate::runtime::TurnStatus,
    },
    Thinking(String),
    Text(String),
    ToolCall {
        tool_use_id: Option<String>,
        tool_name: String,
        summary: String,
        /// Complete arguments as the runtime streamed them, for structured cards.
        input: Option<serde_json::Value>,
    },
    ToolResult {
        tool_use_id: Option<String>,
        tool_name: String,
        content: String,
        status: Option<crate::runtime::ToolStatus>,
        exit_code: Option<i32>,
        result_kind: ToolResultKind,
        presentation: Option<ToolPresentation>,
        artifacts: Vec<ArtifactDisplay>,
    },
    Error(String),
    Stop,
    Retry,
    Info(String),
    TitleUpdate(String, StatsSnapshot),
    SubAgentStatus {
        session_id: String,
        status: String,
        label: Option<String>,
        stop_reason: Option<String>,
        in_tokens: u64,
        out_tokens: u64,
    },
    SubAgentStream {
        session_id: String,
        kind: SubAgentStreamKind,
        content: String,
    },
    SubAgentOutput {
        session_id: String,
        status: String,
        label: Option<String>,
        stop_reason: Option<String>,
        thinking: String,
        text: String,
        in_tokens: u64,
        out_tokens: u64,
    },
    #[default]
    Shutdown,
}

fn sub_agent_title(
    session_id: &str,
    label: Option<&str>,
    status: &str,
    stop_reason: Option<&str>,
    in_tokens: u64,
    out_tokens: u64,
) -> String {
    let who = label
        .filter(|label| !label.is_empty())
        .unwrap_or(session_id);
    let reason = stop_reason
        .filter(|reason| !reason.is_empty())
        .map(|reason| format!(" [{reason}]"))
        .unwrap_or_default();
    format!("[sub-agent {who}] {status}{reason} (in={in_tokens}, out={out_tokens})")
}

impl TuiState {
    pub(crate) fn apply(&mut self, sig: TuiSignal) {
        self.invalidate_detail_resource("panel:Status");
        if let TuiSignal::SubAgentStatus { session_id, .. }
        | TuiSignal::SubAgentStream { session_id, .. }
        | TuiSignal::SubAgentOutput { session_id, .. } = &sig
        {
            self.invalidate_detail_resource(&format!("sub:{session_id}"));
        }
        match sig {
            TuiSignal::GoalUpdated(goal) => {
                self.mirrors.goal = Some(*goal);
                self.invalidate_detail_resource("goal");
            }
            TuiSignal::GoalCard(message) => {
                self.finalize_stream();
                self.push_line(TranscriptItem::goal_card(*message));
            }
            TuiSignal::GoalEnded { goal_id, revision } => {
                self.push_line(TranscriptItem::new(
                    format!(
                        "Goal {goal_id} ended (revision {revision}). See /goals for the contract."
                    ),
                    TranscriptKind::Info,
                ));
            }
            TuiSignal::GoalTurnStarted { turn_id } => {
                self.stream.active_turn_id = Some(turn_id);
                self.stream.stopping = false;
                self.stream.work_state = WorkState::WaitingModel;
                self.arm_task_notification();
            }
            TuiSignal::GuidanceApplied { input_id, text } => {
                if self.admission.applied_inputs.insert(input_id.clone()) {
                    self.finalize_stream();
                    self.push_line(TranscriptItem::new(
                        format!(
                            "> [Added to context] {}",
                            crate::tui::state::compact_user_input_for_display(&text)
                        ),
                        TranscriptKind::Info,
                    ));
                    self.admission.inputs.retain(|i| i.input_id != input_id);
                    self.stream.work_state = WorkState::WaitingModel;
                }
            }
            TuiSignal::TurnFinished { turn_id, status } => {
                self.apply_turn_finished(&turn_id, status);
            }
            TuiSignal::Thinking(c) => {
                self.apply_stream_chunk(
                    TranscriptKind::StreamThinking,
                    WorkState::StreamingThinking,
                    &c,
                );
            }
            TuiSignal::Text(c) => {
                self.apply_stream_chunk(TranscriptKind::StreamText, WorkState::StreamingText, &c);
            }
            TuiSignal::Stop => {
                self.finalize_stream();
                self.seal_incomplete_transcript("Result unavailable: turn stopped.");
                if self.stream.active_turn_id.is_none() {
                    self.stream.work_state = WorkState::Idle;
                    self.finish_task_notification(TaskNotificationKind::Completed);
                }
            }
            TuiSignal::Retry => {
                self.finalize_stream();
                self.stream.work_state = WorkState::WaitingModel;
            }
            TuiSignal::ToolCall {
                tool_use_id,
                tool_name,
                summary,
                input,
            } => {
                self.apply_tool_call(tool_use_id, tool_name, summary, input);
            }
            TuiSignal::Info(n) => {
                // 等待心跳（LLM 流式等待/首事件等待）：只在状态栏瞬时展示（如 `·30s`），
                // 无论是否流式都不进入 transcript。
                if let Some(label) = crate::tui::state::heartbeat_status_label(&n) {
                    self.stream.stream_status = Some(label);
                } else if self.stream.streaming {
                    // 流式期间不打断：文本可能正处在未闭合的 markdown 围栏中间，
                    // 直接 finalize 会让后续内容丢失围栏上下文而无法正确渲染。
                    // 非心跳告警推迟到流结束时落盘。
                    self.transcript.pending_infos.push(n);
                } else {
                    self.finalize_stream();
                    self.push_line(TranscriptItem::new(n, TranscriptKind::Info));
                }
            }
            TuiSignal::ToolResult {
                tool_use_id,
                tool_name,
                content,
                status,
                exit_code,
                result_kind,
                presentation,
                artifacts,
            } => {
                self.apply_tool_result(
                    tool_use_id,
                    tool_name,
                    content,
                    status,
                    exit_code,
                    result_kind,
                    presentation,
                    artifacts,
                );
            }
            TuiSignal::Error(m) => {
                self.finalize_stream();
                self.seal_incomplete_transcript("Result unavailable: turn failed.");
                self.stream.work_state = WorkState::Error;
                self.push_line(TranscriptItem::new(
                    format!("Error: {m}"),
                    TranscriptKind::Error,
                ));
                if self.stream.active_turn_id.is_none() {
                    self.finish_task_notification(TaskNotificationKind::Failed);
                }
            }
            TuiSignal::TitleUpdate(m, s) => {
                self.invalidate_detail_resource("panel:Status");
                self.mirrors.model = m;
                self.mirrors.stats = s;
            }
            TuiSignal::SubAgentStatus {
                session_id,
                status,
                label,
                stop_reason,
                in_tokens,
                out_tokens,
            } => {
                self.apply_sub_agent_status(
                    session_id,
                    status,
                    label,
                    stop_reason,
                    in_tokens,
                    out_tokens,
                );
            }
            TuiSignal::SubAgentStream {
                session_id,
                kind,
                content,
            } => {
                if let Some(idx) = self
                    .transcript
                    .sub_agents
                    .line_by_session
                    .get(&session_id)
                    .copied()
                    && let Some(line) = self.transcript.lines.get_mut(idx)
                    && let Some(ref mut detail) = line.sub_detail
                {
                    let content = sanitize_tui_text(&content);
                    match kind {
                        SubAgentStreamKind::Thinking => detail.thinking.push_str(&content),
                        SubAgentStreamKind::Text => detail.text.push_str(&content),
                    }
                }
            }
            TuiSignal::SubAgentOutput {
                session_id,
                status,
                label,
                stop_reason,
                thinking,
                text,
                in_tokens,
                out_tokens,
            } => {
                self.apply_sub_agent_output(
                    session_id,
                    status,
                    label,
                    stop_reason,
                    thinking,
                    text,
                    in_tokens,
                    out_tokens,
                );
            }
            TuiSignal::Shutdown => {}
        }
    }

    fn apply_turn_finished(&mut self, turn_id: &str, status: crate::runtime::TurnStatus) {
        if self.stream.active_turn_id.as_deref() != Some(turn_id) {
            return;
        }
        self.finalize_stream();
        self.seal_incomplete_transcript("Result unavailable: turn ended.");
        self.stream.active_turn_id = None;
        self.stream.stopping = false;
        self.refresh_inputs();
        let ok = status == crate::runtime::TurnStatus::Ok;
        self.stream.work_state = if ok || status == crate::runtime::TurnStatus::Interrupted {
            WorkState::Idle
        } else {
            WorkState::Error
        };
        if status == crate::runtime::TurnStatus::Interrupted {
            self.push_line(TranscriptItem::new(
                "Turn interrupted; unapplied inputs remain in /inputs.".into(),
                TranscriptKind::Info,
            ));
        }
        if mink_goal::is_goal_turn_id(&crate::runtime::TurnId::new(turn_id)) {
            self.push_line(TranscriptItem::new(
                "Goal turn ended; checking goal decision.".into(),
                TranscriptKind::Info,
            ));
        } else {
            self.finish_task_notification(if status == crate::runtime::TurnStatus::Interrupted {
                TaskNotificationKind::Interrupted
            } else if ok {
                TaskNotificationKind::Completed
            } else {
                TaskNotificationKind::Failed
            });
        }
    }

    fn apply_stream_chunk(&mut self, kind: TranscriptKind, work_state: WorkState, chunk: &str) {
        // 新内容到达：清除过期的瞬时等待状态（心跳标签）。
        self.stream.stream_status = None;
        if !self.stream.stream_line.is_empty() && self.stream.stream_kind != kind {
            self.stream.stream_line.push('\n');
            self.save_stream();
        } else if !self.stream.streaming || self.stream.stream_kind != kind {
            self.stream.stream_filter.reset();
        }
        let cleaned = sanitize_tui_stream_chunk(&mut self.stream.stream_filter, chunk);
        self.stream.stream_kind = kind;
        self.stream.streaming = true;
        self.stream.stream_line.push_str(&cleaned);
        self.stream.stream_revision = self.stream.stream_revision.wrapping_add(1);
        self.stream.work_state = work_state;
    }

    fn apply_tool_call(
        &mut self,
        tool_use_id: Option<String>,
        tool_name: String,
        summary: String,
        input: Option<serde_json::Value>,
    ) {
        self.finalize_stream();
        self.stream.work_state = WorkState::RunningTool;
        let index = self.push_line(TranscriptItem::new_tool_call(
            tool_use_id.clone(),
            tool_name,
            summary,
            input,
        ));
        if let Some(id) = tool_use_id {
            self.transcript.active_tools.insert(id, index);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_tool_result(
        &mut self,
        tool_use_id: Option<String>,
        tool_name: String,
        content: String,
        status: Option<crate::runtime::ToolStatus>,
        exit_code: Option<i32>,
        result_kind: ToolResultKind,
        presentation: Option<ToolPresentation>,
        artifacts: Vec<ArtifactDisplay>,
    ) {
        self.finalize_stream();
        self.stream.work_state = WorkState::WaitingModel;
        let existing = tool_use_id
            .as_ref()
            .and_then(|id| self.transcript.active_tools.remove(id))
            .filter(|index| {
                *index >= self.transcript.inline.committed
                    && self
                        .transcript
                        .lines
                        .get(*index)
                        .is_some_and(|item| !item.sealed)
            });
        let idx = if let Some(idx) = existing {
            idx
        } else {
            self.push_line(TranscriptItem::new_tool_result(
                tool_name.clone(),
                String::new(),
            ))
        };
        if let Some(item) = self.transcript.lines.get_mut(idx) {
            item.text = sanitize_tui_text(&content);
            item.tool_name = Some(sanitize_tui_text(&tool_name));
            item.tool_use_id = tool_use_id;
            item.tool_status = status;
            item.tool_exit_code = exit_code;
            item.tool_result_kind = Some(result_kind);
            item.presentation = presentation.clone();
            item.artifacts = artifacts;
            item.sealed = true;
            item.invalidate_cache();
        }
        self.transcript.unsealed.remove(&idx);
        match presentation {
            Some(ToolPresentation::Plan(plan)) => {
                self.invalidate_detail_resource("plan");
                self.invalidate_detail_resource("panel:Status");
                self.mirrors.plan = Some(plan);
            }
            Some(ToolPresentation::Todo(todos)) => self.apply_todo_presentation(&todos),
            Some(ToolPresentation::PtcRun(_)) | None => {}
        }
        self.invalidate_item(idx);
    }

    fn apply_sub_agent_status(
        &mut self,
        session_id: String,
        status: String,
        label: Option<String>,
        stop_reason: Option<String>,
        in_tokens: u64,
        out_tokens: u64,
    ) {
        let title = sub_agent_title(
            &session_id,
            label.as_deref(),
            &status,
            stop_reason.as_deref(),
            in_tokens,
            out_tokens,
        );
        let running = status == "launched" || status == "running";
        let terminal = matches!(
            status.as_str(),
            "ok" | "failed" | "timed_out" | "cancelled" | "channel_closed"
        );
        let sub_detail = running.then(|| SubAgentDetail {
            thinking: String::new(),
            text: String::new(),
        });
        if running {
            self.transcript
                .sub_agents
                .active_sessions
                .insert(session_id.clone());
            self.stream.work_state = WorkState::RunningSubAgent;
        } else if terminal {
            self.transcript
                .sub_agents
                .active_sessions
                .remove(&session_id);
            if self.transcript.sub_agents.active_sessions.is_empty() {
                self.stream.work_state = WorkState::WaitingModel;
            }
        }
        if let Some(idx) = self
            .transcript
            .sub_agents
            .line_by_session
            .get(&session_id)
            .copied()
            && let Some(line) = self.transcript.lines.get_mut(idx)
        {
            line.text = sanitize_tui_text(&title);
            line.sealed = !running;
            if running {
                self.transcript.unsealed.insert(idx);
            } else {
                self.transcript.unsealed.remove(&idx);
            }
            if running && line.sub_detail.is_none() {
                line.sub_detail = sub_detail;
            }
            line.invalidate_cache();
            self.invalidate_item(idx);
        } else {
            let mut item =
                TranscriptItem::new(title, TranscriptKind::SubAgent).with_sub_detail(sub_detail);
            item.sealed = !running;
            let idx = self.push_line(item);
            self.transcript
                .sub_agents
                .line_by_session
                .insert(session_id, idx);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_sub_agent_output(
        &mut self,
        session_id: String,
        status: String,
        label: Option<String>,
        stop_reason: Option<String>,
        thinking: String,
        text: String,
        in_tokens: u64,
        out_tokens: u64,
    ) {
        self.finalize_stream();
        self.transcript
            .sub_agents
            .active_sessions
            .remove(&session_id);
        self.stream.work_state = if self.transcript.sub_agents.active_sessions.is_empty() {
            WorkState::WaitingModel
        } else {
            WorkState::RunningSubAgent
        };
        let title = sub_agent_title(
            &session_id,
            label.as_deref(),
            &status,
            stop_reason.as_deref(),
            in_tokens,
            out_tokens,
        );
        let thinking = sanitize_tui_text(&thinking);
        let text = sanitize_tui_text(&text);
        let mut found_idx = None;
        if let Some(idx) = self
            .transcript
            .sub_agents
            .line_by_session
            .get(&session_id)
            .copied()
            && let Some(line) = self.transcript.lines.get_mut(idx)
        {
            line.text = sanitize_tui_text(&title);
            line.sealed = true;
            self.transcript.unsealed.remove(&idx);
            if let Some(ref mut detail) = line.sub_detail {
                detail.thinking = thinking;
                detail.text = text;
            } else {
                line.sub_detail = Some(SubAgentDetail { thinking, text });
            }
            line.invalidate_cache();
            found_idx = Some(idx);
        } else {
            let mut item = TranscriptItem::new(title, TranscriptKind::SubAgent)
                .with_sub_detail(Some(SubAgentDetail { thinking, text }));
            item.sealed = true;
            let idx = self.push_line(item);
            self.transcript
                .sub_agents
                .line_by_session
                .insert(session_id, idx);
        }
        if let Some(idx) = found_idx {
            self.invalidate_item(idx);
        }
    }
}

#[cfg(test)]
pub(crate) fn drain_signals(
    rx: &mut mpsc::UnboundedReceiver<TuiSignal>,
    state: &mut TuiState,
    mode: TuiMode,
) -> bool {
    const MAX_SIGNALS_PER_TICK: usize = 512;
    let mut pending = Vec::new();
    for _ in 0..MAX_SIGNALS_PER_TICK {
        let sig = match rx.try_recv() {
            Ok(sig) => sig,
            Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                break;
            }
        };
        match (pending.last_mut(), sig) {
            (Some(TuiSignal::Thinking(existing)), TuiSignal::Thinking(next))
            | (Some(TuiSignal::Text(existing)), TuiSignal::Text(next)) => {
                existing.push_str(&next);
            }
            (_, sig) => pending.push(sig),
        }
    }

    apply_signals(pending, state, mode)
}

pub(crate) fn apply_signals(signals: Vec<TuiSignal>, state: &mut TuiState, _mode: TuiMode) -> bool {
    let mut pending = Vec::new();
    for sig in signals {
        match (pending.last_mut(), sig) {
            (Some(TuiSignal::Thinking(existing)), TuiSignal::Thinking(next))
            | (Some(TuiSignal::Text(existing)), TuiSignal::Text(next)) => existing.push_str(&next),
            (_, sig) => pending.push(sig),
        }
    }
    let mut visible_change = false;
    for sig in pending {
        let visible = match &sig {
            TuiSignal::SubAgentStream { session_id, .. } => matches!(
                &state.view,
                crate::tui::state::View::SubAgentDetail {
                    session_id: visible,
                    ..
                } if visible == session_id
            ),
            _ => true,
        };
        state.apply(sig);
        state.promote_stable_stream_prefix();
        visible_change |= visible;
    }
    visible_change
}
