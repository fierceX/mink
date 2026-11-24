use super::editing::{
    cursor_backspace, cursor_delete_after, cursor_delete_before, cursor_delete_char_after,
    cursor_delete_word, cursor_left, cursor_right, cursor_vertical, cursor_word_left,
    cursor_word_right, insert_char, is_text_modifier, request_clipboard_image,
};
use super::picker::{accept_file_picker, open_file_picker, refresh_file_picker};
use super::submit::handle_enter;
use super::*;

pub(super) fn handle_key(
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
    if matches!(state.view, View::Main) {
        // Keyboard parity with the goal card's click actions: Ctrl+G opens the
        // read-only goal detail (/goals), Ctrl+O folds the newest boundary card.
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('g') {
            state.view = View::Goal { scroll: 0 };
            return false;
        }
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('o') {
            if let Some(index) = state
                .lines
                .iter()
                .rposition(|item| item.kind == TranscriptKind::Goal)
                && index >= state.inline.committed
            {
                state.lines[index].toggle_collapsed();
                state.invalidate_item(index);
            }
            return false;
        }
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
        crate::tui::panels::detail_actions(state)
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
        ClickAction::OpenGoal => state.view = View::Goal { scroll: 0 },
        ClickAction::OpenArtifact { id } => state.open_artifact(&id),
        ClickAction::OpenSubAgent { session_id } => state.open_sub_agent(&session_id),
    }
}

pub(super) fn view_scroll_mut(view: &mut View) -> Option<&mut usize> {
    match view {
        View::SubAgentDetail { scroll, .. }
        | View::Plan { scroll }
        | View::Goal { scroll }
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
