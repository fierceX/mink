use crate::cli::RuntimeCmd;
use crate::config::TuiMode;
use crate::tui::command::{SlashCommand, parse_slash_command};
use crate::tui::file_picker::FilePickerState;
use crate::tui::sanitize::normalize_tui_input;
use crate::tui::state::{
    ActiveOverlay, ClickAction, PanelKind, PendingImage, TranscriptItem, TranscriptKind, TuiState,
    TuiUiEvent, View, WorkState, display_user_input, submitted_user_input,
};
use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};
use std::time::{Duration, Instant};

pub(super) const SCROLL_STEP: usize = 3;
pub(super) const INTERRUPT_EXIT_WINDOW: Duration = Duration::from_secs(2);
/// A clipboard read that never reports back (hung `osascript`) must not
/// disable paste for the rest of the session: after this window a new read is
/// allowed to start.
pub(super) const CLIPBOARD_RETRY_WINDOW: Duration = Duration::from_secs(10);

pub(super) fn scroll_by(state: &mut TuiState, delta: isize) {
    let base = if state.viewport.auto_scroll {
        state.viewport.max_scroll
    } else {
        state.viewport.scroll
    };
    state.viewport.auto_scroll = false;
    state.viewport.anchor = None;
    if delta < 0 {
        state.viewport.scroll = base.saturating_sub(delta.unsigned_abs());
    } else {
        state.viewport.scroll = base
            .saturating_add(delta as usize)
            .min(state.viewport.max_scroll);
    }
}

mod editing;
mod keys;
mod picker;
mod submit;

pub(crate) use editing::handle_ctrl_c;
use keys::{handle_key, view_scroll_mut};
use picker::refresh_file_picker;

#[cfg(test)]
pub(crate) fn handle_event(
    ev: Event,
    state: &mut TuiState,
    orch_tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
) -> bool {
    handle_event_for_mode(ev, state, orch_tx, TuiMode::Full)
}

pub(crate) fn handle_event_for_mode(
    ev: Event,
    state: &mut TuiState,
    orch_tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
    mode: TuiMode,
) -> bool {
    let is_undo = matches!(&ev, Event::Key(key) if key.code == KeyCode::Char('z') && (key.modifiers == KeyModifiers::CONTROL || key.modifiers == KeyModifiers::ALT));
    let edit = matches!(&ev, Event::Paste(_))
        || matches!(&ev, Event::Key(key) if matches!(key.code, KeyCode::Backspace | KeyCode::Delete | KeyCode::Char(_)) || key.code == KeyCode::Enter && (key.modifiers != KeyModifiers::NONE || state.overlay.is_some()));
    if !matches!(&ev, Event::Key(key) if matches!(key.code, KeyCode::Up | KeyCode::Down)) {
        state.input.preferred_column = None;
    }
    let track_draft = matches!(&ev, Event::Key(_))
        || (matches!(&ev, Event::Paste(_)) && matches!(state.view, View::Main));
    let old_cursor = state.input.cursor;
    let old_snapshot =
        track_draft.then(|| (state.input.buf.clone(), state.input.pending_images.clone()));
    let mut quit = false;
    match ev {
        Event::Key(key) => quit = handle_key(key, state, orch_tx),
        Event::Mouse(mouse) => {
            if let Some(scroll) = view_scroll_mut(&mut state.view) {
                match mouse.kind {
                    MouseEventKind::ScrollUp => *scroll = scroll.saturating_sub(3),
                    MouseEventKind::ScrollDown => *scroll = scroll.saturating_add(3),
                    _ => {}
                }
            } else if mode == TuiMode::Full {
                match mouse.kind {
                    MouseEventKind::ScrollUp => scroll_by(state, -(SCROLL_STEP as isize)),
                    MouseEventKind::ScrollDown => scroll_by(state, SCROLL_STEP as isize),
                    MouseEventKind::Down(crossterm::event::MouseButton::Left)
                        if mouse.row == state.status_row
                            && state.status_goal_span.is_some_and(|(start, end)| {
                                (start..end).contains(&mouse.column)
                            }) =>
                    {
                        state.view = View::Goal { scroll: 0 };
                    }
                    MouseEventKind::Down(crossterm::event::MouseButton::Left)
                        if mouse.column >= state.viewport.content_x
                            && mouse.column
                                < state
                                    .viewport
                                    .content_x
                                    .saturating_add(state.viewport.width) =>
                    {
                        handle_full_click(state, mouse.row)
                    }
                    _ => {}
                }
            }
        }
        Event::Resize(..) => {}
        Event::Paste(content) => {
            if matches!(state.view, View::Main) {
                state.input.clamp_cursor();
                let to_insert = normalize_tui_input(&content);
                if !to_insert.is_empty() {
                    state.input.buf.insert_str(state.input.cursor, &to_insert);
                    state.input.cursor += to_insert.len();
                }
                refresh_file_picker(state);
            }
        }
        _ => {}
    }
    if let Some((old_text, old_images)) = old_snapshot
        && (old_text != state.input.buf || old_images != state.input.pending_images)
    {
        state.input.revision = state.input.revision.wrapping_add(1);
        if edit {
            state.input.history_idx = None;
        }
        if edit && !is_undo && old_text != state.input.buf {
            state.input.undo.push(super::editor::DraftEdit {
                text: old_text,
                cursor: old_cursor,
            });
            if state.input.undo.len() > 100 {
                state.input.undo.remove(0);
            }
            state.input.redo.clear();
        }
    }
    quit
}

fn handle_full_click(state: &mut TuiState, mouse_row: u16) {
    if mouse_row < state.viewport.content_y {
        return;
    }
    let row = usize::from(mouse_row - state.viewport.content_y);
    let action = state
        .viewport
        .click_map
        .iter()
        .find(|target| (target.start_row..=target.end_row).contains(&row))
        .map(|target| (target.line_idx, target.action.clone()));
    match action {
        Some((idx, ClickAction::ToggleCollapse)) => {
            if let Some(item) = state.lines.get_mut(idx) {
                item.toggle_collapsed();
                state.invalidate_item(idx);
            }
        }
        Some((_, ClickAction::OpenPlan)) => state.view = View::Plan { scroll: 0 },
        Some((_, ClickAction::OpenTodos)) => state.view = View::Todos { scroll: 0 },
        Some((_, ClickAction::OpenGoal)) => state.view = View::Goal { scroll: 0 },
        Some((_, ClickAction::OpenArtifact { id })) => state.open_artifact(&id),
        Some((_, ClickAction::OpenSubAgent { session_id })) => {
            state.open_sub_agent(&session_id);
        }
        None => {}
    }
}
