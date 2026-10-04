use super::*;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use state::{PanelKind, TranscriptItem, TranscriptKind, View, WorkState};

fn draw(terminal: &mut Terminal<TestBackend>, state: &mut TuiState) {
    terminal.draw(|f| render(f, state, TuiMode::Full)).unwrap();
    while matches!(state.view, View::Main)
        && (state.cache.rebuild_next.is_some() || !state.cache.pending.is_empty())
    {
        terminal.draw(|f| render(f, state, TuiMode::Full)).unwrap();
    }
}
fn key(state: &mut TuiState, code: KeyCode, mods: KeyModifiers) {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    input::handle_event(Event::Key(KeyEvent::new(code, mods)), state, &tx);
}

#[test]
fn warmed_input_frame_does_not_visit_history_and_append_only_lays_out_new_item() {
    let mut state = TuiState::default();
    for index in 0..10_000 {
        state.push_line(TranscriptItem::new(
            format!("paragraph {index}\n"),
            TranscriptKind::Text,
        ));
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw(&mut terminal, &mut state);
    let visits = state.cache.visited;
    let parsed = state.cache.parsed;
    key(&mut state, KeyCode::Char('x'), KeyModifiers::NONE);
    draw(&mut terminal, &mut state);
    assert_eq!(state.cache.visited, visits);
    assert_eq!(state.cache.parsed, parsed);
    state.push_line(TranscriptItem::new("new".into(), TranscriptKind::Text));
    draw(&mut terminal, &mut state);
    assert_eq!(state.cache.visited, visits + 1);
}

#[test]
fn reading_anchor_survives_sealing_new_output_collapse_resize_and_detail_return() {
    let mut state = TuiState::default();
    for index in 0..30 {
        state.push_line(TranscriptItem::new(
            format!("message {index}: {}", "word ".repeat(30)),
            TranscriptKind::Text,
        ));
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw(&mut terminal, &mut state);
    key(&mut state, KeyCode::PageUp, KeyModifiers::NONE);
    draw(&mut terminal, &mut state);
    let anchor = state.viewport.anchor.clone().unwrap();
    state.lines[0].collapse_policy = state::CollapsePolicy::Auto {
        threshold_lines: 20,
    };
    state.lines[0].toggle_collapsed();
    state.invalidate_item(0);
    state.apply(&TuiSignal::Text("answer\n\nnext".into()));
    state.promote_stable_stream_prefix();
    state.finalize_stream();
    draw(&mut terminal, &mut state);
    assert!(!state.viewport.auto_scroll);
    assert_eq!(state.viewport.anchor.as_ref().unwrap().id, anchor.id);
    state.add_help();
    draw(&mut terminal, &mut state);
    key(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    terminal.backend_mut().resize(40, 24);
    draw(&mut terminal, &mut state);
    assert_eq!(state.viewport.anchor.as_ref().unwrap().id, anchor.id);
    assert!(!state.viewport.auto_scroll);
    key(&mut state, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert!(state.viewport.auto_scroll);
}

#[test]
fn detail_scroll_reuses_layout_and_clamps_authoritative_position() {
    let mut state = TuiState::default();
    state.artifact_detail = Some(state::ArtifactDetail {
        id: "a0001".into(),
        content: "long line 中文\n".repeat(20_000),
        truncated: false,
    });
    state.view = View::Artifact { scroll: 0 };
    let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
    draw(&mut terminal, &mut state);
    let parses = state.cache.detail_parses;
    key(&mut state, KeyCode::PageDown, KeyModifiers::NONE);
    draw(&mut terminal, &mut state);
    assert_eq!(state.cache.detail_parses, parses);
    state.view = View::Artifact { scroll: usize::MAX };
    draw(&mut terminal, &mut state);
    assert!(matches!(state.view, View::Artifact { scroll } if scroll == state.detail_max_scroll));
}

#[test]
fn inline_batches_release_committed_text_and_preserve_the_idle_last_item() {
    let mut state = TuiState {
        work_state: WorkState::WaitingModel,
        ..Default::default()
    };
    state.push_line(TranscriptItem::new(
        "output\n".repeat(600),
        TranscriptKind::Text,
    ));
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 16),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Inline(8),
        },
    )
    .unwrap();
    assert!(commit_ready(&mut terminal, &mut state).unwrap());
    assert_eq!(state.inline.committed, 0);
    assert_eq!(state.inline.row_offset, 256);
    assert!(commit_ready(&mut terminal, &mut state).unwrap());
    assert_eq!(state.inline.row_offset, 512);
    assert!(commit_ready(&mut terminal, &mut state).unwrap());
    assert_eq!(state.inline.committed, 1);
    assert!(state.lines[0].text.is_empty());
    assert!(state.lines[0].cached_lines.is_none());
    assert!(!commit_ready(&mut terminal, &mut state).unwrap());
    state.push_line(TranscriptItem::new("last".into(), TranscriptKind::Text));
    state.work_state = WorkState::Idle;
    assert!(!commit_ready(&mut terminal, &mut state).unwrap());
    assert_eq!(state.lines[1].text, "last");
}

#[test]
fn streaming_long_unclosed_structure_keeps_earlier_text_visible() {
    let mut state = TuiState::default();
    state.apply(&TuiSignal::Text(format!(
        "```text\nSTART_MARKER\n{}",
        "line\n".repeat(20_000)
    )));
    state.promote_stable_stream_prefix();
    assert!(state.lines.is_empty());
    state.viewport.auto_scroll = false;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw(&mut terminal, &mut state);
    let first = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(first.contains("START_MARKER"));
}

#[test]
fn incomplete_closing_fence_does_not_disable_incremental_code_layout() {
    let mut state = TuiState::default();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    state.apply(&TuiSignal::Text("```text\nfirst\n```".into()));
    state.promote_stable_stream_prefix();
    draw(&mut terminal, &mut state);
    assert!(!state.cache.stream_code_closed);
    state.apply(&TuiSignal::Text("suffix\n".into()));
    state.promote_stable_stream_prefix();
    draw(&mut terminal, &mut state);
    assert!(!state.cache.stream_code_closed);
    assert_eq!(state.cache.stream_parses, 3);
    assert!(
        state
            .cache
            .stream_code_lines
            .iter()
            .any(|line| line.to_string().contains("```suffix"))
    );
}

#[test]
fn mixed_fence_markers_and_lengths_are_literal_inside_code() {
    let blocks = markdown::parse_blocks("````rust\n~~~\n```\ncode\n````\nafter");
    assert!(
        matches!(&blocks[0], markdown::MdBlock::CodeBlock { lines, .. } if lines == &["~~~", "```", "code"])
    );
}

#[test]
fn grapheme_editing_and_paste_undo_preserve_unicode_clusters() {
    let mut state = TuiState::default();
    let text = "e\u{301}👨‍👩‍👧‍👦🇨🇳中";
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    input::handle_event(Event::Paste(text.into()), &mut state, &tx);
    key(&mut state, KeyCode::Char('z'), KeyModifiers::CONTROL);
    assert!(state.input.buf.is_empty());
    key(&mut state, KeyCode::Char('z'), KeyModifiers::ALT);
    assert_eq!(state.input.buf, text);
    key(&mut state, KeyCode::Backspace, KeyModifiers::NONE);
    key(&mut state, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(state.input.buf, "e\u{301}👨‍👩‍👧‍👦");
    key(&mut state, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(state.input.cursor, "e\u{301}".len());
    key(&mut state, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(state.input.buf, "👨‍👩‍👧‍👦");
}

#[test]
fn visual_vertical_movement_preserves_column_and_home_end_are_logical() {
    let mut state = TuiState::default();
    state.input.buf = "abcdef\nx\nabcdef".into();
    state.input.cursor = 4;
    state.cache.width = 80;
    key(&mut state, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(state.input.cursor, 8);
    key(&mut state, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(state.input.cursor, 13);
    key(&mut state, KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(state.input.cursor, 9);
    key(&mut state, KeyCode::End, KeyModifiers::NONE);
    assert_eq!(state.input.cursor, state.input.buf.len());
    key(&mut state, KeyCode::Char('a'), KeyModifiers::CONTROL);
    assert_eq!(state.input.cursor, 0);
}

#[test]
fn local_panels_do_not_echo_or_seal_model_stream_and_esc_does_not_quit() {
    let mut state = TuiState::default();
    state.apply(&TuiSignal::Text("open stream".into()));
    for command in ["/help", "/status", "/inputs", "/details"] {
        state.input.buf = command.into();
        key(&mut state, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(state.view, View::Panel { .. }));
        assert_eq!(state.stream_line, "open stream");
        assert!(state.lines.is_empty());
        key(&mut state, KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(state.view, View::Main));
    }
    key(&mut state, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!state.quit);
    state.view = View::Panel {
        panel: PanelKind::Status,
        scroll: 7,
        selected: 0,
    };
    key(&mut state, KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert!(matches!(state.view, View::Panel { scroll: 7, .. }));
}

#[test]
fn narrow_status_preserves_work_and_counts_cache_creation_as_input() {
    let mut state = TuiState {
        model: "very-long-model-name".repeat(20),
        work_state: WorkState::RunningTool,
        ..Default::default()
    };
    let narrow = render::build_status_line(&state, 40);
    assert!(narrow.contains("[tool]"));
    assert!(unicode_width::UnicodeWidthStr::width(narrow.as_str()) <= 40);
    state.model = "m".into();
    state.stats.total_input_tokens = 100;
    state.stats.total_cache_read_tokens = 100;
    state.stats.total_cache_creation_tokens = 100;
    assert!(render::build_status_line(&state, 120).contains("I:300(33%)"));
}

#[test]
fn resize_only_reflows_the_uncommitted_inline_remainder() {
    let mut state = TuiState {
        work_state: WorkState::WaitingModel,
        ..Default::default()
    };
    let prefix = (0..256)
        .map(|index| format!("committed-{index}\n"))
        .collect::<String>();
    state.push_line(TranscriptItem::new(
        prefix + &"remaining-long-line ".repeat(30),
        TranscriptKind::Text,
    ));
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 16),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Inline(8),
        },
    )
    .unwrap();
    commit_ready(&mut terminal, &mut state).unwrap();
    assert_eq!(state.inline.row_offset, 256);
    terminal.backend_mut().resize(40, 16);
    let before = terminal
        .backend()
        .scrollback()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    while commit_ready(&mut terminal, &mut state).unwrap() {}
    let all = terminal
        .backend()
        .scrollback()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(all.starts_with(&before));
    assert_eq!(all.matches("committed-0 ").count(), 1);
    assert!(all.contains("remaining-long-line"));
}

#[test]
fn inline_can_draw_after_committing_the_entire_active_queue() {
    let mut state = TuiState {
        work_state: WorkState::WaitingModel,
        ..Default::default()
    };
    state.push_line(TranscriptItem::new(
        "user input".into(),
        TranscriptKind::Info,
    ));
    let mut terminal = Terminal::with_options(
        TestBackend::new(80, 16),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Inline(8),
        },
    )
    .unwrap();
    commit_ready(&mut terminal, &mut state).unwrap();
    terminal
        .draw(|f| render(f, &mut state, TuiMode::Inline))
        .unwrap();
    assert_eq!(state.inline.committed, state.lines.len());
    assert_eq!(state.cache.heights.total(), 0);
}

#[test]
fn unrelated_subagent_progress_preserves_artifact_layout() {
    let mut state = TuiState::default();
    state.artifact_detail = Some(state::ArtifactDetail {
        id: "a0001".into(),
        content: "body\n".repeat(100),
        truncated: false,
    });
    state.view = View::Artifact { scroll: 0 };
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw(&mut terminal, &mut state);
    let parses = state.cache.detail_parses;
    state.apply(&TuiSignal::SubAgentStatus {
        session_id: "sub_other".into(),
        status: "running".into(),
        in_tokens: 0,
        out_tokens: 0,
    });
    state.apply(&TuiSignal::SubAgentStream {
        session_id: "sub_other".into(),
        kind: crate::ui::SubAgentStreamKind::Thinking,
        content: "progress".into(),
    });
    draw(&mut terminal, &mut state);
    assert_eq!(state.cache.detail_parses, parses);
}

#[test]
fn anchor_is_retained_during_batched_resize_layout() {
    let mut state = TuiState::default();
    for _ in 0..10_000 {
        state.push_line(TranscriptItem::new(
            "content ".repeat(10),
            TranscriptKind::Text,
        ));
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw(&mut terminal, &mut state);
    key(&mut state, KeyCode::PageUp, KeyModifiers::NONE);
    draw(&mut terminal, &mut state);
    let id = state.viewport.anchor.as_ref().unwrap().id;
    terminal.backend_mut().resize(40, 24);
    terminal
        .draw(|f| render(f, &mut state, TuiMode::Full))
        .unwrap();
    assert!(state.cache.rebuild_next.is_some());
    assert_eq!(state.viewport.anchor.as_ref().unwrap().id, id);
    draw(&mut terminal, &mut state);
    assert_eq!(state.viewport.anchor.as_ref().unwrap().id, id);
}

#[test]
fn panel_resource_invalidation_matches_selected_row_and_not_other_resources() {
    let mut state = TuiState {
        view: View::Panel {
            panel: PanelKind::Inputs,
            scroll: 0,
            selected: 0,
        },
        ..Default::default()
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    draw(&mut terminal, &mut state);
    let parses = state.cache.detail_parses;
    state.invalidate_detail_resource("panel:Status");
    draw(&mut terminal, &mut state);
    assert_eq!(state.cache.detail_parses, parses);
    state.invalidate_detail_resource("panel:Inputs");
    draw(&mut terminal, &mut state);
    assert_eq!(state.cache.detail_parses, parses + 1);
}

#[test]
fn budgeted_signal_processing_retains_reliable_order_and_pending_events() {
    let mut state = TuiState::default();
    let mut pending = (0..800)
        .map(|index| TuiSignal::Info(format!("event {index}")))
        .collect::<std::collections::VecDeque<_>>();
    let (changed, immediate) = process_signal_batch(&mut pending, &mut state, TuiMode::Full);
    assert!(changed && immediate);
    assert!(pending.len() >= 288 && pending.len() < 800);
    while !pending.is_empty() {
        process_signal_batch(&mut pending, &mut state, TuiMode::Full);
    }
    assert_eq!(state.lines.len(), 800);
    for (index, item) in state.lines.iter().enumerate() {
        assert_eq!(item.text, format!("event {index}"));
    }
}

#[test]
fn inline_partial_terminal_write_failure_preserves_commit_position_and_body() {
    use ratatui::{
        backend::{Backend, ClearType, WindowSize},
        buffer::Cell,
        layout::{Position, Size},
    };
    struct PartialWrite(TestBackend, usize);
    impl Backend for PartialWrite {
        type Error = std::io::Error;
        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.1 += 1;
            self.0.draw(content.take(2)).unwrap();
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "partial write",
            ))
        }
        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.0.hide_cursor().unwrap();
            Ok(())
        }
        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.0.show_cursor().unwrap();
            Ok(())
        }
        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            Ok(self.0.get_cursor_position().unwrap())
        }
        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.0.set_cursor_position(position).unwrap();
            Ok(())
        }
        fn clear(&mut self) -> Result<(), Self::Error> {
            self.0.clear().unwrap();
            Ok(())
        }
        fn clear_region(&mut self, region: ClearType) -> Result<(), Self::Error> {
            self.0.clear_region(region).unwrap();
            Ok(())
        }
        fn size(&self) -> Result<Size, Self::Error> {
            Ok(self.0.size().unwrap())
        }
        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            Ok(self.0.window_size().unwrap())
        }
        fn flush(&mut self) -> Result<(), Self::Error> {
            self.0.flush().unwrap();
            Ok(())
        }
        fn append_lines(&mut self, count: u16) -> Result<(), Self::Error> {
            self.0.append_lines(count).unwrap();
            Ok(())
        }
        fn scroll_region_up(
            &mut self,
            region: std::ops::Range<u16>,
            count: u16,
        ) -> Result<(), Self::Error> {
            self.0.scroll_region_up(region, count).unwrap();
            Ok(())
        }
        fn scroll_region_down(
            &mut self,
            region: std::ops::Range<u16>,
            count: u16,
        ) -> Result<(), Self::Error> {
            self.0.scroll_region_down(region, count).unwrap();
            Ok(())
        }
    }
    let mut terminal = Terminal::with_options(
        PartialWrite(TestBackend::new(80, 24), 0),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Inline(8),
        },
    )
    .unwrap();
    let mut state = TuiState {
        work_state: WorkState::WaitingModel,
        ..Default::default()
    };
    state.push_line(TranscriptItem::new(
        "body to preserve".into(),
        TranscriptKind::Text,
    ));
    let error = commit_ready(&mut terminal, &mut state).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(terminal.backend().1, 1);
    assert_eq!(state.inline.committed, 0);
    assert_eq!(state.inline.row_offset, 0);
    assert_eq!(state.lines[0].text, "body to preserve");
}
