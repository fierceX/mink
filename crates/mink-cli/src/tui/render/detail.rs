use crate::tui::markdown::{render_md_with_tables_with_width, wrap_lines_word};
use crate::tui::render::padded_content_area;
use crate::tui::state::{TranscriptKind, TuiState};
use crate::tui::theme;
use crate::ui::{PlanTransitionDisplay, TodoStatusDisplay};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span, Text},
    widgets::Paragraph,
};

pub(super) fn render_detail_content(
    f: &mut Frame,
    area: Rect,
    session_id: &str,
    scroll: usize,
    state: &mut TuiState,
) {
    render_cached_detail(
        f,
        area,
        scroll,
        state,
        format!("sub:{session_id}"),
        |state, width| detail_lines_for_session_with_width(state, session_id, width),
    );
}

#[cfg(test)]
pub(crate) fn detail_lines_for_session(state: &TuiState, session_id: &str) -> Vec<Line<'static>> {
    detail_lines_for_session_with_width(state, session_id, 80)
}

pub(crate) fn detail_lines_for_session_with_width(
    state: &TuiState,
    session_id: &str,
    max_width: u16,
) -> Vec<Line<'static>> {
    let Some(line_idx) = state.sub_agents.line_by_session.get(session_id).copied() else {
        return vec![Line::from(Span::styled(
            format!("Sub-agent {session_id} is no longer available."),
            theme::muted(),
        ))];
    };
    let Some(line) = state
        .lines
        .get(line_idx)
        .filter(|line| line.kind == TranscriptKind::SubAgent)
    else {
        return vec![Line::from(Span::styled(
            format!("Sub-agent {session_id} has no detail line."),
            theme::muted(),
        ))];
    };
    let detail = line.sub_detail.as_ref().or_else(|| {
        state
            .restored_sub_detail
            .as_ref()
            .filter(|(id, _)| id == session_id)
            .map(|(_, detail)| detail)
    });
    let thinking = detail.map(|d| d.thinking.as_str()).unwrap_or("");
    let text = detail.map(|d| d.text.as_str()).unwrap_or("");
    let mut all_lines: Vec<Line<'static>> = Vec::new();
    all_lines.push(Line::from(Span::styled(
        line.text.clone(),
        theme::sub_agent(),
    )));
    all_lines.push(Line::from(""));

    if !thinking.is_empty() {
        all_lines.push(Line::from(Span::styled("── Thinking ──", theme::muted())));
        for raw in thinking.split('\n') {
            all_lines.push(Line::from(Span::styled(raw.to_string(), theme::muted())));
        }
        all_lines.push(Line::from(""));
    }

    if !text.is_empty() {
        all_lines.push(Line::from(Span::styled(
            "── Text ──",
            theme::primary_bold(),
        )));
        render_md_with_tables_with_width(&mut all_lines, text, max_width);
    }

    if all_lines.is_empty() {
        all_lines.push(Line::from(Span::styled("(no output)", theme::muted())));
    }
    wrap_lines_word(&all_lines, max_width.max(1))
}

pub(super) fn render_detail_bar(f: &mut Frame, area: Rect) {
    let text = Span::styled(" Esc: Back │ ↑↓ PgUp/PgDn: Scroll ", theme::info());
    f.render_widget(Paragraph::new(Line::from(text)), area);
}

pub(super) fn render_plan_content(f: &mut Frame, area: Rect, scroll: usize, state: &mut TuiState) {
    render_cached_detail(f, area, scroll, state, "plan".into(), plan_lines);
}
fn plan_lines(state: &TuiState, inner_w: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(plan) = state.plan.as_ref() {
        let status = match plan.transition {
            PlanTransitionDisplay::DraftSaved => "Plan draft · awaiting confirmation",
            PlanTransitionDisplay::DraftCancelled => "Plan draft cancelled",
            PlanTransitionDisplay::Confirmed => "Plan confirmed",
            PlanTransitionDisplay::Cleared => "No active plan",
        };
        lines.push(Line::from(Span::styled(status, theme::primary_bold())));
        lines.push(Line::default());
        if let Some(content) = plan.content.as_deref() {
            render_md_with_tables_with_width(&mut lines, content, inner_w);
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No plan state is available in this session.",
            theme::muted(),
        )));
    }
    lines
}

pub(super) fn render_todos_content(f: &mut Frame, area: Rect, scroll: usize, state: &mut TuiState) {
    render_cached_detail(f, area, scroll, state, "todos".into(), |state, _| {
        todo_lines(state)
    });
}
fn todo_lines(state: &TuiState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(todos) = state.todos.as_ref() {
        lines.push(Line::from(Span::styled(
            format!(
                "Todos r{} · {} active · {} pending · {} completed",
                todos.revision,
                todos.counts.in_progress,
                todos.counts.pending,
                todos.counts.completed
            ),
            theme::primary_bold(),
        )));
        lines.push(Line::default());
        for item in &todos.items {
            let marker = match item.status {
                TodoStatusDisplay::Pending => "○",
                TodoStatusDisplay::InProgress => "◉",
                TodoStatusDisplay::Completed => "✓",
            };
            lines.push(Line::from(format!(
                "{marker} {}  {}",
                item.id, item.content
            )));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No todo state is available in this session.",
            theme::muted(),
        )));
    }
    lines
}

pub(super) fn render_artifact_content(
    f: &mut Frame,
    area: Rect,
    scroll: usize,
    state: &mut TuiState,
) {
    let identity = format!(
        "artifact:{}",
        state
            .artifact_detail
            .as_ref()
            .map_or("", |artifact| artifact.id.as_str())
    );
    render_cached_detail(f, area, scroll, state, identity, |state, _| {
        artifact_lines(state)
    });
}
fn artifact_lines(state: &TuiState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(artifact) = state.artifact_detail.as_ref() {
        lines.push(Line::from(Span::styled(
            format!("artifact://{}", artifact.id),
            theme::primary_bold(),
        )));
        if artifact.truncated {
            lines.push(Line::from(Span::styled(
                "Showing the first 256 KiB. Use Read with a selector for later sections.",
                theme::info(),
            )));
        }
        lines.push(Line::default());
        for raw in artifact.content.lines() {
            lines.push(Line::from(raw.to_string()));
        }
    }
    lines
}

pub(super) fn render_panel_content(
    f: &mut Frame,
    area: Rect,
    state: &mut TuiState,
    panel: crate::tui::state::PanelKind,
    scroll: usize,
    selected: usize,
) {
    render_cached_detail(
        f,
        area,
        scroll,
        state,
        format!("panel:{panel:?}:{selected}"),
        |state, _| crate::tui::panels::panel_lines(state, panel, selected),
    );
}

fn render_cached_detail(
    f: &mut Frame,
    area: Rect,
    scroll: usize,
    state: &mut TuiState,
    identity: String,
    build: impl FnOnce(&TuiState, u16) -> Vec<Line<'static>>,
) {
    let content_area = padded_content_area(area);
    let width = content_area.width.max(1);
    let valid = state.cache.detail.as_ref().is_some_and(|cache| {
        cache.identity == identity
            && cache.width == width
            && cache.revision == state.detail_revision
    });
    if !valid {
        let lines = wrap_lines_word(&build(state, width), width);
        state.cache.detail = Some(crate::tui::state::DetailLayout {
            identity,
            revision: state.detail_revision,
            width,
            lines: std::sync::Arc::new(lines),
        });
        state.cache.detail_parses += 1;
    }
    let lines = &state.cache.detail.as_ref().expect("detail cache").lines;
    let viewport = content_area.height as usize;
    let max_scroll = lines.len().saturating_sub(viewport);
    let scroll = scroll.min(max_scroll);
    state.detail_height = viewport;
    state.detail_max_scroll = max_scroll;
    match &mut state.view {
        crate::tui::state::View::SubAgentDetail {
            scroll: current, ..
        }
        | crate::tui::state::View::Plan { scroll: current }
        | crate::tui::state::View::Todos { scroll: current }
        | crate::tui::state::View::Artifact { scroll: current }
        | crate::tui::state::View::Panel {
            scroll: current, ..
        } => *current = scroll,
        _ => {}
    }
    let visible = lines
        .iter()
        .skip(scroll)
        .take(viewport)
        .cloned()
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(Text::from(visible)), content_area);
}

#[cfg(test)]
pub(crate) fn detail_viewport_height(area_height: u16) -> usize {
    padded_content_area(Rect {
        x: 0,
        y: 0,
        width: 1,
        height: area_height,
    })
    .height as usize
}

#[cfg(test)]
#[path = "detail_tests.rs"]
mod tests;
