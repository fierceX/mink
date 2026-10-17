use crate::tui::markdown::truncate_visual;
use crate::tui::state::TuiState;
use crate::tui::theme;
use crate::ui::PlanTransitionDisplay;
use crate::ui::StatsSnapshot;
use crate::util::fmt_k;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

pub(super) fn render_status(f: &mut Frame, area: Rect, state: &TuiState) {
    let line = build_status_spans(state, area.width);
    f.render_widget(Paragraph::new(line), area);
}

pub(crate) fn build_status_spans(state: &TuiState, width: u16) -> Line<'static> {
    let items = visible_status_items(state, width);
    let mut spans = Vec::new();
    for item in items {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(item.text, item.style));
    }
    Line::from(spans)
}

#[cfg(test)]
pub(crate) fn build_status_line(state: &TuiState, width: u16) -> String {
    let mut out = String::new();
    for item in visible_status_items(state, width) {
        out.push(' ');
        out.push_str(&item.text);
    }
    truncate_visual(&out, width as usize)
}

#[derive(Clone)]
struct StatusItem {
    text: String,
    style: Style,
    priority: u8,
    /// Marker that opens a detail view when clicked. Only one is clickable in
    /// the status bar today; the rest of the line stays inert.
    click: Option<StatusClick>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusClick {
    Goal,
}

/// Column range `(start, end)` the goal marker occupies for `width`.
///
/// Returns `None` when the session has no goal, when the goal was cleared by
/// the user, or when the marker was dropped to fit a narrow terminal — in that
/// last case nothing is clickable, because nothing is drawn.
pub(crate) fn status_goal_span(state: &TuiState, width: u16) -> Option<(u16, u16)> {
    // `build_status_line` emits one leading space before every item, so the
    // first item's text starts at column 1, not 0.
    let mut column = 1u16;
    for item in visible_status_items(state, width) {
        let len = status_text_width(&item.text);
        if item.click == Some(StatusClick::Goal) {
            return Some((column, column.saturating_add(len)));
        }
        column = column.saturating_add(1 + len);
    }
    None
}

fn visible_status_items(state: &TuiState, width: u16) -> Vec<StatusItem> {
    let mut items = build_status_items(state);
    while status_width(&items) > width as usize {
        let Some((idx, _)) = items
            .iter()
            .enumerate()
            .max_by_key(|(_, item)| item.priority)
        else {
            break;
        };
        if items[idx].priority <= 1 {
            break;
        }
        items.remove(idx);
    }
    if status_width(&items) > width as usize
        && let Some(model) = items.iter_mut().find(|item| item.priority == 1)
    {
        let work_width = state.work_state.label().len() + 3;
        model.text = truncate_visual(&model.text, (width as usize).saturating_sub(work_width + 1));
    }
    items.retain(|item| !item.text.is_empty());
    items
}

fn build_status_items(state: &TuiState) -> Vec<StatusItem> {
    let s = &state.stats;
    let ti = s
        .total_input_tokens
        .saturating_add(s.total_cache_read_tokens)
        .saturating_add(s.total_cache_creation_tokens);
    let mut items = vec![StatusItem {
        text: super::super::sanitize::sanitize_tui_text(&state.model),
        style: theme::primary_bold(),
        priority: 1,
        click: None,
    }];
    if !state.cwd_label.is_empty() {
        items.push(StatusItem {
            text: format!("@{}", state.cwd_label),
            style: theme::muted(),
            priority: 4,
            click: None,
        });
    }
    if let Some(label) = goal_status_label(state.goal.as_ref()) {
        items.push(StatusItem {
            text: super::super::sanitize::sanitize_tui_text(&label),
            style: if state
                .goal
                .as_ref()
                .is_some_and(|goal| goal.status == "active")
            {
                theme::info()
            } else {
                theme::muted()
            },
            priority: 6,
            click: Some(StatusClick::Goal),
        });
    }
    if s.belief > 0.0 {
        items.push(StatusItem {
            text: format!("B:{:.2}", s.belief),
            style: theme::muted(),
            priority: 7,
            click: None,
        });
    }
    if let Some(plan) = state.plan.as_ref() {
        let label = match plan.transition {
            PlanTransitionDisplay::DraftSaved => "plan:draft",
            PlanTransitionDisplay::Confirmed => "plan:confirmed",
            PlanTransitionDisplay::DraftCancelled | PlanTransitionDisplay::Cleared => "plan:none",
        };
        items.push(StatusItem {
            text: label.to_string(),
            style: theme::info(),
            priority: 6,
            click: None,
        });
    }
    if let Some(todos) = state.todos.as_ref()
        && todos.counts.in_progress + todos.counts.pending > 0
    {
        items.push(StatusItem {
            text: format!("todo:{}/{}", todos.counts.in_progress, todos.counts.pending),
            style: theme::info(),
            priority: 6,
            click: None,
        });
    }
    items.push(StatusItem {
        text: format!("T:{}", StatsSnapshot::fmt_num(s.current_turn_count)),
        style: theme::muted(),
        priority: 8,
        click: None,
    });
    items.push(StatusItem {
        text: format!("R:{}", StatsSnapshot::fmt_num(s.agent_request_count)),
        style: theme::muted(),
        priority: 8,
        click: None,
    });
    items.push(StatusItem {
        text: format!("I:{}({})", fmt_k(ti), s.cache_pct()),
        style: theme::muted(),
        priority: 5,
        click: None,
    });
    items.push(StatusItem {
        text: format!("O:{}", fmt_k(s.total_output_tokens)),
        style: theme::muted(),
        priority: 5,
        click: None,
    });
    items.push(StatusItem {
        text: format!("C:{}({})", fmt_k(s.current_context_tokens), s.ctx_pct()),
        style: theme::muted(),
        priority: 2,
        click: None,
    });
    // 流式等待心跳的精简状态标签（如 `·30s`），渲染宽度不足时优先被裁剪。
    if let Some(status) = state.stream_status.as_deref() {
        items.push(StatusItem {
            text: status.to_string(),
            style: theme::info(),
            priority: 9,
            click: None,
        });
    }
    items.push(StatusItem {
        text: format!("[{}]", state.work_state.label()),
        style: theme::work_state(state.work_state),
        priority: 0,
        click: None,
    });
    items
}

/// Status-bar marker for the active goal, or `None` when nothing should be
/// shown. Cleared goals never appear: the user asked to stop seeing them.
fn goal_status_label(goal: Option<&crate::ui::GoalDisplay>) -> Option<String> {
    let goal = goal.filter(|goal| !goal.hidden)?;
    let mut label = format!(
        "goal:{} {}/{}",
        goal.status, goal.remaining_turns, goal.max_turns
    );
    // A finished goal is only interesting when it states how it was accepted;
    // running goals get the turn budget instead.
    if !goal.remaining_turns.eq(&0) || goal.status == "active" {
        return Some(label);
    }
    label = format!("goal:{}", goal.status);
    if let Some(basis) = goal.completion_basis.as_deref().filter(|b| !b.is_empty()) {
        label.push(' ');
        label.push_str(basis);
    }
    Some(label)
}

fn status_width(items: &[StatusItem]) -> usize {
    items
        .iter()
        .map(|item| 1 + status_text_width(&item.text) as usize)
        .sum()
}

/// Display columns of one status item. The click span and the painted line must
/// agree; `chars().count()` under-counts CJK/emoji and shifts the goal marker.
pub(crate) fn status_text_width(text: &str) -> u16 {
    unicode_width::UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16
}
