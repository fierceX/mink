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

const SCROLL_STEP: usize = 3;
const INTERRUPT_EXIT_WINDOW: Duration = Duration::from_secs(2);
/// A clipboard read that never reports back (hung `osascript`) must not
/// disable paste for the rest of the session: after this window a new read is
/// allowed to start.
const CLIPBOARD_RETRY_WINDOW: Duration = Duration::from_secs(10);

fn scroll_by(state: &mut TuiState, delta: isize) {
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

fn handle_key(
    key: crossterm::event::KeyEvent,
    state: &mut TuiState,
    orch_tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
) -> bool {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return false;
    }
    state.input.clamp_cursor();

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&'c'))
    {
        return handle_ctrl_c(state, orch_tx);
    }

    if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('l') {
        state.viewport.auto_scroll = true;
        state.viewport.anchor = None;
        return false;
    }
    if handle_overlay_key(&key, state) {
        return false;
    }

    if handle_panel_key(&key, state, orch_tx) {
        return false;
    }
    if !matches!(state.view, View::Main) {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                state.view = View::Main;
                return false;
            }
            (KeyModifiers::NONE, KeyCode::PageUp) => {
                if let Some(scroll) = view_scroll_mut(&mut state.view) {
                    *scroll = scroll.saturating_sub(state.detail_height.max(1));
                }
                return false;
            }
            (KeyModifiers::NONE, KeyCode::PageDown) => {
                if let Some(scroll) = view_scroll_mut(&mut state.view) {
                    *scroll = scroll.saturating_add(state.detail_height.max(1));
                }
                return false;
            }
            (KeyModifiers::NONE, KeyCode::Up) => {
                if let Some(scroll) = view_scroll_mut(&mut state.view) {
                    *scroll = scroll.saturating_sub(1);
                }
                return false;
            }
            (KeyModifiers::NONE, KeyCode::Down) => {
                if let Some(scroll) = view_scroll_mut(&mut state.view) {
                    *scroll = scroll.saturating_add(1);
                }
                return false;
            }
            _ => return false,
        }
    }

    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Tab) => {
            open_file_picker(state);
        }
        (KeyModifiers::NONE, KeyCode::Esc) => {}
        (mods, KeyCode::Char(c)) if is_text_modifier(mods) => {
            if !c.is_control() {
                insert_char(state, c);
            }
        }
        (KeyModifiers::SHIFT, KeyCode::Enter)
        | (KeyModifiers::ALT, KeyCode::Enter)
        | (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
            // The file picker owns these keys while it is open: a newline in a
            // path query is meaningless and only blanks the candidate list.
            if state.overlay.is_none() {
                insert_char(state, '\n');
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
            state.input.cursor = 0;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
            state.input.cursor = state.input.buf.len();
        }
        (KeyModifiers::NONE, KeyCode::Home) => {
            state.input.cursor = state.input.buf[..state.input.cursor]
                .rfind('\n')
                .map_or(0, |pos| pos + 1);
        }
        (KeyModifiers::NONE, KeyCode::End) => {
            state.input.cursor += state.input.buf[state.input.cursor..]
                .find('\n')
                .unwrap_or(state.input.buf.len() - state.input.cursor);
        }
        (KeyModifiers::CONTROL, KeyCode::Char('z')) => state.input.undo_edit(false),
        (KeyModifiers::ALT, KeyCode::Char('z')) => state.input.undo_edit(true),
        (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
            state.viewport.auto_scroll = true;
            state.viewport.anchor = None;
        }
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => cursor_delete_before(state),
        (KeyModifiers::CONTROL, KeyCode::Char('k')) => cursor_delete_after(state),
        (mods, KeyCode::Char('v'))
            if mods.contains(KeyModifiers::CONTROL) || mods.contains(KeyModifiers::SUPER) =>
        {
            // Queuing an image behind an open overlay gives no visible
            // feedback and would garble the picker; require a closed overlay.
            if state.overlay.is_none() {
                request_clipboard_image(state)
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
            if state.input.buf.is_empty()
                && state.input.pending_images.is_empty()
                && state.clipboard_started.is_none()
                && !state.admission_pending
            {
                state.quit = true;
                return true;
            }
            cursor_delete_char_after(state);
        }
        (KeyModifiers::ALT, KeyCode::Left) | (KeyModifiers::ALT, KeyCode::Char('b')) => {
            cursor_word_left(state);
        }
        (KeyModifiers::ALT, KeyCode::Right) | (KeyModifiers::ALT, KeyCode::Char('f')) => {
            cursor_word_right(state);
        }
        (KeyModifiers::NONE, KeyCode::Left) => cursor_left(state),
        (KeyModifiers::NONE, KeyCode::Right) => cursor_right(state),
        (KeyModifiers::NONE, KeyCode::Enter) => return handle_enter(state, orch_tx),
        (KeyModifiers::NONE, KeyCode::Delete) => cursor_delete_char_after(state),
        (KeyModifiers::NONE, KeyCode::Backspace) => {
            if state.input.buf.is_empty() {
                // An empty input turns Backspace into "remove the last queued
                // clipboard image".
                state.input.pending_images.pop();
            } else {
                cursor_backspace(state);
            }
        }
        (KeyModifiers::CONTROL, KeyCode::Char('w')) | (KeyModifiers::ALT, KeyCode::Backspace) => {
            cursor_delete_word(state)
        }
        (KeyModifiers::NONE, KeyCode::PageUp) => {
            scroll_by(state, -(state.viewport.height.max(1) as isize));
        }
        (KeyModifiers::NONE, KeyCode::PageDown) => {
            scroll_by(state, state.viewport.height.max(1) as isize);
        }
        (KeyModifiers::NONE, KeyCode::Up) => {
            if !state.input.history.is_empty()
                && (state.input.buf.is_empty() || state.input.history_idx.is_some())
            {
                if state.input.history_idx.is_none() {
                    state.input.draft_before_history = Some(state.input.buf.clone());
                }
                let idx = match state.input.history_idx {
                    None => state.input.history.len().saturating_sub(1),
                    Some(i) => i.saturating_sub(1),
                };
                state.input.buf = state.input.history[idx].clone();
                state.input.cursor = state.input.buf.len();
                state.input.history_idx = Some(idx);
            } else if !state.input.buf.is_empty() {
                cursor_vertical(state, -1);
            } else {
                scroll_by(state, -(SCROLL_STEP as isize));
            }
        }
        (KeyModifiers::NONE, KeyCode::Down) => {
            if let Some(idx) = state.input.history_idx {
                let next = idx + 1;
                if next >= state.input.history.len() {
                    state.input.buf = state.input.draft_before_history.take().unwrap_or_default();
                    state.input.cursor = state.input.buf.len();
                    state.input.history_idx = None;
                } else {
                    state.input.buf = state.input.history[next].clone();
                    state.input.cursor = state.input.buf.len();
                    state.input.history_idx = Some(next);
                }
            } else if !state.input.buf.is_empty() {
                cursor_vertical(state, 1);
            } else {
                scroll_by(state, SCROLL_STEP as isize);
            }
        }
        _ => {}
    }
    refresh_file_picker(state);
    false
}

fn handle_panel_key(
    key: &crossterm::event::KeyEvent,
    state: &mut TuiState,
    tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
) -> bool {
    let View::Panel {
        panel, selected, ..
    } = state.view.clone()
    else {
        return false;
    };
    if !matches!(panel, PanelKind::Inputs | PanelKind::Details) {
        return false;
    }
    let actions = if panel == PanelKind::Details {
        super::panels::detail_actions(state)
    } else {
        Vec::new()
    };
    let count = if panel == PanelKind::Inputs {
        state.inputs.len() + state.failed_drafts.len()
    } else {
        actions.len()
    };
    if key.modifiers != KeyModifiers::NONE {
        return false;
    }
    match key.code {
        KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End => {
            let next = match key.code {
                KeyCode::Up => selected.saturating_sub(1),
                KeyCode::Down => (selected + 1).min(count.saturating_sub(1)),
                KeyCode::Home => 0,
                _ => count.saturating_sub(1),
            };
            if let View::Panel {
                selected, scroll, ..
            } = &mut state.view
            {
                *selected = next;
                *scroll = next;
            }
            true
        }
        KeyCode::Enter if panel == PanelKind::Details => {
            if let Some((_, index, action)) = actions.get(selected) {
                state.view = View::Main;
                activate_action(state, *index, action.clone());
            }
            true
        }
        KeyCode::Char('r') | KeyCode::Char('e') | KeyCode::Char('w')
            if panel == PanelKind::Inputs =>
        {
            if state.admission_pending {
                return true;
            }
            if let Some(receipt) = state.inputs.get(selected).cloned() {
                if !matches!(
                    receipt.status,
                    crate::runtime::InputStatus::Pending | crate::runtime::InputStatus::Unapplied
                ) {
                    state.input_notice = Some(
                        "Input is already applying; wait for its authoritative result.".into(),
                    );
                    return true;
                }
                if key.code == KeyCode::Char('e') {
                    state.edit_input = Some((receipt.input_id, receipt.revision));
                    state.input.buf = receipt.input.text;
                    state.input.cursor = state.input.buf.len();
                    state.input.revision = state.input.revision.wrapping_add(1);
                    state.input.undo.clear();
                    state.input.redo.clear();
                    state.view = View::Main;
                } else {
                    let command = if key.code == KeyCode::Char('r') {
                        SlashCommand::Resume(receipt.input_id)
                    } else {
                        SlashCommand::Withdraw(receipt.input_id)
                    };
                    if let Err(error) = state.inbox_command(&command, tx) {
                        state.input_rejected(error.to_string());
                    }
                }
            } else if key.code == KeyCode::Char('r') && selected >= state.inputs.len() {
                let index = selected - state.inputs.len();
                if index < state.failed_drafts.len() {
                    let draft = state.failed_drafts.remove(index);
                    state.input.buf = draft.buf;
                    state.input.cursor = draft.cursor;
                    state.input.pending_images = draft.pending_images;
                    state.input.undo.clear();
                    state.input.redo.clear();
                    state.input.revision = state.input.revision.wrapping_add(1);
                    state.view = View::Main;
                }
            }
            true
        }
        _ => false,
    }
}

fn activate_action(state: &mut TuiState, index: usize, action: ClickAction) {
    match action {
        ClickAction::ToggleCollapse => {
            if index >= state.inline.committed
                && let Some(item) = state.lines.get_mut(index)
            {
                item.toggle_collapsed();
                state.invalidate_item(index);
            }
        }
        ClickAction::OpenPlan => state.view = View::Plan { scroll: 0 },
        ClickAction::OpenTodos => state.view = View::Todos { scroll: 0 },
        ClickAction::OpenArtifact { id } => state.open_artifact(&id),
        ClickAction::OpenSubAgent { session_id } => state.open_sub_agent(&session_id),
    }
}

fn view_scroll_mut(view: &mut View) -> Option<&mut usize> {
    match view {
        View::SubAgentDetail { scroll, .. }
        | View::Plan { scroll }
        | View::Todos { scroll }
        | View::Artifact { scroll }
        | View::Panel { scroll, .. } => Some(scroll),
        View::Main => None,
    }
}

fn handle_overlay_key(key: &crossterm::event::KeyEvent, state: &mut TuiState) -> bool {
    let Some(ActiveOverlay::FilePicker(picker)) = state.overlay.as_mut() else {
        return false;
    };
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Esc) => {
            state.overlay = None;
            true
        }
        (KeyModifiers::NONE, KeyCode::Up) => {
            picker.move_selection(-1, 8);
            true
        }
        (KeyModifiers::NONE, KeyCode::Down) => {
            picker.move_selection(1, 8);
            true
        }
        (KeyModifiers::NONE, KeyCode::PageUp) => {
            picker.move_selection(-8, 8);
            true
        }
        (KeyModifiers::NONE, KeyCode::PageDown) => {
            picker.move_selection(8, 8);
            true
        }
        (KeyModifiers::NONE, KeyCode::Enter) => {
            accept_file_picker(state, false);
            true
        }
        (KeyModifiers::NONE, KeyCode::Tab) => {
            accept_file_picker(state, true);
            true
        }
        _ => false,
    }
}

fn accept_file_picker(state: &mut TuiState, keep_open_for_dirs: bool) {
    let Some(ActiveOverlay::FilePicker(picker)) = state.overlay.take() else {
        return;
    };
    let Some(path) = picker.selected_path() else {
        return;
    };
    let start = picker.replace_start.min(state.input.buf.len());
    let end = picker.replace_end.min(state.input.buf.len());
    if start <= end
        && state.input.buf.is_char_boundary(start)
        && state.input.buf.is_char_boundary(end)
    {
        state.input.buf.replace_range(start..end, &path);
        state.input.cursor = start + path.len();
        if keep_open_for_dirs && path.ends_with('/') {
            open_file_picker(state);
        }
    }
}

fn open_file_picker(state: &mut TuiState) {
    if state.ui_tx.is_none() {
        state.overlay = Some(ActiveOverlay::FilePicker(FilePickerState::open(
            &state.input.buf,
            state.input.cursor,
            &state.file_picker_policy,
        )));
        return;
    }
    state.overlay = Some(ActiveOverlay::FilePicker(FilePickerState::default()));
    request_file_picker(state, true);
}
fn request_file_picker(state: &mut TuiState, initial: bool) {
    let Some(tx) = state.ui_tx.clone() else {
        return;
    };
    let Some(ActiveOverlay::FilePicker(picker)) = state.overlay.as_mut() else {
        return;
    };
    let (start, end, query) =
        super::file_picker::path_query_at_cursor(&state.input.buf, state.input.cursor);
    if !initial
        && picker.query == query
        && picker.replace_start == start
        && picker.replace_end == end
    {
        return;
    }
    picker.query = query;
    picker.replace_start = start;
    picker.replace_end = end;
    // Stale candidates may not be accepted while a new query is being scanned.
    picker.items.clear();
    state.picker_generation = state.picker_generation.wrapping_add(1);
    let generation = state.picker_generation;
    let input = state.input.buf.clone();
    let cursor = state.input.cursor;
    let policy = state.file_picker_policy.clone();
    let worker = state
        .picker_worker
        .get_or_insert_with(|| super::file_picker::start_worker(tx));
    let _ = worker.send(super::file_picker::PickerRequest {
        generation,
        input,
        cursor,
        policy,
    });
}
fn refresh_file_picker(state: &mut TuiState) {
    if state.ui_tx.is_some() {
        request_file_picker(state, false);
    } else if let Some(ActiveOverlay::FilePicker(picker)) = state.overlay.as_mut() {
        picker.refresh_with_policy(
            &state.input.buf,
            state.input.cursor,
            &state.file_picker_policy,
        );
    }
}

fn is_text_modifier(modifiers: KeyModifiers) -> bool {
    modifiers == KeyModifiers::NONE || modifiers == KeyModifiers::SHIFT
}

pub(crate) fn handle_ctrl_c(
    state: &mut TuiState,
    orch_tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
) -> bool {
    let now = Instant::now();
    if state
        .last_interrupt
        .is_some_and(|at| now.duration_since(at) <= INTERRUPT_EXIT_WINDOW)
    {
        state.quit = true;
        return true;
    }

    if state.active_turn_id.is_some() || state.work_state.is_working() || state.admission_pending {
        let _ = orch_tx.send(RuntimeCmd::Interrupt);
        state.stopping = state.active_turn_id.is_some() || state.admission_pending;
        state.admission_cancelled |= state.admission_pending;
        state.last_interrupt = Some(now);
        return false;
    }

    state.quit = true;
    true
}

fn insert_char(state: &mut TuiState, c: char) {
    state.input.clamp_cursor();
    state.input.buf.insert(state.input.cursor, c);
    state.input.cursor += c.len_utf8();
}

/// Ctrl+V: read the system clipboard on a worker thread (the platform reader
/// spawns a subprocess, so it must not block the event loop) and stage the
/// image under the session attachment directory.
fn request_clipboard_image(state: &mut TuiState) {
    let Some(limits) = state.image_input.clone() else {
        state.push_line(TranscriptItem::new(
            "Clipboard image paste is unavailable: this session has no image input capability."
                .into(),
            TranscriptKind::Info,
        ));
        return;
    };
    if state
        .clipboard_started
        .is_some_and(|started| started.elapsed() < CLIPBOARD_RETRY_WINDOW)
    {
        return;
    }
    let Some(ui_tx) = state.ui_tx.clone() else {
        return;
    };
    if state.attachments_dir.as_os_str().is_empty() {
        state.push_line(TranscriptItem::new(
            "Clipboard image paste is unavailable: the session attachment directory is unknown."
                .into(),
            TranscriptKind::Error,
        ));
        return;
    }
    state.clipboard_started = Some(Instant::now());
    let dir = state.attachments_dir.clone();
    let reader = state.clipboard_reader.clone();
    std::thread::spawn(move || {
        let staged = match reader {
            Some(reader) => reader(&dir, &limits),
            None => crate::tui::clipboard::read_clipboard_png(&dir, &limits),
        }
        .and_then(|png| {
            let path = crate::tui::attachments::AttachmentStore::new(dir.clone())
                .commit_png(&png.bytes)?;
            // The marker quotes the path; a quote or control character inside
            // it could not be represented unambiguously for the model.
            let display = path.to_string_lossy();
            if display.contains('"') || display.chars().any(char::is_control) {
                anyhow::bail!(
                    "attachment path cannot be represented in the message marker: {display}"
                );
            }
            Ok(PendingImage {
                path,
                width: png.width,
                height: png.height,
                bytes: png.bytes.len(),
            })
        });
        let event = match staged {
            Ok(image) => TuiUiEvent::ImageCaptured(image),
            Err(error) => TuiUiEvent::ClipboardFailed(format!("{error:#}")),
        };
        let _ = ui_tx.send(event);
    });
}

fn cursor_left(state: &mut TuiState) {
    state.input.cursor = prev_char_boundary(&state.input.buf, state.input.cursor);
}
fn cursor_right(state: &mut TuiState) {
    state.input.cursor = next_char_boundary(&state.input.buf, state.input.cursor);
}
fn cursor_vertical(state: &mut TuiState, delta: isize) {
    let layout = state.input.layout(if state.cache.width == 0 {
        78
    } else {
        state.cache.width as usize
    });
    let col = state
        .input
        .preferred_column
        .unwrap_or_else(|| layout.cursor(state.input.cursor).col);
    state.input.preferred_column = Some(col);
    state.input.cursor = layout.vertical(state.input.cursor, delta, col);
}

fn cursor_word_left(state: &mut TuiState) {
    state.input.clamp_cursor();
    let mut pos = state.input.cursor;
    while pos > 0 {
        pos = prev_char_boundary(&state.input.buf, pos);
        let Some(ch) = char_at(&state.input.buf, pos) else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
    }
    while pos > 0 {
        let prev = prev_char_boundary(&state.input.buf, pos);
        let Some(ch) = char_at(&state.input.buf, prev) else {
            break;
        };
        if ch.is_whitespace() {
            break;
        }
        pos = prev;
    }
    state.input.cursor = pos;
}

fn cursor_word_right(state: &mut TuiState) {
    state.input.clamp_cursor();
    let mut pos = state.input.cursor;
    while pos < state.input.buf.len() {
        let Some(ch) = char_at(&state.input.buf, pos) else {
            break;
        };
        if ch.is_whitespace() {
            break;
        }
        pos = next_char_boundary(&state.input.buf, pos);
    }
    while pos < state.input.buf.len() {
        let Some(ch) = char_at(&state.input.buf, pos) else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        pos = next_char_boundary(&state.input.buf, pos);
    }
    state.input.cursor = pos;
}

fn cursor_backspace(state: &mut TuiState) {
    state.input.clamp_cursor();
    let previous = prev_char_boundary(&state.input.buf, state.input.cursor);
    state
        .input
        .buf
        .replace_range(previous..state.input.cursor, "");
    state.input.cursor = previous;
}

fn cursor_delete_before(state: &mut TuiState) {
    state.input.clamp_cursor();
    state.input.buf.replace_range(..state.input.cursor, "");
    state.input.cursor = 0;
}

fn cursor_delete_after(state: &mut TuiState) {
    state.input.clamp_cursor();
    state.input.buf.replace_range(state.input.cursor.., "");
}

fn cursor_delete_char_after(state: &mut TuiState) {
    state.input.clamp_cursor();
    if state.input.cursor < state.input.buf.len() {
        let next = next_char_boundary(&state.input.buf, state.input.cursor);
        state.input.buf.replace_range(state.input.cursor..next, "");
    }
}

fn cursor_delete_word(state: &mut TuiState) {
    state.input.clamp_cursor();
    let end = state.input.cursor;
    let mut start = end;

    while start > 0 {
        let prev = prev_char_boundary(&state.input.buf, start);
        let Some(ch) = char_at(&state.input.buf, prev) else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        start = prev;
    }

    while start > 0 {
        let prev = prev_char_boundary(&state.input.buf, start);
        let Some(ch) = char_at(&state.input.buf, prev) else {
            break;
        };
        if ch.is_whitespace() {
            break;
        }
        start = prev;
    }

    state.input.buf.replace_range(start..end, "");
    state.input.cursor = start;
}

fn char_at(s: &str, pos: usize) -> Option<char> {
    s.get(pos..)?.chars().next()
}

fn prev_char_boundary(s: &str, pos: usize) -> usize {
    super::editor::previous(s, pos)
}
fn next_char_boundary(s: &str, pos: usize) -> usize {
    super::editor::next(s, pos)
}

fn handle_enter(
    state: &mut TuiState,
    orch_tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
) -> bool {
    state.input.clamp_cursor();
    if state.edit_input.is_some() {
        if let Err(error) = state.submit_human_input(orch_tx) {
            state.input_rejected(error.to_string());
        }
        return false;
    }
    let parsed = parse_slash_command(&state.input.buf);
    if let Ok(Some(
        command @ (SlashCommand::Help
        | SlashCommand::Status
        | SlashCommand::Inputs
        | SlashCommand::Details
        | SlashCommand::Latest),
    )) = &parsed
    {
        match command {
            SlashCommand::Help => state.add_help(),
            SlashCommand::Status => {
                state.view = View::Panel {
                    panel: PanelKind::Status,
                    scroll: 0,
                    selected: 0,
                }
            }
            SlashCommand::Inputs => {
                state.refresh_inputs();
                state.view = View::Panel {
                    panel: PanelKind::Inputs,
                    scroll: 0,
                    selected: 0,
                };
            }
            SlashCommand::Details => {
                state.view = View::Panel {
                    panel: PanelKind::Details,
                    scroll: 0,
                    selected: 0,
                }
            }
            SlashCommand::Latest => {
                state.viewport.auto_scroll = true;
                state.viewport.anchor = None;
            }
            _ => unreachable!(),
        }
        state.input.buf.clear();
        state.input.cursor = 0;
        return false;
    }
    if state.runtime.is_some() {
        if matches!(parsed, Ok(None)) {
            if let Err(error) = state.submit_human_input(orch_tx) {
                // Admission failures must not finalize the current stream or
                // erase the draft; notices wait for its accepted boundary.
                state.input_rejected(format!("Input rejected: {error}"));
            }
            return false;
        }
        if let Ok(Some(
            command @ (SlashCommand::Inputs | SlashCommand::Resume(_) | SlashCommand::Withdraw(_)),
        )) = &parsed
        {
            if let Err(error) = state.inbox_command(command, orch_tx) {
                state.input_rejected(error.to_string());
            }
            return false;
        }
        if state.active_turn_id.is_some()
            && matches!(
                parsed,
                Ok(Some(
                    SlashCommand::Compact
                        | SlashCommand::Flash
                        | SlashCommand::Pro
                        | SlashCommand::Model(_)
                ))
            )
        {
            state.input_rejected("Control command unavailable during a turn; draft kept.".into());
            return false;
        }
    }
    let typed = std::mem::take(&mut state.input.buf);
    let images = std::mem::take(&mut state.input.pending_images);
    state.input.cursor = 0;
    if typed.is_empty() && images.is_empty() {
        return false;
    }
    if !typed.is_empty() {
        state.input.history.push(typed.clone());
    }
    state.input.history_idx = None;
    // 提交新输入前先封口上一轮未结束的流式内容，保证用户输入始终显示在
    // 已展示内容之后，避免被后续到达的 finalize 插入到错误位置。
    state.finalize_stream();
    state.push_line(TranscriptItem::new(
        format!("> {}", display_user_input(&typed, &images)),
        TranscriptKind::Info,
    ));
    match parse_slash_command(&typed) {
        Ok(Some(command)) => {
            if !images.is_empty() {
                // Slash commands are local UI/runtime actions, not model
                // turns: keep the images queued for the next real message.
                state.input.pending_images = images;
                state.push_line(TranscriptItem::new(
                    "Queued image(s) were not attached to a slash command; they stay queued for the next message."
                        .into(),
                    TranscriptKind::Info,
                ));
            }
            match command {
                SlashCommand::Flash => {
                    let _ = orch_tx.send(RuntimeCmd::SetModel("flash".into()));
                }
                SlashCommand::Pro => {
                    let _ = orch_tx.send(RuntimeCmd::SetModel("pro".into()));
                }
                SlashCommand::Model(model) => {
                    let _ = orch_tx.send(RuntimeCmd::SetModel(model));
                }
                SlashCommand::Compact => {
                    if orch_tx.send(RuntimeCmd::Compact).is_ok() {
                        state.arm_task_notification();
                        state.work_state = WorkState::Compacting;
                    } else {
                        state.push_line(TranscriptItem::new(
                            "Failed to send compact command.".into(),
                            TranscriptKind::Error,
                        ));
                    }
                }
                SlashCommand::Help => state.add_help(),
                SlashCommand::Skills => state.show_skills(),
                SlashCommand::Plan => state.view = View::Plan { scroll: 0 },
                SlashCommand::Todos => state.view = View::Todos { scroll: 0 },
                SlashCommand::Status
                | SlashCommand::Latest
                | SlashCommand::Details
                | SlashCommand::Inputs
                | SlashCommand::Resume(_)
                | SlashCommand::Withdraw(_) => {
                    state.push_line(TranscriptItem::new(
                        "TUI runtime unavailable.".into(),
                        TranscriptKind::Error,
                    ));
                }
                SlashCommand::SubAgent(session_id) => state.open_sub_agent(&session_id),
                SlashCommand::Artifact(id) => state.open_artifact(&id),
                SlashCommand::Quit => {
                    state.quit = true;
                    return true;
                }
            }
        }
        Ok(None) => {
            let input = submitted_user_input(&typed, &images);
            if orch_tx.send(RuntimeCmd::Run { input, done: None }).is_ok() {
                state.arm_task_notification();
                state.work_state = WorkState::WaitingModel;
            } else {
                // The runtime channel is closed: put the text and the queued
                // images back so a failed send never silently discards them.
                state.input.buf = typed;
                state.input.cursor = state.input.buf.len();
                state.input.pending_images = images;
                state.push_line(TranscriptItem::new(
                    "Failed to send user input.".into(),
                    TranscriptKind::Error,
                ));
            }
        }
        Err(_) => {
            if !images.is_empty() {
                state.input.pending_images = images;
            }
            state.push_line(TranscriptItem::new(
                "Unknown command. Prefix with a space to send it as text.".into(),
                TranscriptKind::Info,
            ));
        }
    }
    state.viewport.auto_scroll = true;
    false
}

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
    let old_cursor = state.input.cursor;
    let old_text = state.input.buf.clone();
    let old_images = state.input.pending_images.clone();
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
            state.input.clamp_cursor();
            let to_insert = normalize_tui_input(&content);
            if !to_insert.is_empty() {
                state.input.buf.insert_str(state.input.cursor, &to_insert);
                state.input.cursor += to_insert.len();
            }
            refresh_file_picker(state);
        }
        _ => {}
    }
    if old_text != state.input.buf || old_images != state.input.pending_images {
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
        Some((_, ClickAction::OpenArtifact { id })) => state.open_artifact(&id),
        Some((_, ClickAction::OpenSubAgent { session_id })) => {
            state.open_sub_agent(&session_id);
        }
        None => {}
    }
}
