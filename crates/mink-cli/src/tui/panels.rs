use super::state::{ClickAction, PanelKind, TranscriptKind, TuiState};
use ratatui::text::Line;

pub(crate) fn detail_actions(state: &TuiState) -> Vec<(String, usize, ClickAction)> {
    let mut actions = Vec::new();
    if state.mirrors.plan.is_some() {
        actions.push(("Plan".into(), 0, ClickAction::OpenPlan));
    }
    if state.mirrors.todos.is_some() {
        actions.push(("Todos".into(), 0, ClickAction::OpenTodos));
    }
    if state.mirrors.goal.as_ref().is_some_and(|goal| !goal.hidden) {
        actions.push(("Goal".into(), 0, ClickAction::OpenGoal));
    }
    for (index, item) in state.transcript.lines.iter().enumerate() {
        if item.kind == TranscriptKind::SubAgent
            && let Some(id) = state.transcript.sub_agents.session_for_line(index)
        {
            actions.push((
                format!("Sub-agent {id}"),
                index,
                ClickAction::OpenSubAgent {
                    session_id: id.into(),
                },
            ));
        }
        for artifact in &item.artifacts {
            actions.push((
                format!("artifact://{}", artifact.id),
                index,
                ClickAction::OpenArtifact {
                    id: artifact.id.clone(),
                },
            ));
        }
        if index >= state.transcript.inline.committed && item.is_collapsible() {
            let label = match item.kind {
                TranscriptKind::Goal => "goal",
                _ => item.tool_name.as_deref().unwrap_or("thinking"),
            };
            actions.push((
                format!(
                    "{} {label}",
                    if item.collapsed { "Expand" } else { "Collapse" },
                ),
                index,
                ClickAction::ToggleCollapse,
            ));
        }
    }
    actions
}

pub(crate) fn panel_lines(
    state: &TuiState,
    panel: PanelKind,
    selected: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<String> =
        match panel {
            PanelKind::Help => crate::local::COMMON_COMMAND_HELP
                .iter()
                .chain(crate::local::TUI_EXTRA_HELP)
                .map(|line| (*line).into())
                .collect(),
            PanelKind::Status => {
                let stats = &state.mirrors.stats;
                vec![
                    format!("Model: {}", state.mirrors.model),
                    format!("Directory: {}", state.mirrors.cwd_label),
                    format!(
                        "Work: {} · {}",
                        state.stream.work_state.label(),
                        state.stream.stream_status.as_deref().unwrap_or("")
                    ),
                    format!(
                        "Turns: {} · Requests: {}",
                        stats.current_turn_count, stats.agent_request_count
                    ),
                    format!(
                        "Input: {} · Cache read: {} · Cache creation: {}",
                        stats.total_input_tokens,
                        stats.total_cache_read_tokens,
                        stats.total_cache_creation_tokens
                    ),
                    format!(
                        "Cache hit: {} · Output: {}",
                        stats.cache_pct(),
                        stats.total_output_tokens
                    ),
                    format!(
                        "Context: {} / {} ({})",
                        stats.current_context_tokens,
                        stats.max_context_tokens,
                        stats.ctx_pct()
                    ),
                    format!(
                        "Belief: {:.2} · Pending inputs: {}",
                        stats.belief,
                        state.admission.inputs.len()
                    ),
                    format!(
                        "Plan: {:?}",
                        state.mirrors.plan.as_ref().map(|plan| plan.transition)
                    ),
                    format!(
                        "Todos: {:?}",
                        state
                            .mirrors
                            .todos
                            .as_ref()
                            .map(|todo| (todo.revision, &todo.counts))
                    ),
                ]
            }
            PanelKind::Details => {
                let mut lines = detail_actions(state)
                    .into_iter()
                    .enumerate()
                    .map(|(index, (label, _, _))| {
                        format!("{} {label}", if index == selected { ">" } else { " " })
                    })
                    .collect::<Vec<_>>();
                lines.push("↑↓ Home/End: select · Enter: open or toggle · Esc: back".into());
                lines
            }
            PanelKind::Inputs => {
                let mut lines = state
                    .admission
                    .inputs
                    .iter()
                    .enumerate()
                    .map(|(index, receipt)| {
                        format!(
                            "{} {} · r{} · {}",
                            if index == selected { ">" } else { " " },
                            receipt.input_id,
                            receipt.revision,
                            super::inbox::input_status(receipt.status)
                        )
                    })
                    .collect::<Vec<_>>();
                lines.extend(state.admission.failed_drafts.iter().enumerate().map(
                    |(index, draft)| {
                        format!(
                            "{} Failed submission: {}",
                            if index + state.admission.inputs.len() == selected {
                                ">"
                            } else {
                                " "
                            },
                            draft.buf.lines().next().unwrap_or("[image]")
                        )
                    },
                ));
                lines.push(
                    "↑↓: select · e: edit · w: withdraw · r: explicitly resume/recover · Esc: back"
                        .into(),
                );
                if let Some(receipt) = state.admission.inputs.get(selected) {
                    lines.push(String::new());
                    lines.push(super::state::compact_user_input_for_display(
                        &receipt.input.text,
                    ));
                } else if let Some(draft) = selected
                    .checked_sub(state.admission.inputs.len())
                    .and_then(|index| state.admission.failed_drafts.get(index))
                {
                    lines.push(super::state::display_user_input(
                        &draft.buf,
                        &draft.pending_images,
                    ));
                }
                if let Some(notice) = &state.admission.input_notice {
                    lines.push(notice.clone());
                }
                lines
            }
        };
    if lines.is_empty() {
        lines.push("No items.".into());
    }
    lines
        .into_iter()
        .map(|line| Line::from(super::sanitize::sanitize_tui_text(&line)))
        .collect()
}
