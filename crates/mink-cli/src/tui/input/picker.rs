use super::*;

pub(super) fn accept_file_picker(state: &mut TuiState, keep_open_for_dirs: bool) {
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

pub(super) fn open_file_picker(state: &mut TuiState) {
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
        crate::tui::file_picker::path_query_at_cursor(&state.input.buf, state.input.cursor);
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
        .get_or_insert_with(|| crate::tui::file_picker::start_worker(tx));
    let _ = worker.send(crate::tui::file_picker::PickerRequest {
        generation,
        input,
        cursor,
        policy,
    });
}
pub(super) fn refresh_file_picker(state: &mut TuiState) {
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
