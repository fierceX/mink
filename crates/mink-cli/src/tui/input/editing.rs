use super::*;

pub(super) fn is_text_modifier(modifiers: KeyModifiers) -> bool {
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

    if state.active_turn_id.is_some()
        || state.work_state.is_working()
        || state.admission_pending
        || state.runtime.as_ref().is_some_and(|r| {
            r.handle
                .goal_snapshot()
                .is_some_and(|g| g.status == crate::runtime::GoalStatus::Active)
        })
    {
        let _ = orch_tx.send(RuntimeCmd::Interrupt);
        state.stopping = state.active_turn_id.is_some() || state.admission_pending;
        state.admission_cancelled |= state.admission_pending;
        state.last_interrupt = Some(now);
        return false;
    }

    state.quit = true;
    true
}

pub(super) fn insert_char(state: &mut TuiState, c: char) {
    state.input.clamp_cursor();
    state.input.buf.insert(state.input.cursor, c);
    state.input.cursor += c.len_utf8();
}

/// Ctrl+V: read the system clipboard on a worker thread (the platform reader
/// spawns a subprocess, so it must not block the event loop) and stage the
/// image under the session attachment directory.
pub(super) fn request_clipboard_image(state: &mut TuiState) {
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

pub(super) fn cursor_left(state: &mut TuiState) {
    state.input.cursor = prev_char_boundary(&state.input.buf, state.input.cursor);
}
pub(super) fn cursor_right(state: &mut TuiState) {
    state.input.cursor = next_char_boundary(&state.input.buf, state.input.cursor);
}
pub(super) fn cursor_vertical(state: &mut TuiState, delta: isize) {
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

pub(super) fn cursor_word_left(state: &mut TuiState) {
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

pub(super) fn cursor_word_right(state: &mut TuiState) {
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

pub(super) fn cursor_backspace(state: &mut TuiState) {
    state.input.clamp_cursor();
    let previous = prev_char_boundary(&state.input.buf, state.input.cursor);
    state
        .input
        .buf
        .replace_range(previous..state.input.cursor, "");
    state.input.cursor = previous;
}

pub(super) fn cursor_delete_before(state: &mut TuiState) {
    state.input.clamp_cursor();
    state.input.buf.replace_range(..state.input.cursor, "");
    state.input.cursor = 0;
}

pub(super) fn cursor_delete_after(state: &mut TuiState) {
    state.input.clamp_cursor();
    state.input.buf.replace_range(state.input.cursor.., "");
}

pub(super) fn cursor_delete_char_after(state: &mut TuiState) {
    state.input.clamp_cursor();
    if state.input.cursor < state.input.buf.len() {
        let next = next_char_boundary(&state.input.buf, state.input.cursor);
        state.input.buf.replace_range(state.input.cursor..next, "");
    }
}

pub(super) fn cursor_delete_word(state: &mut TuiState) {
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
    crate::tui::editor::previous(s, pos)
}
fn next_char_boundary(s: &str, pos: usize) -> usize {
    crate::tui::editor::next(s, pos)
}
