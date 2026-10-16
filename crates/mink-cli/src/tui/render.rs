use crate::config::TuiMode;
use crate::tui::render::content::{ContentMode, render_content};
use crate::tui::render::detail::{
    render_artifact_content, render_cached_detail, render_detail_bar, render_detail_content,
    render_panel_content, render_plan_content, render_todos_content,
};
use crate::tui::render::file_picker::render_file_picker;
use crate::tui::render::input::{render_chips, render_input};
pub(crate) mod status;
use crate::tui::render::status::render_status;
pub(crate) use crate::tui::render::status::status_goal_span;
use crate::tui::state::{TuiState, View};
use crate::tui::theme;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
};

pub(super) mod content;
pub(crate) mod detail;

/// Detail body for the goal view. Read-only: the runtime owns the contract,
/// budget and lifecycle, so this view never proposes a change to them.
pub(crate) fn goal_detail_lines(state: &TuiState) -> Vec<Line<'static>> {
    let Some(goal) = state.goal.as_ref().filter(|goal| !goal.hidden) else {
        return vec![Line::from(Span::styled(
            "No goal state is available in this session.",
            theme::muted(),
        ))];
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("Goal {} · {} ({})", goal.id, goal.status, goal.phase),
        theme::primary_bold(),
    ))];
    // The objective, criteria, constraints and reason are host- or user-supplied
    // text persisted to disk and re-rendered on resume, so they pass the same
    // control-sequence sanitisation as model output.
    let field = |label: &str, value: &str| {
        Line::from(vec![
            Span::styled(
                format!("{}: ", crate::tui::sanitize::sanitize_tui_text(label)),
                theme::muted(),
            ),
            Span::raw(crate::tui::sanitize::sanitize_tui_text(value)),
        ])
    };
    if let Some(stage) = goal.review_stage {
        let label = match stage {
            mink::runtime::GoalReviewStage::Screening => "判断是否需要验收",
            mink::runtime::GoalReviewStage::Inspecting => "检查完成证据",
            mink::runtime::GoalReviewStage::Adjudicating => "宿主提交结论",
        };
        lines.push(field("Review stage", label));
    }
    if let Some(record) = goal.continuations.last().filter(|r| {
        r.proposal.candidate.contract_revision == goal.contract_revision
            && goal
                .reviews
                .last()
                .is_none_or(|f| f.candidate.iteration < r.proposal.candidate.iteration)
    }) {
        lines.push(field(
            "Partial completion",
            &format!(
                "round {}: {}",
                record.proposal.candidate.iteration, record.proposal.summary
            ),
        ));
        lines.push(field(
            "Remaining requirements",
            &record.proposal.remaining_requirements.join(", "),
        ));
        lines.push(field("Next step", &record.proposal.next_step));
        lines.push(field("Screening report", &record.report));
        lines.push(field("Verification", "本轮未进行完整证据验收"));
    }
    lines.push(field("Objective", &goal.objective));
    lines.push(field(
        "Acceptance",
        &match goal.completion_basis.as_deref() {
            Some(basis) if !basis.is_empty() => {
                format!("{} (basis: {basis})", goal.acceptance)
            }
            _ => goal.acceptance.clone(),
        },
    ));
    lines.push(field(
        "Turns",
        &format!(
            "{}/{} used · {}/{} remaining",
            goal.iterations, goal.max_turns, goal.remaining_turns, goal.max_turns
        ),
    ));
    lines.push(field(
        "Deadline",
        &goal
            .draft_duration_secs
            .filter(|_| goal.status == "draft")
            .map(|secs| format!("starts after confirmation: {secs} seconds"))
            .unwrap_or_else(|| format!("unix {}", goal.deadline_unix_secs)),
    ));
    lines.push(field(
        "Token soft limit",
        &goal
            .soft_token_limit
            .map(|limit| limit.to_string())
            .unwrap_or_else(|| "none".into()),
    ));
    if goal.status == "draft" {
        lines.push(field(
            "Confirm exact contract",
            &format!("/goal confirm {} {}", goal.id, goal.revision),
        ));
    }
    if !goal.original_request.is_empty() {
        lines.push(field("Original request", &goal.original_request));
    }
    let historical_review = goal.reviews.last().filter(|r| {
        r.candidate.goal_id == goal.id
            && r.candidate.contract_revision == goal.contract_revision
            && goal.status != "draft"
    });
    let latest_review = historical_review.filter(|r| {
        goal.review_candidate
            .as_ref()
            .is_none_or(|c| c == &r.candidate)
    });
    for subgoal in &goal.subgoals {
        let criteria: Vec<_> = goal
            .criteria
            .iter()
            .filter(|c| c.subgoal_id == subgoal.id)
            .collect();
        if latest_review.is_some_and(|review| review.report.is_some()) {
            lines.push(field(
                &subgoal.id,
                &format!("{} (see independent review report)", subgoal.objective),
            ));
            continue;
        }
        let passed = latest_review.map_or(0, |review| {
            criteria
                .iter()
                .filter(|c| {
                    review.criteria.iter().any(|check| {
                        check.criterion_id == c.id
                            && check.status == mink::runtime::GoalCriterionStatus::Pass
                    })
                })
                .count()
        });
        lines.push(field(
            &subgoal.id,
            &format!(
                "{} ({passed}/{} criteria reviewed as pass)",
                subgoal.objective,
                criteria.len()
            ),
        ));
    }
    for criterion in &goal.criteria {
        lines.push(field(
            &criterion.id,
            &format!(
                "{} · evidence: {}",
                criterion.outcome, criterion.required_evidence
            ),
        ));
    }
    if let Some(review) = historical_review {
        lines.push(field(
            "Independent review",
            &format!("round {}: {}", review.candidate.iteration, review.summary),
        ));
        if let Some(original) = &review.original_request {
            lines.push(field(
                "Original request review",
                &format!("{:?}: {}", original.status, original.reason),
            ));
        }
        if let Some(report) = &review.report {
            lines.push(field("Review report", &report.artifact));
        }
        for criterion in &review.criteria {
            lines.push(field(
                &criterion.criterion_id,
                &format!(
                    "{:?}: {} [{}]",
                    criterion.status,
                    criterion.reason,
                    criterion.evidence.join(", ")
                ),
            ));
        }
        for constraint in &review.constraints {
            lines.push(field(
                &constraint.constraint,
                &format!("{:?}: {}", constraint.status, constraint.reason),
            ));
        }
        lines.push(field("Next step", &review.next_step));
    }
    if !goal.acceptance_criteria.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Acceptance criteria",
            theme::primary_bold(),
        )));
        for criterion in &goal.acceptance_criteria {
            lines.push(Line::from(crate::tui::sanitize::sanitize_tui_text(
                &format!("  · {criterion}"),
            )));
        }
    }
    if !goal.scope.trim().is_empty() {
        lines.push(field("Scope", &goal.scope));
    }
    if !goal.constraints.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Constraints",
            theme::primary_bold(),
        )));
        for constraint in &goal.constraints {
            lines.push(Line::from(crate::tui::sanitize::sanitize_tui_text(
                &format!("  · {constraint}"),
            )));
        }
    }
    if let Some(reason) = goal.reason.as_deref().filter(|r| !r.trim().is_empty()) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("Decision", theme::primary_bold())));
        lines.push(Line::from(Span::styled(
            crate::tui::sanitize::sanitize_tui_text(reason),
            theme::muted(),
        )));
    }
    lines
}

pub(super) fn render_goal_content(f: &mut Frame, area: Rect, scroll: usize, state: &mut TuiState) {
    render_cached_detail(f, area, scroll, state, "goal".into(), |state, _w| {
        goal_detail_lines(state)
    });
}
mod file_picker;
mod input;

#[cfg(test)]
pub(crate) use content::transcript_item_lines;
#[cfg(test)]
pub(crate) use content::{collapsed_summary, content_viewport_height, visible_lines};
#[cfg(test)]
pub(crate) use detail::{detail_lines_for_session, detail_viewport_height};
pub(crate) use input::{chip_lines, clamp_input_scroll, split_at_visual_width};
#[cfg(test)]
pub(crate) use status::{build_status_line, build_status_spans};

pub(crate) fn render(f: &mut Frame, state: &mut TuiState, mode: TuiMode) {
    let area = f.area();
    if area.height < 5 || area.width < 20 {
        return;
    }

    let view = state.view.clone();
    match &view {
        View::Main => {
            state.input.clamp_cursor();
            let inner_w = area.width.saturating_sub(2).max(1) as usize;
            let input_layout = state.input.layout(inner_w);
            let vis_lines = &input_layout.lines;
            let cursor = input_layout.cursor(state.input.clamped_cursor());
            let cursor_row = cursor.row;
            // 待发送的剪贴板图片占输入框上方一行 chip（最多 2 行）；空间不足时
            // 直接不显示。文件选择器浮层紧贴输入框上沿绘制，重叠时会花屏，
            // 因此浮层打开期间隐藏 chip（关闭后恢复显示）。
            let chip_rows = if state.overlay.is_some() {
                0
            } else {
                usize::from(area.height.saturating_sub(5)).min(2)
            };
            let mut chips = Vec::new();
            if let Some(notice) = &state.input_notice {
                chips.push(crate::util::truncate_visual(notice, inner_w));
            }
            if let Some(input) = state.inputs.first() {
                let label = format!(
                    "Inputs {} · {} (/inputs)",
                    state.inputs.len(),
                    super::inbox::input_status(input.status)
                );
                chips.extend(split_at_visual_width(&label, inner_w));
            }
            chips.extend(chip_lines(&state.input.pending_images, inner_w, chip_rows));
            chips.truncate(chip_rows);
            let chips_height = chips.len();
            // 输入框正文最大行数受视口高度约束：正文行 + 2 行边框之外，
            // 布局还需要至少 1 行内容区、chip 行和 1 行状态栏。主视图入口保证
            // `area.height >= 5`，因此小视口（inline 最小 8 行）下不会把
            // 输入框压缩到越界，导致最后一行被裁剪、光标落到边框上。
            let max_content_lines = usize::from(area.height)
                .saturating_sub(4 + chips_height)
                .clamp(1, 5);
            let content_lines = vis_lines.len().clamp(1, max_content_lines);
            state.input.scroll_row = clamp_input_scroll(
                vis_lines.len(),
                cursor_row,
                content_lines,
                state.input.scroll_row,
            );
            let visible_input_lines: Vec<String> = vis_lines
                .iter()
                .skip(state.input.scroll_row)
                .take(content_lines)
                .cloned()
                .collect();
            let input_height = content_lines + 2;

            let mut constraints = vec![Constraint::Min(1)];
            if chips_height > 0 {
                constraints.push(Constraint::Length(chips_height as u16));
            }
            constraints.push(Constraint::Length(input_height as u16));
            constraints.push(Constraint::Length(1));
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            let input_area = chunks[chunks.len() - 2];
            let status_area = chunks[chunks.len() - 1];

            render_content(
                f,
                chunks[0],
                state,
                if mode == TuiMode::Full {
                    ContentMode::Full
                } else {
                    ContentMode::Inline
                },
            );
            if chips_height > 0 {
                render_chips(f, chunks[1], &chips);
            }
            let label = if state.admission_pending {
                "Accepting input · draft editable"
            } else if state.edit_input.is_some() {
                "Enter: save input edit"
            } else if state.stopping {
                "Stopping · draft kept"
            } else if state.active_turn_id.is_some() {
                "Enter: submit guidance"
            } else {
                "Enter: send task"
            };
            render_input(f, input_area, &visible_input_lines, label);
            render_file_picker(f, input_area, state);

            let row = cursor_row.saturating_sub(state.input.scroll_row);
            let col = cursor.col;
            let cursor_x =
                (input_area.x + 1 + col as u16).min(input_area.right().saturating_sub(2));
            let cursor_y =
                (input_area.y + 1 + row as u16).min(input_area.bottom().saturating_sub(2));
            f.set_cursor_position((cursor_x, cursor_y));

            render_status(f, status_area, state);
            // Record where the goal marker landed so a click can open it.
            // The status bar only exists in the main view, so the span is
            // cleared for every other view rather than left stale.
            state.status_row = status_area.y;
            state.status_goal_span = status_goal_span(state, status_area.width);
        }
        View::Panel {
            panel,
            scroll,
            selected,
        } => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area);
            render_panel_content(f, chunks[0], state, *panel, *scroll, *selected);
            render_detail_bar(f, chunks[1]);
        }
        View::SubAgentDetail { session_id, scroll } => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area);
            render_detail_content(f, chunks[0], session_id, *scroll, state);
            render_detail_bar(f, chunks[1]);
        }
        View::Plan { scroll } => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area);
            render_plan_content(f, chunks[0], *scroll, state);
            render_detail_bar(f, chunks[1]);
        }
        View::Goal { scroll } => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area);
            render_goal_content(f, chunks[0], *scroll, state);
            render_detail_bar(f, chunks[1]);
        }
        View::Todos { scroll } => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area);
            render_todos_content(f, chunks[0], *scroll, state);
            render_detail_bar(f, chunks[1]);
        }
        View::Artifact { scroll } => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(area);
            render_artifact_content(f, chunks[0], *scroll, state);
            render_detail_bar(f, chunks[1]);
        }
    }
}

pub(super) fn padded_content_area(area: Rect) -> Rect {
    let horizontal = if area.width > 2 { 1 } else { 0 };
    let bottom = if area.height > 1 { 1 } else { 0 };
    Rect {
        x: area.x.saturating_add(horizontal),
        y: area.y,
        width: area.width.saturating_sub(horizontal * 2),
        height: area.height.saturating_sub(bottom),
    }
}
