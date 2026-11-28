use super::*;

#[test]
fn unknown_slash_command_does_not_reach_orchestrator() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "/unknown".into();
    state.input.cursor = "/unknown".len();

    let key = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    );

    assert!(!handle_event(
        crossterm::event::Event::Key(key),
        &mut state,
        &tx
    ));
    assert!(rx.try_recv().is_err());
    assert_eq!(state.stream.work_state, WorkState::Idle);
    assert!(state.transcript.lines.iter().any(|line| {
        line.kind == TranscriptKind::Info && line.text.contains("Unknown command")
    }));
}

#[test]
fn mistyped_goal_subcommand_is_refused_with_a_suggestion_and_no_dispatch() {
    // `/goal cancal` used to dispatch a create, so a cancellation attempt
    // silently became a goal whose objective was the typo.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "/goal cancal".into();
    state.input.cursor = "/goal cancal".len();

    let key = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    );

    assert!(!handle_event(
        crossterm::event::Event::Key(key),
        &mut state,
        &tx
    ));
    assert!(
        rx.try_recv().is_err(),
        "a typo must not reach the runtime as a command"
    );
    let line = state
        .transcript
        .lines
        .iter()
        .find(|line| line.text.contains("cancel"))
        .expect("suggestion line");
    assert_eq!(line.kind, TranscriptKind::Info);
    assert!(line.text.contains("是否") || line.text.contains("did you mean"));
}

#[test]
fn slash_command_parser_classifies_known_unknown_and_text() {
    assert_eq!(
        parse_slash_command("/flash").unwrap(),
        Some(SlashCommand::Flash)
    );
    assert_eq!(
        parse_slash_command("/model qwen3-coder-plus").unwrap(),
        Some(SlashCommand::Model("qwen3-coder-plus".into()))
    );
    assert_eq!(parse_slash_command("/q").unwrap(), Some(SlashCommand::Quit));
    assert_eq!(parse_slash_command(" /flash").unwrap(), None);
    assert!(parse_slash_command("/unknown").is_err());
}

#[test]
fn slash_command_dispatches_goal_command_to_runtime() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "/goal ship the release".into();
    state.input.cursor = state.input.buf.len();

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    let command = rx.try_recv().expect("goal command");
    let crate::cli::RuntimeCmd::Goal(expected) = command else {
        panic!("expected a Goal command");
    };
    assert_eq!(
        expected,
        crate::local::GoalCommand::Create("ship the release".into())
    );
    assert!(state.input.buf.is_empty());
}

#[test]
fn heartbeat_status_label_extracts_elapsed_seconds() {
    assert_eq!(
        crate::tui::state::heartbeat_status_label(&mink::runtime::llm_wait_heartbeat_message(
            30, 0
        ))
        .as_deref(),
        Some("·30s")
    );
    assert_eq!(
        crate::tui::state::heartbeat_status_label(&mink::runtime::llm_wait_heartbeat_message(
            60, 45
        ))
        .as_deref(),
        Some("·60s")
    );
    assert_eq!(
        crate::tui::state::heartbeat_status_label("Retrying (1/3)..."),
        None
    );
    assert_eq!(crate::tui::state::heartbeat_status_label("其他信息"), None);
}

#[test]
fn status_bar_shows_transient_wait_label() {
    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            cwd_label: String::new(),
            ..Default::default()
        },
        ..Default::default()
    };
    let line = build_status_line(&state, 80);
    assert!(!line.contains("·"));
    // 首事件等待阶段（未流式）的心跳同样只进状态栏，不进 transcript。
    state.apply(TuiSignal::Info(mink::runtime::llm_wait_heartbeat_message(
        30, 0,
    )));
    assert_eq!(state.transcript.lines.len(), 0);
    assert_eq!(state.stream.stream_status.as_deref(), Some("·30s"));
    // 流式阶段心跳替换更新标签。
    state.apply(TuiSignal::Text("hello".into()));
    assert_eq!(state.stream.stream_status, None);
    state.apply(TuiSignal::Info(mink::runtime::llm_wait_heartbeat_message(
        60, 45,
    )));
    assert_eq!(state.stream.stream_status.as_deref(), Some("·60s"));
    let line = build_status_line(&state, 80);
    assert!(line.contains("·60s"));
    assert!(line.contains("[generating]"));
    state.apply(TuiSignal::Stop);
    assert_eq!(state.stream.stream_status, None);
}

#[test]
fn status_click_width_uses_display_columns() {
    assert_eq!(status_text_width("model:flash"), 11);
    assert_eq!(status_text_width("模型"), 4);
    assert_eq!(status_text_width("🙂"), 2);
}

#[test]
fn status_line_adapts_to_terminal_width() {
    let mut state = TuiState {
        stream: state::StreamState {
            work_state: WorkState::StreamingText,
            ..Default::default()
        },
        mirrors: state::SessionMirrors {
            cwd_label: "mink-new".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    state.mirrors.stats.current_turn_count = 12;
    state.mirrors.stats.agent_request_count = 18;
    state.mirrors.stats.total_input_tokens = 14_000;
    state.mirrors.stats.total_cache_read_tokens = 2_000;
    state.mirrors.stats.total_output_tokens = 3_000;
    state.mirrors.stats.current_context_tokens = 58_000;
    state.mirrors.stats.max_context_tokens = 80_000;
    state.mirrors.stats.belief = 0.75;

    let narrow = build_status_line(&state, 40);
    let medium = build_status_line(&state, 80);
    let wide = build_status_line(&state, 120);

    assert!(unicode_width::UnicodeWidthStr::width(narrow.as_str()) <= 40);
    assert!(unicode_width::UnicodeWidthStr::width(medium.as_str()) <= 80);
    assert!(unicode_width::UnicodeWidthStr::width(wide.as_str()) <= 120);
    assert!(!narrow.contains(" T:"));
    assert!(!narrow.contains("@mink-new"));
    assert!(medium.contains(" I:"));
    assert!(medium.contains("@mink-new"));
    assert!(wide.contains(" T:12"));
    assert!(wide.contains(" R:18"));
}

#[test]
fn status_spans_style_path_and_work_state() {
    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            model: "deepseek-chat".into(),
            cwd_label: "mink-new".into(),
            ..Default::default()
        },
        stream: state::StreamState {
            work_state: WorkState::RunningTool,
            ..Default::default()
        },
        ..Default::default()
    };
    state.mirrors.stats.belief = 0.75;

    let line = build_status_spans(&state, 120);

    assert_eq!(line_text(&line), build_status_line(&state, 120));
    assert_eq!(line.spans[1].style.fg, Some(Color::Blue));
    assert!(
        line.spans[1]
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
    let path_span = line
        .spans
        .iter()
        .find(|span| span.content.as_ref() == "@mink-new")
        .unwrap();
    assert_eq!(path_span.style.fg, Some(Color::DarkGray));
    let work_span = line.spans.last().unwrap();
    assert_eq!(work_span.content.as_ref(), "[tool]");
    assert_eq!(work_span.style.fg, Some(Color::Yellow));
    assert!(
        work_span
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

#[test]
fn status_spans_color_error_state_as_error() {
    let state = TuiState {
        stream: state::StreamState {
            work_state: WorkState::Error,
            ..Default::default()
        },
        mirrors: state::SessionMirrors {
            cwd_label: "mink-new".into(),
            ..Default::default()
        },
        ..Default::default()
    };

    let line = build_status_spans(&state, 120);
    let work_span = line.spans.last().unwrap();

    assert_eq!(work_span.content.as_ref(), "[error]");
    assert_eq!(work_span.style.fg, Some(Color::Red));
    assert!(
        work_span
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
}

#[test]
fn slash_command_keeps_pending_images_queued() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "/plan".into();
    state.input.cursor = state.input.buf.len();
    state.input.pending_images = vec![pending_image("a")];

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    assert!(
        rx.try_recv().is_err(),
        "slash commands never reach the runtime"
    );
    assert!(matches!(state.view, View::Plan { .. }));
    assert_eq!(state.input.pending_images.len(), 1);
    assert!(
        state
            .transcript
            .lines
            .iter()
            .any(|line| line.text.contains("stay queued")),
        "queue notice missing"
    );
}

#[test]
fn status_bar_shows_goal_marker_with_status_and_remaining_turns() {
    let mut state = TuiState::default();
    assert!(!build_status_line(&state, 100).contains("goal:"));
    state.mirrors.goal = Some(goal_display());
    let line = build_status_line(&state, 100);
    assert!(line.contains("goal:active 12/20"), "{line}");
}

#[test]
fn status_bar_omits_hidden_goal_and_marks_completion_basis() {
    let mut state = TuiState::default();
    let mut done = goal_display();
    done.status = "completed".into();
    done.phase = "idle".into();
    done.remaining_turns = 0;
    done.completion_basis = Some("verified".into());
    state.mirrors.goal = Some(done.clone());
    assert!(
        build_status_line(&state, 100).contains("goal:completed verified"),
        "completed goals state their basis"
    );
    done.hidden = true;
    state.mirrors.goal = Some(done);
    assert!(!build_status_line(&state, 100).contains("goal:"));
}

#[test]
fn status_goal_marker_reports_its_column_range() {
    let mut state = TuiState::default();
    assert_eq!(status_goal_span(&state, 100), None);
    state.mirrors.goal = Some(goal_display());
    let (start, end) = status_goal_span(&state, 100).expect("goal marker is clickable");
    assert!(start < end, "marker must occupy a non-empty column range");
    // The reported range must line up with the rendered marker text.
    let line = build_status_line(&state, 100);
    let marker = "goal:active 12/20";
    let at = line.find(marker).expect("marker rendered");
    assert!(
        at >= usize::from(start) && at + marker.len() <= usize::from(end),
        "range {start}..{end} must cover marker at {at}..{}",
        at + marker.len()
    );
}

#[test]
fn clicking_the_goal_marker_opens_the_goal_view() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            goal: Some(goal_display()),
            status_row: 4,
            status_goal_span: Some((10, 28)),
            ..Default::default()
        },
        ..Default::default()
    };

    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 14,
        row: 4,
        modifiers: KeyModifiers::NONE,
    };
    assert!(!handle_event(Event::Mouse(click), &mut state, &tx));
    assert!(
        matches!(state.view, View::Goal { scroll: 0 }),
        "{:?}",
        state.view
    );

    // A click elsewhere on the status row must not open it.
    state.view = View::Main;
    let outside = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 40,
        row: 4,
        modifiers: KeyModifiers::NONE,
    };
    assert!(!handle_event(Event::Mouse(outside), &mut state, &tx));
    assert!(matches!(state.view, View::Main));
}

#[test]
fn goal_updated_signal_replaces_state_goal() {
    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            goal: Some(goal_display()),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut paused = goal_display();
    paused.status = "paused".into();
    paused.phase = "idle".into();
    state.apply(TuiSignal::GoalUpdated(Box::new(paused)));
    let goal = state.mirrors.goal.expect("goal retained");
    assert_eq!(goal.status, "paused");
    assert_eq!(goal.phase, "idle");
}

#[test]
fn goals_slash_command_opens_the_goal_view() {
    assert_eq!(
        parse_slash_command("/goals").unwrap(),
        Some(SlashCommand::Goals)
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            goal: Some(goal_display()),
            ..Default::default()
        },
        ..Default::default()
    };
    state.input.buf = "/goals".into();
    state.input.cursor = state.input.buf.len();
    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));
    assert!(
        matches!(state.view, View::Goal { scroll: 0 }),
        "{:?}",
        state.view
    );
}

#[test]
fn goal_view_derives_progress_from_current_contract_review() {
    let mut goal = goal_display();
    goal.subgoals = vec![mink_goal::GoalSubgoal {
        id: "delivery".into(),
        objective: "deliver parser".into(),
    }];
    goal.criteria = vec![mink_goal::GoalCriterion {
        id: "proof".into(),
        subgoal_id: "delivery".into(),
        outcome: "parser delivered".into(),
        required_evidence: "parser output".into(),
    }];
    goal.reviews = vec![serde_json::from_value(serde_json::json!({
        "candidate":{"goal_id":"goal-1","contract_revision":1,"iteration":1,"conversation_seq":1,"reviewer_id":"auditor"},
        "verdict":"satisfied","original_request":{"status":"pass","reason":"checked","evidence":["proof.txt"]},
        "criteria":[{"criterion_id":"proof","status":"pass","reason":"checked","evidence":["proof.txt"]}],
        "constraints":[],"summary":"reviewed","next_step":"","blocker":null
    })).unwrap()];
    for (revision, generation, passes) in [(1, 0, true), (1, 1, false), (2, 0, false)] {
        goal.contract_revision = revision;
        let mut candidate = goal.reviews[0].candidate.clone();
        candidate.generation = generation;
        goal.review_candidate = Some(candidate);
        let state = TuiState {
            mirrors: state::SessionMirrors {
                goal: Some(goal.clone()),
                ..Default::default()
            },
            ..Default::default()
        };
        let text = super::render::goal_detail_lines(&state)
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();
        assert!(text.contains(if passes {
            "1/1 criteria reviewed as pass"
        } else {
            "0/1 criteria reviewed as pass"
        }));
        assert!(text.contains("evidence: parser output"));
    }
}

#[test]
fn goal_view_renders_objective_limits_and_reason() {
    let mut goal = goal_display();
    goal.status = "blocked".into();
    goal.reason = Some("needs a decision".into());
    let state = TuiState {
        mirrors: state::SessionMirrors {
            goal: Some(goal),
            ..Default::default()
        },
        view: View::Goal { scroll: 0 },
        ..Default::default()
    };
    let lines = super::render::goal_detail_lines(&state);
    let text = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "blocked",
        "ship the release",
        "cut a tag",
        "no network",
        "12/20",
        "needs a decision",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
}

#[test]
fn goal_view_sanitises_persisted_user_text_before_rendering() {
    let mut goal = goal_display();
    // The objective is user input persisted to goal.json and re-read on resume;
    // control sequences must never reach the terminal unfiltered.
    goal.objective = "ship \u{1b}[31mred\u{1b}[0m \u{7}bell".into();
    goal.constraints = vec!["no \u{1b}[2Jclear".into()];
    goal.reason = Some("needs \u{1b}[1minput".into());
    let state = TuiState {
        mirrors: state::SessionMirrors {
            goal: Some(goal),
            ..Default::default()
        },
        view: View::Goal { scroll: 0 },
        ..Default::default()
    };
    let text = super::render::goal_detail_lines(&state)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains('\u{1b}'), "escape survived: {text:?}");
    assert!(!text.contains('\u{7}'), "bell survived: {text:?}");
    assert!(text.contains("ship"), "plain text was dropped: {text:?}");
}

/// The terminal parser and the web workbench are tested against one shared
/// fixture, so a command added on one surface cannot be missing on the other.
#[test]
fn slash_command_surface_matches_the_shared_contract() {
    let fixture = include_str!("../slash_commands.json");
    let cases: serde_json::Value = serde_json::from_str(fixture).expect("shared command fixture");
    for case in cases["commands"].as_array().expect("commands array") {
        let input = case["input"].as_str().expect("input");
        let parsed = super::command::parse_slash_command(input);
        if case.get("not_command").and_then(serde_json::Value::as_bool) == Some(true) {
            assert!(parsed.is_ok(), "{input} should parse as prose");
            assert!(parsed.unwrap().is_none(), "{input} is not a command");
            continue;
        }
        if case.get("error").and_then(serde_json::Value::as_bool) == Some(true) {
            assert!(parsed.is_err(), "{input} should be rejected");
            continue;
        }
        let command = parsed
            .unwrap_or_else(|e| panic!("{input} should parse: {e:?}"))
            .unwrap_or_else(|| panic!("{input} should be a command"));
        assert_eq!(
            command_name(&command),
            case["kind"].as_str().unwrap(),
            "{input}"
        );
    }
}

#[test]
fn goal_boundary_cards_render_from_the_structured_payload() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::GoalCard(Box::new(goal_message(
        mink_goal::GoalMessageKind::Draft,
        0,
        Some("add evidence"),
    ))));
    state.apply(TuiSignal::GoalCard(Box::new(goal_message(
        mink_goal::GoalMessageKind::Start,
        1,
        None,
    ))));
    let cards: Vec<_> = state
        .transcript
        .lines
        .iter()
        .filter(|item| item.kind == TranscriptKind::Goal)
        .collect();
    assert_eq!(cards.len(), 2, "each boundary opens one card");

    let draft = goal_card_lines(cards[0], 60);
    assert!(draft.contains("目标草案讨论"), "{draft}");
    assert!(draft.contains("契约 r2"), "{draft}");
    assert!(draft.contains("修改意见：add evidence"), "{draft}");
    assert!(!draft.contains("第 0 轮"), "a draft has no round: {draft}");

    let start = goal_card_lines(cards[1], 60);
    assert!(start.contains("开始目标"), "{start}");
    assert!(start.contains("第 1 轮 · 契约 r2"), "{start}");
    assert!(start.contains("parses fixtures"), "{start}");
    assert!(start.contains("no network"), "{start}");
    // Inline/plain projection carries no expand affordance.
    assert!(!start.contains('▶') && !start.contains('▼'), "{start}");
}

#[test]
fn clicking_a_goal_card_opens_the_goal_view() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.apply(TuiSignal::GoalCard(Box::new(goal_message(
        mink_goal::GoalMessageKind::Start,
        1,
        None,
    ))));
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut state, TuiMode::Full))
        .unwrap();

    let target = state
        .viewport
        .click_map
        .iter()
        .find(|target| target.action == crate::tui::state::ClickAction::OpenGoal)
        .expect("the goal card registers a click target");
    let row = state.viewport.content_y + target.start_row as u16;
    handle_event(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        &mut state,
        &tx,
    );
    assert!(
        matches!(state.view, View::Goal { scroll: 0 }),
        "{:?}",
        state.view
    );
}

#[test]
fn goal_card_keyboard_collapses_the_newest_card_and_opens_the_detail() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.apply(TuiSignal::GoalCard(Box::new(goal_message(
        mink_goal::GoalMessageKind::Start,
        1,
        None,
    ))));
    let index = state.transcript.lines.len() - 1;
    assert!(!state.transcript.lines[index].collapsed);

    let collapse = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(!handle_event(Event::Key(collapse), &mut state, &tx));
    assert!(
        state.transcript.lines[index].collapsed,
        "Ctrl+O folds the newest card"
    );
    assert!(matches!(state.view, View::Main));

    let open = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert!(!handle_event(Event::Key(open), &mut state, &tx));
    assert!(
        matches!(state.view, View::Goal { scroll: 0 }),
        "{:?}",
        state.view
    );
}

#[test]
fn inline_goal_card_output_is_stable_across_renders() {
    let mut item = TranscriptItem::goal_card(goal_message(
        mink_goal::GoalMessageKind::Continuation,
        3,
        None,
    ));
    let first = goal_card_lines(&item, 50);
    let second = goal_card_lines(&item, 50);
    assert_eq!(first, second, "repeat renders must be byte-identical");
    assert!(first.contains("继续执行目标"), "{first}");
    assert!(first.contains("第 3 轮 · 契约 r2"), "{first}");
    assert!(!first.contains('▶') && !first.contains('▼'), "{first}");

    // The Inline projection never mints a second item for the same render.
    let mut state = TuiState::default();
    state.apply(TuiSignal::GoalCard(Box::new(
        item.goal_message.take().unwrap(),
    )));
    let before = state.transcript.lines.len();
    let _ = goal_card_lines(&state.transcript.lines[0], 50);
    let _ = goal_card_lines(&state.transcript.lines[0], 50);
    assert_eq!(state.transcript.lines.len(), before);
}

#[test]
fn goal_card_fold_and_unfold_render_the_screen_marker() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::GoalCard(Box::new(goal_message(
        mink_goal::GoalMessageKind::Start,
        1,
        None,
    ))));
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    // Buffer-wide CJK cells carry a trailing blank cell; strip spaces so the
    // assertions read the glyphs that were rendered.
    let screen = |terminal: &Terminal<TestBackend>| -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .replace(' ', "")
    };
    terminal
        .draw(|frame| render(frame, &mut state, TuiMode::Full))
        .unwrap();
    assert!(
        screen(&terminal).contains('▼'),
        "expanded marker missing: {:?}",
        screen(&terminal)
    );
    assert!(
        screen(&terminal).contains("开始目标"),
        "boundary label missing: {:?}",
        screen(&terminal)
    );

    let index = state.transcript.lines.len() - 1;
    state.transcript.lines[index].toggle_collapsed();
    state.invalidate_item(index);
    terminal
        .draw(|frame| render(frame, &mut state, TuiMode::Full))
        .unwrap();
    assert!(screen(&terminal).contains('▶'), "folded marker missing");
    assert!(
        !screen(&terminal).contains("parsesfixtures"),
        "folding must hide the card body: {:?}",
        screen(&terminal)
    );

    state.transcript.lines[index].toggle_collapsed();
    state.invalidate_item(index);
    terminal
        .draw(|frame| render(frame, &mut state, TuiMode::Full))
        .unwrap();
    assert!(screen(&terminal).contains('▼'), "unfolded marker missing");
    assert!(
        screen(&terminal).contains("parsesfixtures"),
        "unfolding must restore the card body: {:?}",
        screen(&terminal)
    );
}

#[test]
fn history_load_adopts_the_goal_mirror_without_overwriting_a_live_one() {
    // The background state reads goal.json; a restored session must show the
    // marker before any GoalUpdated arrives.
    let mut live = TuiState {
        async_load: state::AsyncLoadState {
            loading_history: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let history = TuiState {
        mirrors: state::SessionMirrors {
            goal: Some(goal_display()),
            ..Default::default()
        },
        ..Default::default()
    };
    live.apply_ui_event(crate::tui::state::TuiUiEvent::HistoryLoaded {
        generation: 0,
        state: Box::new(history),
    });
    assert_eq!(
        live.mirrors.goal.as_ref().map(|goal| goal.id.clone()),
        Some("goal-1".to_string())
    );

    // A mirror the runtime already published wins over the disk read.
    let mut paused = goal_display();
    paused.status = "paused".into();
    let mut live = TuiState {
        async_load: state::AsyncLoadState {
            loading_history: true,
            ..Default::default()
        },
        mirrors: state::SessionMirrors {
            goal: Some(paused),
            ..Default::default()
        },
        ..Default::default()
    };
    let history = TuiState {
        mirrors: state::SessionMirrors {
            goal: Some(goal_display()),
            ..Default::default()
        },
        ..Default::default()
    };
    live.apply_ui_event(crate::tui::state::TuiUiEvent::HistoryLoaded {
        generation: 0,
        state: Box::new(history),
    });
    assert_eq!(
        live.mirrors.goal.as_ref().map(|goal| goal.status.clone()),
        Some("paused".to_string())
    );
}

#[test]
fn live_goal_card_kinds_render_on_screen() {
    let mut state = TuiState::default();
    for (kind, iteration, feedback) in [
        (
            mink_goal::GoalMessageKind::Draft,
            0,
            Some("add the fixture"),
        ),
        (mink_goal::GoalMessageKind::Start, 1, None),
        (mink_goal::GoalMessageKind::Continuation, 2, None),
    ] {
        state.apply(TuiSignal::GoalCard(Box::new(goal_message(
            kind, iteration, feedback,
        ))));
    }
    let backend = TestBackend::new(80, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut state, TuiMode::Full))
        .unwrap();
    let tight = screen_text(&terminal).replace(' ', "");
    assert!(tight.contains("目标草案讨论"), "{tight}");
    assert!(tight.contains("修改意见：addthefixture"), "{tight}");
    assert!(tight.contains("开始目标"), "{tight}");
    assert!(tight.contains("第1轮·契约r2"), "{tight}");
    assert!(tight.contains("继续执行目标"), "{tight}");
    assert!(tight.contains("第2轮·契约r2"), "{tight}");
    assert!(!tight.contains("第0轮"), "a draft has no round: {tight}");
}

#[test]
fn session_fixture_restores_goal_cards_and_never_echoes_internal_rows() {
    let directory = temp_session_dir("session");
    let rows = vec![
        serde_json::json!({"role":"user","content":"please do the thing"}),
        serde_json::json!({"role":"assistant","content":[{"type":"text","text":"working on it"}]}),
        serde_json::json!({"role":"user","content":"use the fixture","_mink":{"guidance":true,"input_id":"i1"}}),
        // Runtime-internal families, exactly as core writes them.
        serde_json::json!({
            "role":"user",
            "content":"This is a runtime-generated checkpoint replacing an earlier span of the conversation; it may be a model summary or a lossy excerpt.\n\n<compacted-summary>\nold history\n</compacted-summary>",
            "internal": true
        }),
        serde_json::json!({
            "role":"user",
            "content":"<todo-sync revision=\"3\">\nThe persisted todo state is newer than the active conversation.\n</todo-sync>\n\n<current-todos>\n- do it\n</current-todos>",
            "_mink":{"todo_revision":3,"todo_state_kind":"sync"}
        }),
        serde_json::json!({
            "role":"user",
            "content":"Plan confirmed transition",
            "internal": true,
            "_mink":{"plan_transaction_id":"tx-1"}
        }),
        goal_boundary_row(
            "draft",
            0,
            Some("add the fixture"),
            "<goal-draft>\nFeedback: add the fixture\n</goal-draft>",
        ),
        goal_boundary_row(
            "start",
            1,
            None,
            "<current-goal>\nGoal ID: goal-1\nRound: 1/20\n</current-goal>",
        ),
        goal_boundary_row(
            "continuation",
            2,
            None,
            "<current-goal>\nGoal ID: goal-1\nRound: 2/20\n</current-goal>",
        ),
        serde_json::json!({
            "role":"user",
            "content":"<goal-review>host submission prompt</goal-review>",
            "_mink":{"internal":true,"source":"goal_adjudication","candidate":{"goal_id":"goal-1"}}
        }),
        serde_json::json!({
            "role":"user",
            "content":"<current-goal>\nGoal ID: goal-1\nState: ended\n</current-goal>",
            "_mink":{"internal":true,"goal_id":"goal-1","goal_revision":9,"goal_state_kind":"ended"}
        }),
        serde_json::json!({
            "role":"user",
            "content":"<current-goal>\nGoal ID: goal-1\nState: contract\n</current-goal>",
            "_mink":{"internal":true,"goal_id":"goal-1","goal_revision":9,"goal_state_kind":"contract"}
        }),
    ];
    let conversation = rows
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(directory.join("conversation.jsonl"), conversation).unwrap();
    std::fs::write(directory.join("events.jsonl"), "").unwrap();

    let items = crate::tui::replay::load_session(&directory.join("events.jsonl"));
    let mut state = TuiState {
        transcript: state::TranscriptState {
            lines: items,
            ..Default::default()
        },
        ..Default::default()
    };
    let backend = TestBackend::new(80, 60);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut state, TuiMode::Full))
        .unwrap();
    let raw = screen_text(&terminal);
    let tight = raw.replace(' ', "");
    let _ = std::fs::remove_dir_all(&directory);

    // Internal rows are never echoed as user prose.
    for payload in [
        "<current-goal>",
        "<compacted-summary>",
        "<current-todos>",
        "<todo-sync",
        "<goal-draft>",
        "<goal-review>",
    ] {
        assert!(!raw.contains(payload), "{payload} leaked: {raw}");
    }
    // Human rows and guidance keep their distinct presentation.
    assert!(raw.contains("> please do the thing"), "{raw}");
    assert!(raw.contains("[Added to context] use the fixture"), "{raw}");

    // All three boundary kinds render as cards from the structured payload.
    assert!(tight.contains("目标草案讨论"), "{tight}");
    assert!(tight.contains("修改意见：addthefixture"), "{tight}");
    assert!(tight.contains("开始目标"), "{tight}");
    assert!(tight.contains("第1轮·契约r3"), "{tight}");
    assert!(tight.contains("继续执行目标"), "{tight}");
    assert!(tight.contains("第2轮·契约r4"), "{tight}");
    assert!(!tight.contains("第0轮"), "a draft has no round: {tight}");
    // The terminal facts row renders with the structured identity.
    assert!(raw.contains("Goal goal-1 ended (revision 9)"), "{raw}");

    let cards: Vec<_> = state
        .transcript
        .lines
        .iter()
        .filter(|item| item.kind == TranscriptKind::Goal)
        .collect();
    assert_eq!(cards.len(), 3, "one card per boundary row");
    let kinds: Vec<_> = cards
        .iter()
        .map(|item| item.goal_message.as_ref().expect("structured payload").kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            mink_goal::GoalMessageKind::Draft,
            mink_goal::GoalMessageKind::Start,
            mink_goal::GoalMessageKind::Continuation
        ]
    );
}

#[test]
fn status_bar_shows_completion_basis_even_with_remaining_turns() {
    let mut state = TuiState::default();
    let mut done = goal_display();
    done.status = "completed".into();
    done.phase = "idle".into();
    done.remaining_turns = 8;
    done.completion_basis = Some("verified".into());
    state.mirrors.goal = Some(done);
    let line = build_status_line(&state, 100);
    assert!(
        line.contains("goal:completed verified"),
        "early-completed goal must display completion basis: {line}"
    );
}

#[test]
fn details_panel_includes_goal_and_labels_collapsed_goal_card() {
    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            goal: Some(goal_display()),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut card = TranscriptItem::goal_card(mink_goal::GoalMessage {
        kind: mink_goal::GoalMessageKind::Start,
        goal_id: "g1".into(),
        contract_revision: 1,
        iteration: 1,
        objective: "ship".into(),
        acceptance_criteria: vec![],
        constraints: vec![],
        prior_decision: None,
        feedback: None,
    });
    card.collapsed = true;
    state.push_line(card);

    let actions = panels::detail_actions(&state);
    assert!(
        actions
            .iter()
            .any(|(label, _, action)| label == "Goal" && *action == state::ClickAction::OpenGoal),
        "Goal entry must be listed in /details: {actions:?}"
    );
    assert!(
        actions.iter().any(|(label, _, _)| label == "Expand goal"),
        "collapsed goal card must be labeled 'Expand goal', not thinking: {actions:?}"
    );
}
