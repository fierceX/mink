use crate::tui::markdown::{push_msg_with_width, truncate_visual, wrap_lines_word};
use crate::tui::render::padded_content_area;
use crate::tui::state::{ClickAction, ClickTarget, TranscriptItem, TranscriptKind, TuiState};
use crate::tui::theme;
use crate::ui::{
    PlanTransitionDisplay, TodoChangeDisplay, TodoStatusDisplay, ToolPresentation, ToolResultKind,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentMode {
    Full,
    Inline,
}

impl ContentMode {
    fn interactive(self) -> bool {
        matches!(self, Self::Full)
    }
}

pub(super) fn render_content(f: &mut Frame, area: Rect, state: &mut TuiState, mode: ContentMode) {
    let content_area = padded_content_area(area);
    state.viewport.content_y = content_area.y;
    state.viewport.content_x = content_area.x;
    state.viewport.width = content_area.width;
    state.viewport.height = content_area.height as usize;
    let start = if mode == ContentMode::Full {
        0
    } else {
        state.inline.committed
    };
    let interactive = mode.interactive();
    let inner_w = content_area.width.max(1);
    ensure_history_layout(state, start, inner_w, interactive);
    ensure_stream_cache(state, inner_w);
    let history_len = state.cache.heights.total();
    let stream_len =
        state.cache.stream_code_lines.len() + state.cache.stream_lines.as_ref().map_or(0, Vec::len);
    let viewport = content_area.height as usize;
    let max_scroll = (history_len + stream_len).saturating_sub(viewport);
    state.viewport.max_scroll = max_scroll;
    let mut scroll = state.viewport.scroll.min(max_scroll);
    if state.viewport.auto_scroll {
        scroll = max_scroll;
    } else if let Some(anchor) = &state.viewport.anchor
        && let Some(&index) = state.cache.indices.get(&anchor.id)
        && let Some(item) = state.lines.get(index).filter(|item| item.id == anchor.id)
    {
        let row = item
            .cached_offsets
            .partition_point(|offset| *offset <= anchor.offset)
            .saturating_sub(1);
        scroll = (state.cache.heights.prefix(index) + row).min(max_scroll);
    }
    state.viewport.scroll = scroll;
    if !state.viewport.auto_scroll
        && state.cache.rebuild_next.is_none()
        && state.cache.pending.is_empty()
    {
        let index = state.cache.heights.locate(scroll);
        if let Some(item) = state.lines.get(index) {
            let row = scroll.saturating_sub(state.cache.heights.prefix(index));
            state.viewport.anchor =
                item.cached_offsets
                    .get(row)
                    .map(|offset| super::super::state::ReadingAnchor {
                        id: item.id,
                        offset: *offset,
                    });
        }
    }
    state.viewport.click_map = if interactive {
        build_visible_click_map(state, start, scroll, viewport)
    } else {
        Vec::new()
    };
    let mut visible = Vec::with_capacity(viewport);
    let mut index = state.cache.heights.locate(scroll);
    let mut row = state.cache.heights.prefix(index);
    while index < state.lines.len() && visible.len() < viewport {
        if let Some(lines) = &state.lines[index].cached_lines {
            let partial = if !interactive && index == start {
                state.inline.row_offset
            } else {
                0
            };
            visible.extend(
                lines
                    .iter()
                    .skip(partial + scroll.saturating_sub(row))
                    .take(viewport - visible.len())
                    .cloned(),
            );
            row += lines.len().saturating_sub(partial);
        }
        index += 1;
    }
    if visible.len() < viewport {
        visible.extend(
            state
                .cache
                .stream_code_lines
                .iter()
                .chain(state.cache.stream_lines.iter().flatten())
                .skip(scroll.saturating_sub(history_len))
                .take(viewport - visible.len())
                .cloned(),
        );
    }
    f.render_widget(Paragraph::new(Text::from(visible)), content_area);
}

pub(crate) fn ensure_history_layout(
    state: &mut TuiState,
    start: usize,
    width: u16,
    interactive: bool,
) {
    if state.cache.width != width || state.cache.interactive != interactive {
        state.cache.width = width;
        state.cache.interactive = interactive;
        state.cache.rebuild_next = Some(start);
    }
    while state.cache.heights.len() < state.lines.len() {
        state.cache.pending.insert(state.cache.heights.len());
        state.cache.heights.push(0);
    }
    // Committed items have no active layout, regardless of the previous viewport.
    for index in state.cache.start..start {
        state.cache.heights.set(index, 0);
    }
    state.cache.start = start;
    let began = std::time::Instant::now();
    let mut count = 0;
    loop {
        let next = if let Some(index) = state.cache.rebuild_next {
            if index >= state.lines.len() {
                state.cache.rebuild_next = None;
                continue;
            }
            state.cache.rebuild_next = Some(index + 1);
            Some(index)
        } else {
            state.cache.pending.pop_first()
        };
        let Some(index) = next else { break };
        state.cache.pending.remove(&index);
        if index < start || index >= state.lines.len() {
            continue;
        }
        let item = &mut state.lines[index];
        if !interactive && index == start && state.inline.row_offset > 0 {
            prepare_inline_item(item, width, &mut state.inline.row_offset);
        }
        state.cache.visited += 1;
        if item.cached_width != width || !item.cache_valid(interactive) {
            rebuild_message_cache(item, width, interactive);
            state.cache.parsed += 1;
        }
        state.cache.heights.set(
            index,
            item.cached_lines
                .as_ref()
                .map_or(0, Vec::len)
                .saturating_sub(if !interactive && index == start {
                    state.inline.row_offset
                } else {
                    0
                }),
        );
        state.cache.indices.insert(item.id, index);
        count += 1;
        if count >= 2048 || began.elapsed() >= std::time::Duration::from_millis(4) {
            break;
        }
    }
}

fn rebuild_message_cache(item: &mut TranscriptItem, inner_w: u16, interactive: bool) {
    let mut seg = build_message_segments(item, inner_w, interactive);
    let mut wrapped = wrap_lines_word(&seg, inner_w);
    if !item.collapsed
        && !item.collapse_overridden
        && item.collapse_policy.should_collapse_rendered(wrapped.len())
    {
        item.collapsed = true;
        seg = build_message_segments(item, inner_w, interactive);
        wrapped = wrap_lines_word(&seg, inner_w);
    }
    let mut offset = 0;
    item.cached_offsets = wrapped
        .iter()
        .map(|line| {
            let start = offset;
            offset += line
                .spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>();
            start
        })
        .collect();
    item.cached_width = inner_w;
    item.cached_lines = Some(wrapped);
    item.cached_collapsed = item.collapsed;
    item.cached_interactive = interactive;
}

#[cfg(test)]
pub(crate) fn transcript_item_lines(item: &TranscriptItem, width: u16) -> Vec<Line<'static>> {
    let segments = build_message_segments(item, width.max(1), false);
    wrap_lines_word(&segments, width.max(1))
}

pub(crate) fn prepare_inline_item(item: &mut TranscriptItem, width: u16, row_offset: &mut usize) {
    if *row_offset > 0 && item.cached_width != width {
        let remaining = item
            .cached_lines
            .take()
            .unwrap_or_default()
            .into_iter()
            .skip(*row_offset)
            .collect::<Vec<_>>();
        item.cached_lines = Some(wrap_lines_word(&remaining, width.max(1)));
        item.cached_width = width;
        *row_offset = 0;
    } else if item.cached_width != width || !item.cache_valid(false) {
        rebuild_message_cache(item, width.max(1), false);
    }
}

fn build_message_segments(
    item: &TranscriptItem,
    inner_w: u16,
    interactive: bool,
) -> Vec<Line<'static>> {
    if item.kind == TranscriptKind::Tool {
        return build_tool_card_segments(item, inner_w, interactive);
    }

    let mut seg = Vec::new();
    if item.collapsed {
        let max_w = (inner_w as usize).saturating_sub(4).max(1);
        push_msg_with_width(
            &mut seg,
            &collapsed_summary(item, max_w),
            item.kind,
            inner_w,
            item.tool_name.as_deref(),
        );
    } else if interactive && item.is_collapsible() {
        push_msg_with_width(
            &mut seg,
            &format!("▼ {}", item.text),
            item.kind,
            inner_w,
            item.tool_name.as_deref(),
        );
    } else {
        push_msg_with_width(
            &mut seg,
            &item.text,
            item.kind,
            inner_w,
            item.tool_name.as_deref(),
        );
    }
    seg
}

fn build_tool_card_segments(
    item: &TranscriptItem,
    inner_w: u16,
    interactive: bool,
) -> Vec<Line<'static>> {
    let mut lines = vec![tool_header_line(item, interactive)];
    if item.collapsed {
        return lines;
    }

    let body = tool_body_text(item);
    if !body.is_empty() {
        push_msg_with_width(
            &mut lines,
            &body,
            TranscriptKind::Tool,
            inner_w,
            item.tool_name.as_deref(),
        );
    }
    lines
}

fn tool_header_line(item: &TranscriptItem, interactive: bool) -> Line<'static> {
    let (status, status_style) = match item.tool_status {
        Some(crate::runtime::ToolStatus::Succeeded) => ("✓", theme::success()),
        Some(crate::runtime::ToolStatus::Failed(_)) => ("✗ failed", theme::error()),
        Some(crate::runtime::ToolStatus::Blocked(_)) => ("⊘ blocked", theme::info()),
        Some(crate::runtime::ToolStatus::Interrupted) => ("■ interrupted", theme::info()),
        None if item.sealed => ("? 状态未记录", theme::muted()),
        None => ("◇", theme::info()),
    };
    let name = item.tool_name.as_deref().unwrap_or("Tool");
    let summary = item.tool_summary.as_deref().unwrap_or("");
    let mut spans = Vec::new();
    if interactive && item.is_collapsible() {
        spans.push(Span::styled(
            if item.collapsed { "▶ " } else { "▼ " },
            theme::muted(),
        ));
    }
    spans.push(Span::styled(format!("{status} "), status_style));
    spans.push(Span::styled(
        name.to_string(),
        tool_kind_style(item).add_modifier(Modifier::BOLD),
    ));
    if !summary.is_empty() && summary != name {
        spans.push(Span::styled(format!("  {summary}"), theme::muted()));
    }
    if let Some(code) = item.tool_exit_code {
        spans.push(Span::styled(format!(" · exit {code}"), theme::muted()));
    }
    if item.collapsed
        && let Some(first) = item.artifacts.first()
    {
        let additional = item.artifacts.len().saturating_sub(1);
        let suffix = if additional == 0 {
            String::new()
        } else {
            format!(" +{additional}")
        };
        spans.push(Span::styled(
            format!(" · artifact://{}{suffix}", first.id),
            theme::secondary(),
        ));
    }
    Line::from(spans)
}

fn tool_kind_style(item: &TranscriptItem) -> Style {
    if matches!(
        item.presentation,
        Some(ToolPresentation::Plan(_) | ToolPresentation::Todo(_))
    ) {
        return theme::primary_bold();
    }
    let kind = item.tool_result_kind.unwrap_or_else(|| {
        match item.tool_name.as_deref().unwrap_or_default() {
            "Read" => ToolResultKind::FileRead,
            "Glob" | "Grep" => ToolResultKind::Search,
            "Write" => ToolResultKind::FileWrite,
            "Edit" => ToolResultKind::Edit,
            "Bash" | "Python" | "PythonSandbox" => ToolResultKind::Command,
            "PlanDraft" | "PlanConfirm" | "PlanClear" | "TodoWrite" | "TodoRead"
            | "TodoAdvance" => ToolResultKind::Control,
            "SubAgent" => ToolResultKind::SubAgent,
            _ => ToolResultKind::Text,
        }
    });
    match kind {
        ToolResultKind::FileRead => theme::secondary(),
        ToolResultKind::Search => theme::primary(),
        ToolResultKind::FileWrite | ToolResultKind::Edit => theme::info(),
        ToolResultKind::Command => theme::sub_agent(),
        ToolResultKind::Control => theme::primary_bold(),
        ToolResultKind::SubAgent => theme::sub_agent(),
        ToolResultKind::Text => theme::text(),
    }
}

fn tool_body_text(item: &TranscriptItem) -> String {
    let mut out = String::new();
    match item.presentation.as_ref() {
        Some(ToolPresentation::Plan(plan)) => {
            let label = match plan.transition {
                PlanTransitionDisplay::DraftSaved => "Plan draft saved · awaiting confirmation",
                PlanTransitionDisplay::DraftCancelled => "Plan draft cancelled",
                PlanTransitionDisplay::Confirmed => "Plan confirmed",
                PlanTransitionDisplay::Cleared => "Plan cleared",
            };
            out.push_str(label);
            if let Some(content) = plan.content.as_deref() {
                out.push_str("\n\n");
                out.push_str(content);
            }
        }
        Some(ToolPresentation::Todo(todo)) => {
            out.push_str(&format!(
                "Todos r{} · {} active · {} pending · {} completed",
                todo.revision, todo.counts.in_progress, todo.counts.pending, todo.counts.completed
            ));
            for change in &todo.changes {
                let line = match change {
                    TodoChangeDisplay::Added { item } => {
                        format!("+ added {}: {}", item.id, item.content)
                    }
                    TodoChangeDisplay::Updated { id, content } => {
                        format!("~ updated {id}: {content}")
                    }
                    TodoChangeDisplay::Removed { id } => format!("- removed {id}"),
                    TodoChangeDisplay::Completed { id } => format!("✓ completed {id}"),
                    TodoChangeDisplay::Activated { id } => format!("◉ activated {id}"),
                    TodoChangeDisplay::Paused { id } => format!("○ paused {id}"),
                    TodoChangeDisplay::Reopened { id } => format!("↻ reopened {id}"),
                };
                out.push('\n');
                out.push_str(&line);
            }
            for todo_item in &todo.items {
                let marker = match todo_item.status {
                    TodoStatusDisplay::Pending => "○",
                    TodoStatusDisplay::InProgress => "◉",
                    TodoStatusDisplay::Completed => "✓",
                };
                out.push_str(&format!(
                    "\n{marker} {}  {}",
                    todo_item.id, todo_item.content
                ));
            }
        }
        None => out.push_str(&item.text),
    }
    for artifact in &item.artifacts {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "Full output: artifact://{} · {} bytes",
            artifact.id, artifact.bytes
        ));
    }
    out
}

fn click_action_for_message(
    state: &TuiState,
    line_idx: usize,
    item: &TranscriptItem,
) -> Option<ClickAction> {
    match item.presentation.as_ref() {
        Some(ToolPresentation::Plan(_)) => return Some(ClickAction::OpenPlan),
        Some(ToolPresentation::Todo(_)) => return Some(ClickAction::OpenTodos),
        None => {}
    }
    if let Some(artifact) = item.artifacts.first() {
        return Some(ClickAction::OpenArtifact {
            id: artifact.id.clone(),
        });
    }
    if item.kind == TranscriptKind::SubAgent {
        return state
            .sub_agents
            .session_for_line(line_idx)
            .map(|session_id| ClickAction::OpenSubAgent {
                session_id: session_id.to_string(),
            });
    }
    item.is_collapsible().then_some(ClickAction::ToggleCollapse)
}

pub(crate) fn build_visible_click_map(
    state: &TuiState,
    start_index: usize,
    scroll: usize,
    viewport: usize,
) -> Vec<ClickTarget> {
    if viewport == 0 {
        return Vec::new();
    }
    let visible_end = scroll.saturating_add(viewport).saturating_sub(1);
    let mut out = Vec::new();
    let mut idx = state.cache.heights.locate(scroll).max(start_index);
    let mut current_row = state.cache.heights.prefix(idx);
    while let Some(item) = state.lines.get(idx) {
        let len = item.cached_lines.as_ref().map_or(0, Vec::len);
        if current_row > visible_end {
            break;
        }
        let end = current_row + len.saturating_sub(1);
        if len > 0
            && end >= scroll
            && let Some(action) = click_action_for_message(state, idx, item)
        {
            out.push(ClickTarget {
                line_idx: idx,
                start_row: current_row.saturating_sub(scroll),
                end_row: end.min(visible_end).saturating_sub(scroll),
                action,
            });
        }
        current_row += len;
        idx += 1;
    }
    out
}

pub(crate) fn collapsed_summary(item: &TranscriptItem, max_w: usize) -> String {
    let line_count = item.text.lines().count();
    let first = item
        .text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    let prefix = match item.kind {
        TranscriptKind::Tool => tool_result_summary_prefix(item, line_count),
        TranscriptKind::StreamThinking => "► thinking | ".to_string(),
        _ => "► ".to_string(),
    };
    let available = max_w.saturating_sub(unicode_width::UnicodeWidthStr::width(prefix.as_str()));
    format!("{prefix}{}", truncate_visual(first, available))
}

fn tool_result_summary_prefix(item: &TranscriptItem, line_count: usize) -> String {
    let tool = item.tool_name.as_deref().unwrap_or("tool");
    let bytes = item.text.len();
    let truncated = if item.text.contains("[... truncated:") {
        ", truncated"
    } else {
        ""
    };
    format!("► {tool}: {line_count} lines, {bytes} bytes{truncated} | ")
}

fn ensure_stream_cache(state: &mut TuiState, inner_w: u16) {
    if !state.streaming || state.stream_line.is_empty() {
        state.invalidate_stream_cache();
        return;
    }
    let cache_valid = state.cache.stream_lines.is_some()
        && state.cache.stream_width == inner_w
        && state.cache.stream_kind == state.stream_kind
        && state.cache.stream_revision == state.stream_revision;
    if cache_valid {
        return;
    }
    if state.stream_kind == TranscriptKind::StreamText
        && let Some(end) = state.stream_line.find('\n')
        && let Some((fence, info)) =
            crate::tui::stream_boundary::Fence::opening(&state.stream_line[..end])
    {
        let key = (fence.marker, fence.len, inner_w);
        if state.cache.stream_code_key != Some(key) {
            state.cache.stream_code_lines.clear();
            state.cache.stream_code_bytes = end + 1;
            state.cache.stream_code_key = Some(key);
            state.cache.stream_code_closed = false;
            if !info.is_empty() {
                state.cache.stream_code_lines.extend(wrap_lines_word(
                    &[Line::from(Span::styled(
                        format!("-- {info} --"),
                        theme::muted(),
                    ))],
                    inner_w,
                ));
            }
        }
        if !state.cache.stream_code_closed {
            let start = state.cache.stream_code_bytes;
            let mut complete = start;
            let mut tail = Vec::new();
            for raw in state.stream_line[start..].split_inclusive('\n') {
                if raw.ends_with('\n') && fence.closes(raw) {
                    state.cache.stream_code_closed = true;
                    break;
                }
                let lines = wrap_lines_word(
                    &[Line::from(Span::styled(
                        raw.trim_end_matches('\n').to_string(),
                        theme::muted(),
                    ))],
                    inner_w,
                );
                state.cache.stream_parses += 1;
                if raw.ends_with('\n') {
                    complete += raw.len();
                    state.cache.stream_code_lines.extend(lines);
                } else {
                    tail = lines;
                }
            }
            state.cache.stream_code_bytes = complete;
            if !state.cache.stream_code_closed {
                state.cache.stream_lines = Some(tail);
                state.cache.stream_width = inner_w;
                state.cache.stream_kind = state.stream_kind;
                state.cache.stream_revision = state.stream_revision;
                return;
            }
        }
        state.cache.stream_code_lines.clear();
    } else {
        state.cache.stream_code_key = None;
        state.cache.stream_code_lines.clear();
    }
    state.cache.stream_parses += 1;
    let mut seg = Vec::new();
    push_msg_with_width(
        &mut seg,
        &state.stream_line,
        state.stream_kind,
        inner_w,
        None,
    );
    state.cache.stream_lines = Some(wrap_lines_word(&seg, inner_w));
    state.cache.stream_width = inner_w;
    state.cache.stream_kind = state.stream_kind;
    state.cache.stream_revision = state.stream_revision;
}

#[cfg(test)]
pub(crate) fn visible_lines(
    history: &[Line<'static>],
    stream: &[Line<'static>],
    scroll: usize,
    viewport: usize,
) -> Vec<Line<'static>> {
    let mut visible = Vec::with_capacity(viewport.min(history.len() + stream.len()));
    if viewport == 0 {
        return visible;
    }
    if scroll < history.len() {
        let take = viewport.min(history.len() - scroll);
        visible.extend(history.iter().skip(scroll).take(take).cloned());
    }
    if visible.len() < viewport {
        let stream_skip = scroll.saturating_sub(history.len());
        let remaining = viewport - visible.len();
        visible.extend(stream.iter().skip(stream_skip).take(remaining).cloned());
    }
    visible
}

#[cfg(test)]
pub(crate) fn content_viewport_height(area_height: u16) -> usize {
    padded_content_area(Rect {
        x: 0,
        y: 0,
        width: 1,
        height: area_height,
    })
    .height as usize
}
