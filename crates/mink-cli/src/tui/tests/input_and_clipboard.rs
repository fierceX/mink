use super::*;

#[test]
fn input_scroll_keeps_cursor_visible() {
    assert_eq!(render::clamp_input_scroll(8, 7, 5, 0), 3);
    assert_eq!(render::clamp_input_scroll(8, 1, 5, 3), 1);
    assert_eq!(render::clamp_input_scroll(8, 4, 5, 2), 2);
    assert_eq!(render::clamp_input_scroll(3, 2, 5, 9), 0);
}

#[test]
fn ctrl_c_interrupts_working_state_then_exits_on_second_press() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState {
        stream: state::StreamState {
            streaming: true,
            work_state: WorkState::StreamingText,
            ..Default::default()
        },
        ..Default::default()
    };

    assert!(!handle_ctrl_c(&mut state, &tx));
    assert!(matches!(
        rx.try_recv(),
        Ok(crate::cli::RuntimeCmd::Interrupt)
    ));
    assert!(!state.quit);

    assert!(handle_ctrl_c(&mut state, &tx));
    assert!(state.quit);
}

#[test]
fn ctrl_shift_c_interrupts_working_state() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState {
        stream: state::StreamState {
            work_state: WorkState::WaitingModel,
            ..Default::default()
        },
        ..Default::default()
    };

    assert!(!handle_event(
        Event::Key(KeyEvent::new(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )),
        &mut state,
        &tx,
    ));
    assert!(!state.quit);
}

#[test]
fn ctrl_c_exits_immediately_when_idle() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();

    assert!(handle_ctrl_c(&mut state, &tx));
    assert!(state.quit);
}

#[test]
fn input_supports_readline_shortcuts_and_multiline_insert() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "hello world".into();
    state.input.cursor = state.input.buf.len();

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.cursor, 0);

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.cursor, state.input.buf.len());

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.cursor, "hello ".len());

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.buf, "hello ");

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.buf, "");
    assert_eq!(state.input.cursor, 0);

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.buf, "\n");
    assert_eq!(state.input.cursor, 1);

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.buf, "\nA");
    assert_eq!(state.input.cursor, 2);
}

#[test]
fn paste_normalizes_control_text_before_inserting() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();

    assert!(!handle_event(
        Event::Paste("a\r\n\tb\x07c".into()),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "a\n    bc");
    assert_eq!(state.input.cursor, state.input.buf.len());
}

#[test]
fn input_history_can_walk_multiple_entries_and_restore_draft() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.history = vec!["first".into(), "second".into()];

    for (key, expected) in [
        (KeyCode::Up, "second"),
        (KeyCode::Up, "first"),
        (KeyCode::Down, "second"),
        (KeyCode::Down, ""),
    ] {
        assert!(!handle_event(
            Event::Key(KeyEvent::new(key, KeyModifiers::NONE)),
            &mut state,
            &tx,
        ));
        assert_eq!(state.input.buf, expected);
        assert_eq!(state.input.cursor, state.input.buf.len());
    }
}

#[test]
fn input_ctrl_d_deletes_next_char_or_exits_when_empty() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "a中🙂b".into();
    state.input.cursor = "a".len();

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.buf, "a🙂b");
    assert_eq!(state.input.cursor, "a".len());

    state.input.buf.clear();
    state.input.cursor = 0;
    assert!(handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        &mut state,
        &tx,
    ));
    assert!(state.quit);
}

#[test]
fn input_clamps_invalid_cursor_before_editing() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "a中b".into();
    state.input.cursor = 2;

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.buf, "aX中b");
    assert_eq!(state.input.cursor, "aX".len());

    state.input.cursor = usize::MAX;
    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.buf, "aX中b");
    assert_eq!(state.input.cursor, state.input.buf.len());
}

#[test]
fn input_alt_backspace_deletes_previous_utf8_word() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "hello 世界".into();
    state.input.cursor = state.input.buf.len();

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.buf, "hello ");
    assert_eq!(state.input.cursor, "hello ".len());

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.buf, "");
    assert_eq!(state.input.cursor, 0);
}

#[test]
fn small_viewport_input_box_keeps_cursor_inside_and_border_intact() {
    // Regression: on an 8-row viewport (inline minimum) a 5-line input
    // must not squeeze the input block — the last line was clipped and
    // the cursor landed on the bottom border.
    let mut state = TuiState::default();
    state.input.buf = "line1\nline2\nline3\nline4\nline5".into();
    state.input.cursor = state.input.buf.len();

    use ratatui::backend::Backend;
    let backend = TestBackend::new(80, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| render(f, &mut state, TuiMode::Full))
        .unwrap();

    let buf = terminal.backend().buffer();
    // Layout for 8 rows: content row0, input block rows1..6, status row7.
    assert_eq!(buf.cell((0, 1)).unwrap().symbol(), "┌");
    assert_eq!(buf.cell((0, 6)).unwrap().symbol(), "└");
    // The last input line must be visible inside the box (row5), not
    // clipped or overwriting the bottom border (row6).
    let last_row: String = (1..6)
        .map(|x| buf.cell((x, 5)).unwrap().symbol().to_string())
        .collect();
    assert_eq!(last_row, "line5");
    assert!(matches!(buf.cell((1, 6)).unwrap().symbol(), "─" | "━"));
    let cur = terminal.backend_mut().get_cursor_position().unwrap();
    // Cursor must sit on the last inner row (5), never on the border (6).
    assert_eq!(cur.y, 5);
}

#[test]
fn new_user_input_is_appended_after_the_open_stream() {
    // Regression: submitting a new message while the previous turn's
    // stream is still open must not let the late finalize (Stop) insert
    // the old answer after the new user input.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();

    state.apply(&TuiSignal::Text("previous answer".into()));
    assert!(state.stream.streaming);

    state.input.buf = "next question".into();
    state.input.cursor = state.input.buf.len();
    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert!(!state.stream.streaming);
    let texts: Vec<&str> = state
        .transcript
        .lines
        .iter()
        .map(|item| item.text.as_str())
        .collect();
    assert_eq!(
        texts,
        vec!["previous answer", "> next question"],
        "the user input echo must follow the already displayed stream content"
    );

    // A late Stop must not move the previous answer behind the new input.
    state.apply(&TuiSignal::Stop);
    let texts: Vec<&str> = state
        .transcript
        .lines
        .iter()
        .map(|item| item.text.as_str())
        .collect();
    assert_eq!(texts, vec!["previous answer", "> next question"]);
    assert_eq!(state.stream.work_state, WorkState::Idle);
}

#[test]
fn ctrl_v_stages_clipboard_image_and_queues_it() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel();
    let dir = unique_test_dir("clipboard-stage");
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
        ..Default::default()
    };

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
        &mut state,
        &tx,
    ));
    assert!(state.async_load.clipboard_started.is_some());

    let event = ui_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("clipboard worker must report back");
    state.apply_ui_event(event);

    assert!(state.async_load.clipboard_started.is_none());
    assert_eq!(state.input.pending_images.len(), 1);
    let image = &state.input.pending_images[0];
    assert!(image.path.starts_with(&dir), "{:?}", image.path);
    assert_eq!((image.width, image.height, image.bytes), (12, 8, 8));
    assert_eq!(std::fs::read(&image.path).unwrap(), b"fake-png");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn super_v_also_requests_clipboard_image() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel();
    let dir = unique_test_dir("clipboard-super");
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
        ..Default::default()
    };

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::SUPER)),
        &mut state,
        &tx,
    ));

    let event = ui_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("Super+V must trigger the clipboard worker");
    state.apply_ui_event(event);

    assert_eq!(state.input.pending_images.len(), 1);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn stale_clipboard_read_allows_a_retry() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel();
    let dir = unique_test_dir("clipboard-stale");
    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            image_input: Some(mink::runtime::OpenAiChatImageUrlLimits::default()),
            attachments_dir: dir.clone(),
            ..Default::default()
        },
        async_load: state::AsyncLoadState {
            ui_tx: Some(ui_tx),
            clipboard_reader: Some(fake_clipboard_reader()),
            // A read that never reported back (hung osascript) must not disable
            // paste for the rest of the session.
            clipboard_started: Some(std::time::Instant::now() - std::time::Duration::from_secs(60)),
            ..Default::default()
        },
        ..Default::default()
    };

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
        &mut state,
        &tx,
    ));

    let event = ui_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("a stale in-flight read must not block a retry");
    state.apply_ui_event(event);
    assert_eq!(state.input.pending_images.len(), 1);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn ctrl_v_without_image_capability_reports_and_queues_nothing() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = std::sync::mpsc::channel();
    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            image_input: None,
            attachments_dir: unique_test_dir("clipboard-disabled"),
            ..Default::default()
        },
        async_load: state::AsyncLoadState {
            ui_tx: Some(ui_tx),
            clipboard_reader: Some(fake_clipboard_reader()),
            ..Default::default()
        },
        ..Default::default()
    };

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
        &mut state,
        &tx,
    ));

    assert!(state.async_load.clipboard_started.is_none());
    assert!(state.input.pending_images.is_empty());
    let last = state.transcript.lines.last().unwrap();
    assert_eq!(last.kind, TranscriptKind::Info);
    assert!(
        last.text.contains("no image input capability"),
        "{}",
        last.text
    );
}

#[test]
fn backspace_on_empty_input_drops_last_queued_image() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.pending_images = vec![pending_image("a"), pending_image("b")];

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.input.pending_images.len(), 1);
    assert_eq!(
        state.input.pending_images[0].path,
        PathBuf::from("/tmp/a.png")
    );

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));
    assert!(state.input.pending_images.is_empty());
}

#[test]
fn enter_expands_image_markers_and_keeps_history_clean() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "look at this".into();
    state.input.cursor = state.input.buf.len();
    state.input.pending_images = vec![pending_image("a")];

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    let command = rx.try_recv().expect("run command");
    let crate::cli::RuntimeCmd::Run { input, .. } = command else {
        panic!("expected a Run command");
    };
    assert_eq!(
        input,
        "look at this\n\n[Attached image: \"/tmp/a.png\" - Read it to view.]"
    );
    assert_eq!(state.input.history, vec!["look at this".to_string()]);
    assert!(state.input.pending_images.is_empty());
    assert_eq!(
        state.transcript.lines.last().unwrap().text,
        "> [image #1] look at this"
    );
}

#[test]
fn enter_with_only_images_submits_marker_text() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.pending_images = vec![pending_image("a")];

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    let command = rx.try_recv().expect("run command");
    let crate::cli::RuntimeCmd::Run { input, .. } = command else {
        panic!("expected a Run command");
    };
    assert_eq!(input, "[Attached image: \"/tmp/a.png\" - Read it to view.]");
    assert!(state.input.history.is_empty());
    assert_eq!(state.transcript.lines.last().unwrap().text, "> [image #1]");
}

#[test]
fn failed_send_keeps_text_and_queued_images() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<crate::cli::RuntimeCmd>();
    drop(rx);
    let mut state = TuiState::default();
    state.input.buf = "look at this".into();
    state.input.cursor = state.input.buf.len();
    state.input.pending_images = vec![pending_image("a")];

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert_eq!(state.input.buf, "look at this");
    assert_eq!(state.input.cursor, "look at this".len());
    assert_eq!(state.input.pending_images.len(), 1);
    assert!(
        state
            .transcript
            .lines
            .iter()
            .any(|line| line.kind == TranscriptKind::Error
                && line.text.contains("Failed to send user input")),
        "a failed send must be reported"
    );
}

#[test]
fn unrepresentable_attachment_path_fails_closed() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel();
    let root = unique_test_dir("clipboard-quote");
    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            image_input: Some(mink::runtime::OpenAiChatImageUrlLimits::default()),
            attachments_dir: root.join("bad\"dir"),
            ..Default::default()
        },
        async_load: state::AsyncLoadState {
            ui_tx: Some(ui_tx),
            clipboard_reader: Some(fake_clipboard_reader()),
            ..Default::default()
        },
        ..Default::default()
    };

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL)),
        &mut state,
        &tx,
    ));
    let event = ui_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("clipboard worker must report back");
    state.apply_ui_event(event);

    assert!(state.input.pending_images.is_empty());
    assert!(
        state
            .transcript
            .lines
            .iter()
            .any(|line| line.kind == TranscriptKind::Error
                && line.text.contains("cannot be represented")),
        "unrepresentable paths must fail closed"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn chip_lines_truncate_to_requested_rows() {
    let images = vec![pending_image("a"), pending_image("b"), pending_image("c")];

    let lines = render::chip_lines(&images, 12, 2);

    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("[image #1"), "{}", lines[0]);
    assert!(lines[1].ends_with('…'), "{}", lines[1]);
    assert!(
        lines
            .iter()
            .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 12),
        "chips must stay within the requested width: {lines:?}"
    );
    assert!(render::chip_lines(&images, 80, 0).is_empty());
    assert!(render::chip_lines(&[], 80, 2).is_empty());
}

#[test]
fn load_image_limits_reads_frozen_snapshot() {
    let dir = unique_test_dir("capabilities");
    std::fs::create_dir_all(&dir).unwrap();
    let info = session_info(&dir);

    assert!(
        load_image_limits(&info).is_none(),
        "missing snapshot fails closed"
    );

    let supported = mink::runtime::ImageInputCapability::OpenAiChatImageUrl(Default::default());
    std::fs::write(
        dir.join("model-capabilities.json"),
        serde_json::json!({"image_input": supported}).to_string(),
    )
    .unwrap();
    assert!(load_image_limits(&info).is_some());

    let unsupported = mink::runtime::ImageInputCapability::Unsupported;
    std::fs::write(
        dir.join("model-capabilities.json"),
        serde_json::json!({"image_input": unsupported}).to_string(),
    )
    .unwrap();
    assert!(
        load_image_limits(&info).is_none(),
        "text-only snapshot fails closed"
    );

    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn render_draws_queued_image_chips_above_the_input() {
    let mut state = TuiState::default();
    state
        .transcript
        .lines
        .push(TranscriptItem::new("hello".into(), TranscriptKind::Text));
    state.input.pending_images = vec![pending_image("a")];
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|f| render(f, &mut state, TuiMode::Full))
        .unwrap();

    let buf = terminal.backend().buffer();
    let row_text = |y: u16| -> String {
        (0..80)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect()
    };
    let chip_row = (0..20)
        .find(|y| row_text(*y).contains("[image #1 12x8 3.4KB]"))
        .expect("chip row must be rendered");
    assert!(
        row_text(chip_row + 1).starts_with('┌'),
        "input box must stay directly below the chips"
    );
}

#[test]
fn format_bytes_keeps_one_decimal_only_when_needed() {
    let format = crate::tui::state::format_bytes;

    assert_eq!(format(512), "512B");
    assert_eq!(format(1024), "1KB");
    assert_eq!(format(1536), "1.5KB");
    assert_eq!(format(220 * 1024), "220KB");
    assert_eq!(format(1024 * 1024), "1MB");
    assert_eq!(format(1024 * 1024 * 3 / 2), "1.5MB");
}

#[test]
fn duplicate_paste_is_not_queued_twice() {
    let mut state = TuiState::default();

    state.apply_ui_event(crate::tui::state::TuiUiEvent::ImageCaptured(pending_image(
        "a",
    )));
    state.apply_ui_event(crate::tui::state::TuiUiEvent::ImageCaptured(pending_image(
        "a",
    )));

    assert_eq!(state.input.pending_images.len(), 1);
    let notice = state
        .transcript
        .lines
        .iter()
        .find(|line| line.text.contains("already queued"))
        .expect("duplicate paste must be reported");
    // The notice must not echo the absolute attachment path.
    assert!(!notice.text.contains("/tmp/"), "{}", notice.text);
}

#[test]
fn compact_user_input_replaces_only_marker_lines() {
    use crate::tui::state::compact_user_input_for_display as compact;

    assert_eq!(
        compact("[Attached image: \"/tmp/a.png\" - Read it to view.]"),
        "[image]"
    );
    assert_eq!(
        compact("hello\n[Attached image: \"/tmp/a.png\" - Read it to view.]"),
        "hello\n[image]"
    );
    assert_eq!(compact("plain text"), "plain text");
}

#[test]
fn replay_compacts_paste_markers_in_user_input() {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "mink_tui_paste_replay_{}_{}_{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let events = [
        serde_json::json!({
            "type": "user_input",
            "content": "[Attached image: \"/tmp/a.png\" - Read it to view.]"
        }),
        serde_json::json!({"type": "text", "content": "answer"}),
    ];
    let data = events
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, data).unwrap();

    let lines = load_session(&path);
    let _ = std::fs::remove_file(path);

    assert!(
        lines.iter().any(|line| line.text == "> [image]"),
        "replayed paste markers must not echo absolute paths: {:?}",
        lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn shift_alt_enter_and_ctrl_j_insert_newlines() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "ab".into();
    state.input.cursor = 1;

    for (code, modifiers) in [
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

    assert_eq!(state.input.buf, "a\n\n\nb");
    assert_eq!(state.input.cursor, "a\n\n\n".len());
}

#[test]
fn keyboard_enhancement_gate_respects_env_and_term() {
    assert!(keyboard_enhancement_allowed(None, Some("xterm-256color")));
    assert!(keyboard_enhancement_allowed(
        Some("on"),
        Some("xterm-256color")
    ));
    assert!(!keyboard_enhancement_allowed(
        Some("off"),
        Some("xterm-256color")
    ));
    assert!(!keyboard_enhancement_allowed(Some(" 0 "), None));
    assert!(!keyboard_enhancement_allowed(Some("false"), None));
    assert!(!keyboard_enhancement_allowed(Some("no"), None));
    // Case-insensitive, like every other MINK_* parser.
    assert!(!keyboard_enhancement_allowed(Some("OFF"), None));
    assert!(!keyboard_enhancement_allowed(Some(" False "), None));
    assert!(!keyboard_enhancement_allowed(Some("No"), None));
    assert!(!keyboard_enhancement_allowed(None, Some("dumb")));
    assert!(!keyboard_enhancement_allowed(None, Some("")));
    // TERM unset is not a reason to skip the probe: the probe itself decides.
    assert!(keyboard_enhancement_allowed(None, None));
}

#[test]
fn disable_keyboard_enhancement_pops_at_most_once() {
    KEYBOARD_ENHANCED.store(false, Ordering::SeqCst);
    // Never pushed: must not emit a stray pop sequence.
    disable_keyboard_enhancement();
    assert!(!KEYBOARD_ENHANCED.load(Ordering::SeqCst));

    KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
    disable_keyboard_enhancement();
    assert!(!KEYBOARD_ENHANCED.load(Ordering::SeqCst));
    // Idempotent: the restore guard and the panic hook may both call it.
    disable_keyboard_enhancement();
    assert!(!KEYBOARD_ENHANCED.load(Ordering::SeqCst));
}

#[test]
fn stale_background_queries_do_not_replace_current_resource_or_closed_picker() {
    let mut state = TuiState {
        async_load: state::AsyncLoadState {
            artifact_generation: 2,
            picker_generation: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    state.view = View::Artifact { scroll: 0 };
    state.apply_ui_event(state::TuiUiEvent::ArtifactReady {
        generation: 1,
        id: "old".into(),
        result: Ok(state::ArtifactDetail {
            id: "old".into(),
            content: "old".into(),
            truncated: false,
        }),
    });
    assert!(state.mirrors.artifact_detail.is_none());
    state.apply_ui_event(state::TuiUiEvent::FilePickerReady {
        generation: 2,
        picker: FilePickerState::default(),
    });
    assert!(state.overlay.is_none());
}

#[test]
fn late_failed_admission_keeps_new_draft_and_recovers_old_draft_separately() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    let mut draft = state.input.clone();
    draft.buf = "submitted".into();
    state.input.buf = "new draft".into();
    state.input.revision = 1;
    state.finish_admission(super::inbox::AdmissionResult {
        action: super::inbox::AdmissionAction::Submit,
        draft,
        result: Err("failed".into()),
        tx,
    });
    assert_eq!(state.input.buf, "new draft");
    assert_eq!(state.admission.failed_drafts[0].buf, "submitted");
}

#[test]
fn inbox_control_receipts_preserve_unsent_text_images_and_history() {
    use super::inbox::{AdmissionAction, AdmissionResult};
    let receipt = || {
        serde_json::from_value::<crate::runtime::InputReceipt>(serde_json::json!({
        "input_id":"queued", "revision":2, "turn_id":"turn", "status":"withdrawn", "guidance":false,
        "input":{"request_id":"queued", "text":"original task", "attachment_ids":[]},
        "original":{"request_id":"queued", "text":"original task", "attachment_ids":[]}
    })).unwrap()
    };
    for action in [AdmissionAction::Update, AdmissionAction::Resume] {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = TuiState::default();
        state.input.buf = "unsent draft".into();
        state.input.cursor = state.input.buf.len();
        state.input.pending_images.push(state::PendingImage {
            path: "image.png".into(),
            width: 1,
            height: 1,
            bytes: 1,
        });
        let draft = state.input.draft_snapshot();
        state.finish_admission(AdmissionResult {
            action,
            draft,
            result: Ok((receipt(), None)),
            tx,
        });
        assert_eq!(state.input.buf, "unsent draft");
        assert_eq!(state.input.pending_images.len(), 1);
        let expected = if matches!(action, AdmissionAction::Resume) {
            vec!["original task"]
        } else {
            vec![]
        };
        assert_eq!(state.input.history, expected);
    }
    for (action, text) in [
        (AdmissionAction::Update, "/withdraw queued"),
        (AdmissionAction::Resume, "/resume queued"),
        (AdmissionAction::Edit, "edited task"),
    ] {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = TuiState::default();
        state.input.buf = text.into();
        state.input.pending_images.push(state::PendingImage {
            path: "image.png".into(),
            width: 1,
            height: 1,
            bytes: 1,
        });
        let draft = state.input.draft_snapshot();
        state.finish_admission(AdmissionResult {
            action,
            draft,
            result: Ok((receipt(), None)),
            tx,
        });
        assert!(state.input.buf.is_empty());
        assert_eq!(state.input.pending_images.len(), 1);
        let expected = if matches!(action, AdmissionAction::Resume) {
            vec!["original task"]
        } else {
            vec![]
        };
        assert_eq!(state.input.history, expected);
    }
}

#[test]
fn failed_control_receipt_does_not_create_a_failed_submission_from_an_unrelated_draft() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    let draft = state.input.draft_snapshot();
    state.input.buf = "later draft".into();
    state.finish_admission(super::inbox::AdmissionResult {
        action: super::inbox::AdmissionAction::Update,
        draft,
        result: Err("stale revision".into()),
        tx,
    });
    assert_eq!(state.input.buf, "later draft");
    assert!(state.admission.failed_drafts.is_empty());
}

#[test]
fn input_history_and_failed_drafts_are_bounded() {
    let mut state = TuiState::default();
    for i in 0..600 {
        state.input.push_history(format!("cmd-{i}"));
    }
    assert_eq!(state.input.history.len(), 500);
    assert_eq!(
        state.input.history.first().map(String::as_str),
        Some("cmd-100")
    );
    assert_eq!(
        state.input.history.last().map(String::as_str),
        Some("cmd-599")
    );

    for i in 0..80 {
        let draft = state::InputState {
            buf: format!("draft-{i}"),
            ..Default::default()
        };
        state.push_failed_draft(draft);
    }
    assert_eq!(state.admission.failed_drafts.len(), 50);
    assert_eq!(
        state
            .admission
            .failed_drafts
            .first()
            .map(|d| d.buf.as_str()),
        Some("draft-30")
    );
    assert_eq!(
        state.admission.failed_drafts.last().map(|d| d.buf.as_str()),
        Some("draft-79")
    );
}

#[test]
fn paste_in_detail_view_does_not_mutate_hidden_input() {
    let mut state = TuiState {
        view: state::View::Goal { scroll: 0 },
        ..Default::default()
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    input::handle_event(
        crossterm::event::Event::Paste("secret".into()),
        &mut state,
        &tx,
    );
    assert!(
        state.input.buf.is_empty(),
        "paste while viewing a detail screen must not modify hidden input"
    );
}

#[test]
fn replay_preserves_tool_call_input_payload() {
    let events = vec![serde_json::json!({
        "type": "tool_call",
        "id": "call_1",
        "name": "Read",
        "input": { "path": "src/lib.rs" }
    })];
    let loaded = replay::build_lines_from_events(&events);
    let item = loaded
        .iter()
        .find(|item| item.tool_use_id.as_deref() == Some("call_1"))
        .expect("replayed tool call item");
    assert_eq!(
        item.tool_input,
        Some(serde_json::json!({ "path": "src/lib.rs" }))
    );
}
