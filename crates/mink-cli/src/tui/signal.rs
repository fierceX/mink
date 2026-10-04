use crate::config::TuiMode;
use crate::tui::notify::TaskNotificationKind;
use crate::tui::sanitize::sanitize_tui_text;
use crate::tui::state::{SubAgentDetail, TranscriptItem, TranscriptKind, TuiState, WorkState};
use crate::ui::{ArtifactDisplay, ToolPresentation, ToolResultKind};
use crate::ui::{StatsSnapshot, SubAgentStreamKind};
#[cfg(test)]
use std::sync::mpsc;

#[derive(Debug, Clone, Default)]
pub enum TuiSignal {
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
        thinking: String,
        text: String,
        in_tokens: u64,
        out_tokens: u64,
    },
    #[default]
    Shutdown,
}

impl TuiState {
    pub(crate) fn apply(&mut self, sig: &TuiSignal) {
        self.invalidate_detail_resource("panel:Status");
        if let TuiSignal::SubAgentStatus { session_id, .. }
        | TuiSignal::SubAgentStream { session_id, .. }
        | TuiSignal::SubAgentOutput { session_id, .. } = sig
        {
            self.invalidate_detail_resource(&format!("sub:{session_id}"));
        }
        match sig {
            TuiSignal::GuidanceApplied { input_id, text } => {
                if self.applied_inputs.insert(input_id.clone()) {
                    self.finalize_stream();
                    self.push_line(TranscriptItem::new(
                        format!(
                            "> [Added to context] {}",
                            crate::tui::state::compact_user_input_for_display(text)
                        ),
                        TranscriptKind::Info,
                    ));
                    self.inputs.retain(|i| &i.input_id != input_id);
                    self.work_state = WorkState::WaitingModel;
                }
            }
            TuiSignal::TurnFinished { turn_id, status } => {
                if self.active_turn_id.as_ref() == Some(turn_id) {
                    self.finalize_stream();
                    self.seal_incomplete_transcript("Result unavailable: turn ended.");
                    self.active_turn_id = None;
                    self.stopping = false;
                    self.refresh_inputs();
                    let ok = *status == crate::runtime::TurnStatus::Ok;
                    self.work_state = if ok || *status == crate::runtime::TurnStatus::Interrupted {
                        WorkState::Idle
                    } else {
                        WorkState::Error
                    };
                    if *status == crate::runtime::TurnStatus::Interrupted {
                        self.push_line(TranscriptItem::new(
                            "Turn interrupted; unapplied inputs remain in /inputs.".into(),
                            TranscriptKind::Info,
                        ));
                    }
                    self.finish_task_notification(
                        if *status == crate::runtime::TurnStatus::Interrupted {
                            TaskNotificationKind::Interrupted
                        } else if ok {
                            TaskNotificationKind::Completed
                        } else {
                            TaskNotificationKind::Failed
                        },
                    );
                }
            }
            TuiSignal::Thinking(c) => {
                let c = sanitize_tui_text(c);
                self.stream_status = None;
                if !self.stream_line.is_empty()
                    && self.stream_kind != TranscriptKind::StreamThinking
                {
                    self.stream_line.push('\n');
                    self.save_stream();
                }
                self.stream_kind = TranscriptKind::StreamThinking;
                self.streaming = true;
                self.stream_line.push_str(&c);
                self.stream_revision = self.stream_revision.wrapping_add(1);
                self.work_state = WorkState::StreamingThinking;
            }
            TuiSignal::Text(c) => {
                let c = sanitize_tui_text(c);
                // 新内容到达：清除过期的瞬时等待状态（心跳标签）。
                self.stream_status = None;
                if !self.stream_line.is_empty() && self.stream_kind != TranscriptKind::StreamText {
                    self.stream_line.push('\n');
                    self.save_stream();
                }
                self.stream_kind = TranscriptKind::StreamText;
                self.streaming = true;
                self.stream_line.push_str(&c);
                self.stream_revision = self.stream_revision.wrapping_add(1);
                self.work_state = WorkState::StreamingText;
            }
            TuiSignal::Stop => {
                self.finalize_stream();
                self.seal_incomplete_transcript("Result unavailable: turn stopped.");
                if self.active_turn_id.is_none() {
                    self.work_state = WorkState::Idle;
                    self.finish_task_notification(TaskNotificationKind::Completed);
                }
            }
            TuiSignal::Retry => {
                self.finalize_stream();
                self.work_state = WorkState::WaitingModel;
            }
            TuiSignal::ToolCall {
                tool_use_id,
                tool_name,
                summary,
            } => {
                self.finalize_stream();
                self.work_state = WorkState::RunningTool;
                let index = self.push_line(TranscriptItem::new_tool_call(
                    tool_use_id.clone(),
                    tool_name.clone(),
                    summary.clone(),
                ));
                if let Some(id) = tool_use_id {
                    self.active_tools.insert(id.clone(), index);
                }
            }
            TuiSignal::Info(n) => {
                // 等待心跳（LLM 流式等待/首事件等待）：只在状态栏瞬时展示（如 `·30s`），
                // 无论是否流式都不进入 transcript。
                if let Some(label) = crate::tui::state::heartbeat_status_label(n) {
                    self.stream_status = Some(label);
                } else if self.streaming {
                    // 流式期间不打断：文本可能正处在未闭合的 markdown 围栏中间，
                    // 直接 finalize 会让后续内容丢失围栏上下文而无法正确渲染。
                    // 非心跳告警推迟到流结束时落盘。
                    self.pending_infos.push(n.clone());
                } else {
                    self.finalize_stream();
                    self.push_line(TranscriptItem::new(n.clone(), TranscriptKind::Info));
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
                self.finalize_stream();
                self.work_state = WorkState::WaitingModel;
                let existing = tool_use_id
                    .as_ref()
                    .and_then(|id| self.active_tools.remove(id))
                    .filter(|index| {
                        *index >= self.inline.committed
                            && self.lines.get(*index).is_some_and(|item| !item.sealed)
                    });
                let idx = if let Some(idx) = existing {
                    idx
                } else {
                    self.push_line(TranscriptItem::new_tool_result(
                        tool_name.clone(),
                        String::new(),
                    ))
                };
                if let Some(item) = self.lines.get_mut(idx) {
                    item.text = content.clone();
                    item.tool_name = Some(tool_name.clone());
                    item.tool_use_id.clone_from(tool_use_id);
                    item.tool_status = *status;
                    item.tool_exit_code = *exit_code;
                    item.tool_result_kind = Some(*result_kind);
                    item.presentation.clone_from(presentation);
                    item.artifacts.clone_from(artifacts);
                    item.sealed = true;
                    item.invalidate_cache();
                }
                self.unsealed.remove(&idx);
                match presentation {
                    Some(ToolPresentation::Plan(plan)) => {
                        self.invalidate_detail_resource("plan");
                        self.invalidate_detail_resource("panel:Status");
                        self.plan = Some(plan.clone());
                    }
                    Some(ToolPresentation::Todo(todos)) => self.apply_todo_presentation(todos),
                    None => {}
                }
                self.invalidate_item(idx);
            }
            TuiSignal::Error(m) => {
                self.finalize_stream();
                self.seal_incomplete_transcript("Result unavailable: turn failed.");
                self.work_state = WorkState::Error;
                self.push_line(TranscriptItem::new(
                    format!("Error: {m}"),
                    TranscriptKind::Error,
                ));
                if self.active_turn_id.is_none() {
                    self.finish_task_notification(TaskNotificationKind::Failed);
                }
            }
            TuiSignal::TitleUpdate(m, s) => {
                self.invalidate_detail_resource("panel:Status");
                self.model = m.clone();
                self.stats = s.clone();
            }
            TuiSignal::SubAgentStatus {
                session_id,
                status,
                in_tokens,
                out_tokens,
            } => {
                let title = format!(
                    "[sub-agent {}] {} (in={}, out={})",
                    session_id, status, in_tokens, out_tokens
                );
                let running = status == "launched" || status == "running";
                let terminal = matches!(
                    status.as_str(),
                    "ok" | "failed" | "timed_out" | "cancelled" | "channel_closed"
                );
                let sub_detail = if running {
                    Some(SubAgentDetail {
                        thinking: String::new(),
                        text: String::new(),
                    })
                } else {
                    None
                };
                if running {
                    self.sub_agents.active_sessions.insert(session_id.clone());
                    self.work_state = WorkState::RunningSubAgent;
                } else if terminal {
                    self.sub_agents.active_sessions.remove(session_id);
                    if self.sub_agents.active_sessions.is_empty() {
                        self.work_state = WorkState::WaitingModel;
                    }
                }
                if let Some(idx) = self.sub_agents.line_by_session.get(session_id).copied()
                    && let Some(line) = self.lines.get_mut(idx)
                {
                    line.text = title;
                    line.sealed = !running;
                    if running {
                        self.unsealed.insert(idx);
                    } else {
                        self.unsealed.remove(&idx);
                    }
                    if running && line.sub_detail.is_none() {
                        line.sub_detail = sub_detail;
                    }
                    line.invalidate_cache();
                    self.invalidate_item(idx);
                } else {
                    let mut item = TranscriptItem::new(title, TranscriptKind::SubAgent)
                        .with_sub_detail(sub_detail);
                    item.sealed = !running;
                    let idx = self.push_line(item);
                    self.sub_agents
                        .line_by_session
                        .insert(session_id.clone(), idx);
                }
            }
            TuiSignal::SubAgentStream {
                session_id,
                kind,
                content,
            } => {
                if let Some(idx) = self.sub_agents.line_by_session.get(session_id).copied()
                    && let Some(line) = self.lines.get_mut(idx)
                    && let Some(ref mut detail) = line.sub_detail
                {
                    let content = sanitize_tui_text(content);
                    match kind {
                        SubAgentStreamKind::Thinking => detail.thinking.push_str(&content),
                        SubAgentStreamKind::Text => detail.text.push_str(&content),
                    }
                }
            }
            TuiSignal::SubAgentOutput {
                session_id,
                status,
                thinking,
                text,
                in_tokens,
                out_tokens,
            } => {
                self.finalize_stream();
                self.sub_agents.active_sessions.remove(session_id);
                self.work_state = if self.sub_agents.active_sessions.is_empty() {
                    WorkState::WaitingModel
                } else {
                    WorkState::RunningSubAgent
                };
                let title = format!(
                    "[sub-agent {}] {} (in={}, out={})",
                    session_id, status, in_tokens, out_tokens
                );
                let thinking = sanitize_tui_text(thinking);
                let text = sanitize_tui_text(text);
                let mut found = false;
                if let Some(idx) = self.sub_agents.line_by_session.get(session_id).copied()
                    && let Some(line) = self.lines.get_mut(idx)
                {
                    line.text = title.clone();
                    line.sealed = true;
                    self.unsealed.remove(&idx);
                    if let Some(ref mut detail) = line.sub_detail {
                        detail.thinking = thinking.clone();
                        detail.text = text.clone();
                    }
                    line.invalidate_cache();
                    found = true;
                }
                if found && let Some(&idx) = self.sub_agents.line_by_session.get(session_id) {
                    self.invalidate_item(idx);
                }
                if !found {
                    let mut item = TranscriptItem::new(title, TranscriptKind::SubAgent)
                        .with_sub_detail(Some(SubAgentDetail {
                            thinking: thinking.clone(),
                            text: text.clone(),
                        }));
                    item.sealed = true;
                    let idx = self.push_line(item);
                    self.sub_agents
                        .line_by_session
                        .insert(session_id.clone(), idx);
                }
            }
            TuiSignal::Shutdown => {}
        }
    }
}

#[cfg(test)]
pub(crate) fn drain_signals(
    rx: &mut mpsc::Receiver<TuiSignal>,
    state: &mut TuiState,
    mode: TuiMode,
) -> bool {
    const MAX_SIGNALS_PER_TICK: usize = 512;
    let mut pending = Vec::new();
    for _ in 0..MAX_SIGNALS_PER_TICK {
        let sig = match rx.try_recv() {
            Ok(sig) => sig,
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
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
        state.apply(&sig);
        state.promote_stable_stream_prefix();
        visible_change |= visible;
    }
    visible_change
}
