use super::*;

#[test]
fn tab_opens_file_picker_overlay() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "src/tui/in".into();
    state.input.cursor = state.input.buf.len();

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert!(matches!(state.overlay, Some(ActiveOverlay::FilePicker(_))));
}

#[test]
fn file_picker_accept_replaces_current_path_token() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "Read @src/tui/in".into();
    state.input.cursor = state.input.buf.len();
    state.overlay = Some(ActiveOverlay::FilePicker(
        FilePickerState::open_with_candidates(
            &state.input.buf,
            state.input.cursor,
            vec![
                FilePickCandidate {
                    path: "src/tui/input.rs".into(),
                    is_dir: false,
                },
                FilePickCandidate {
                    path: "src/tui/render/input.rs".into(),
                    is_dir: false,
                },
            ],
        ),
    ));

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "Read @src/tui/input.rs");
    assert_eq!(state.input.cursor, state.input.buf.len());
    assert!(state.overlay.is_none());
}

#[test]
fn file_picker_enter_accepts_directory_and_closes_overlay() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "Read do".into();
    state.input.cursor = state.input.buf.len();
    state.overlay = Some(ActiveOverlay::FilePicker(
        FilePickerState::open_with_candidates(
            &state.input.buf,
            state.input.cursor,
            vec![FilePickCandidate {
                path: "docs".into(),
                is_dir: true,
            }],
        ),
    ));

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "Read docs/");
    assert_eq!(state.input.cursor, state.input.buf.len());
    assert!(state.overlay.is_none());
}

#[test]
fn file_picker_tab_enters_directory_and_keeps_overlay_open() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "Read do".into();
    state.input.cursor = state.input.buf.len();
    state.overlay = Some(ActiveOverlay::FilePicker(
        FilePickerState::open_with_candidates(
            &state.input.buf,
            state.input.cursor,
            vec![FilePickCandidate {
                path: "docs".into(),
                is_dir: true,
            }],
        ),
    ));

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "Read docs/");
    assert_eq!(state.input.cursor, state.input.buf.len());
    assert!(matches!(state.overlay, Some(ActiveOverlay::FilePicker(_))));
}

#[test]
fn file_picker_accept_replaces_entire_current_path_token() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "Read src/tui/in_suffix".into();
    state.input.cursor = "Read src/tui/in".len();
    state.overlay = Some(ActiveOverlay::FilePicker(
        FilePickerState::open_with_candidates(
            &state.input.buf,
            state.input.cursor,
            vec![FilePickCandidate {
                path: "src/tui/input.rs".into(),
                is_dir: false,
            }],
        ),
    ));

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "Read src/tui/input.rs");
    assert_eq!(state.input.cursor, state.input.buf.len());
}

#[test]
fn file_picker_accept_preserves_read_selector_suffix() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "Read src/tui/in:10-20".into();
    state.input.cursor = "Read src/tui/in".len();
    state.overlay = Some(ActiveOverlay::FilePicker(
        FilePickerState::open_with_candidates(
            &state.input.buf,
            state.input.cursor,
            vec![FilePickCandidate {
                path: "src/tui/input.rs".into(),
                is_dir: false,
            }],
        ),
    ));

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "Read src/tui/input.rs:10-20");
    assert_eq!(state.input.cursor, "Read src/tui/input.rs".len());
}

#[test]
fn file_picker_accept_preserves_selector_when_cursor_is_after_selector() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "Read src/tui/in:raw".into();
    state.input.cursor = state.input.buf.len();
    state.overlay = Some(ActiveOverlay::FilePicker(
        FilePickerState::open_with_candidates(
            &state.input.buf,
            state.input.cursor,
            vec![FilePickCandidate {
                path: "src/tui/input.rs".into(),
                is_dir: false,
            }],
        ),
    ));

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "Read src/tui/input.rs:raw");
    assert_eq!(state.input.cursor, "Read src/tui/input.rs".len());
}

#[test]
fn file_picker_accept_preserves_raw_range_selector() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "Read src/tui/in:raw:10-20".into();
    state.input.cursor = "Read src/tui/in".len();
    state.overlay = Some(ActiveOverlay::FilePicker(
        FilePickerState::open_with_candidates(
            &state.input.buf,
            state.input.cursor,
            vec![FilePickCandidate {
                path: "src/tui/input.rs".into(),
                is_dir: false,
            }],
        ),
    ));

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "Read src/tui/input.rs:raw:10-20");
    assert_eq!(state.input.cursor, "Read src/tui/input.rs".len());
}

#[test]
fn file_picker_accept_does_not_treat_colon_filename_as_selector() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "Read notes/report:2026.md".into();
    state.input.cursor = "Read notes/report".len();
    state.overlay = Some(ActiveOverlay::FilePicker(
        FilePickerState::open_with_candidates(
            &state.input.buf,
            state.input.cursor,
            vec![FilePickCandidate {
                path: "notes/report-final.md".into(),
                is_dir: false,
            }],
        ),
    ));

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "Read notes/report-final.md");
    assert_eq!(state.input.cursor, state.input.buf.len());
}

#[test]
fn file_picker_accept_does_not_treat_alphanumeric_suffix_as_selector() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "Read src/tui/in:10abc".into();
    state.input.cursor = "Read src/tui/in".len();
    state.overlay = Some(ActiveOverlay::FilePicker(
        FilePickerState::open_with_candidates(
            &state.input.buf,
            state.input.cursor,
            vec![FilePickCandidate {
                path: "src/tui/input.rs".into(),
                is_dir: false,
            }],
        ),
    ));

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "Read src/tui/input.rs");
    assert_eq!(state.input.cursor, state.input.buf.len());
}

#[test]
fn file_picker_accept_does_not_treat_zero_line_as_selector() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "Read src/tui/in:0".into();
    state.input.cursor = "Read src/tui/in".len();
    state.overlay = Some(ActiveOverlay::FilePicker(
        FilePickerState::open_with_candidates(
            &state.input.buf,
            state.input.cursor,
            vec![FilePickCandidate {
                path: "src/tui/input.rs".into(),
                is_dir: false,
            }],
        ),
    ));

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "Read src/tui/input.rs");
    assert_eq!(state.input.cursor, state.input.buf.len());
}

#[test]
fn file_picker_parent_prefix_scans_parent_directory() {
    let root = unique_test_dir("parent-scan");
    let cwd = root.join("workspace");
    let shared = root.join("shared");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("lib.rs"), "fn main() {}\n").unwrap();
    let policy = FilePickerPolicy::restricted_for_tests(cwd, vec![root.clone()], 3);

    let picker = FilePickerState::open("../shared/l", "../shared/l".len(), &policy);

    assert!(
        picker
            .items
            .iter()
            .any(|item| item.path == "../shared/lib.rs")
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn file_picker_parent_prefix_respects_depth_limit() {
    let root = unique_test_dir("parent-depth");
    let cwd = root.join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let policy = FilePickerPolicy::restricted_for_tests(cwd, vec![root.clone()], 1);

    let picker = FilePickerState::open("../../", "../../".len(), &policy);

    assert!(picker.items.is_empty());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn file_picker_parent_prefix_respects_restricted_roots() {
    let root = unique_test_dir("parent-restricted");
    let cwd = root.join("workspace");
    let shared = root.join("shared");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("lib.rs"), "fn main() {}\n").unwrap();
    let policy = FilePickerPolicy::restricted_for_tests(cwd.clone(), vec![cwd], 3);

    let picker = FilePickerState::open("../shared/l", "../shared/l".len(), &policy);

    assert!(picker.items.is_empty());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn file_picker_refresh_rescans_when_parent_root_changes() {
    let root = unique_test_dir("parent-refresh");
    let cwd = root.join("workspace");
    let shared = root.join("shared");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("lib.rs"), "fn main() {}\n").unwrap();
    let policy = FilePickerPolicy::restricted_for_tests(cwd, vec![root.clone()], 3);
    let mut picker = FilePickerState::open("", 0, &policy);

    picker.refresh_with_policy("../shared/l", "../shared/l".len(), &policy);

    assert!(
        picker
            .items
            .iter()
            .any(|item| item.path == "../shared/lib.rs")
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn file_picker_parent_prefix_allows_sandbox_sibling_root() {
    let root = unique_test_dir("parent-allowed-sibling");
    let cwd = root.join("workspace");
    let shared = root.join("shared");
    let other = root.join("other");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(shared.join("lib.rs"), "fn main() {}\n").unwrap();
    std::fs::write(other.join("secret.rs"), "fn main() {}\n").unwrap();
    let policy = FilePickerPolicy::restricted_for_tests(cwd, vec![shared.clone()], 3);

    let picker = FilePickerState::open("../", "../".len(), &policy);

    assert!(picker.items.iter().any(|item| item.path == "../shared/"));
    assert!(!picker.items.iter().any(|item| item.path == "../other/"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn ctrl_v_and_newline_keys_are_ignored_while_the_file_picker_is_open() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel();
    let dir = unique_test_dir("clipboard-overlay");
    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            image_input: Some(mink::runtime::OpenAiChatImageUrlLimits::default()),
            attachments_dir: dir.clone(),
            ..Default::default()
        },
        async_load: state::AsyncLoadState {
            ui_tx: Some(ui_tx),
            clipboard_reader: Some(fake_clipboard_reader()),
            ..Default::default()
        },
        overlay: Some(ActiveOverlay::FilePicker(
            FilePickerState::open_with_candidates("", 0, Vec::new()),
        )),
        ..Default::default()
    };

    for (code, modifiers) in [
        (KeyCode::Char('v'), KeyModifiers::CONTROL),
        (KeyCode::Enter, KeyModifiers::SHIFT),
        (KeyCode::Enter, KeyModifiers::ALT),
        (KeyCode::Char('j'), KeyModifiers::CONTROL),
    ] {
        assert!(!handle_event(
            Event::Key(KeyEvent::new(code, modifiers)),
            &mut state,
            &tx,
        ));
    }

    assert!(
        state.async_load.clipboard_started.is_none(),
        "Ctrl+V must not start a clipboard read behind the overlay"
    );
    assert!(ui_rx.try_recv().is_err());
    assert!(state.input.buf.is_empty(), "newline keys must be inert");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn chips_are_hidden_while_the_file_picker_overlay_is_open() {
    let mut state = TuiState::default();
    state.input.pending_images = vec![pending_image("a")];
    state.overlay = Some(ActiveOverlay::FilePicker(
        FilePickerState::open_with_candidates("", 0, Vec::new()),
    ));
    let backend = TestBackend::new(100, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|f| render(f, &mut state, TuiMode::Full))
        .unwrap();

    let buf = terminal.backend().buffer();
    let rows: Vec<String> = (0..40)
        .map(|y| {
            (0..100)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect()
        })
        .collect();
    assert!(
        rows.iter().all(|row| !row.contains("[image #1")),
        "chips must not be drawn under the picker overlay: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("files:")),
        "the picker must still be rendered"
    );
}
