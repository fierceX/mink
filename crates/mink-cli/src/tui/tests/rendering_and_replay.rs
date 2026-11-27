use super::*;

#[test]
fn tui_display_detail_uses_full_tool_result_content() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let display = TuiDisplay::new(tx);

    display.render_tool_result(&crate::ui::PresentedToolResultDisplay {
        base: ToolResultDisplay {
            tool_name: "Bash",
            content_preview: "short preview\n",
            content: "full output\nwith more detail",
            tool_use_id: Some("toolu_1"),
            exit_code: Some(0),
        },
        status: crate::runtime::ToolStatus::Succeeded,
        result_kind: crate::ui::ToolResultKind::Text,
        presentation: None,
        artifacts: &[],
    });

    match rx.try_recv().unwrap() {
        TuiSignal::ToolResult {
            tool_name, content, ..
        } => {
            assert_eq!(tool_name, "Bash");
            assert_eq!(content, "full output\nwith more detail");
        }
        other => panic!("unexpected signal: {other:?}"),
    }
}

#[test]
fn load_session_replays_recent_turns() {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "mink_tui_events_{}_{}_{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let events = [
        serde_json::json!({"type":"user_input","content":"hello\nsecond line"}),
        serde_json::json!({"type":"thinking","content":"thinking"}),
        serde_json::json!({"type":"text","content":"answer"}),
        serde_json::json!({"type":"tool_call","id":"call-1","name":"Read","input":{"file_path":"src/tui/mod.rs"}}),
        serde_json::json!({
            "type":"tool_result",
            "tool_use_id":"call-1",
            "name":"Read",
            "content":"file contents",
            "success":true,
            "result_kind":"text"
        }),
    ];
    let data = events
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, data).unwrap();

    let lines = load_session(&path);
    let _ = std::fs::remove_file(path);

    assert!(lines.iter().any(|line| line.text == "> hello"));
    assert!(
        lines
            .iter()
            .any(|line| line.kind == TranscriptKind::StreamThinking)
    );
    assert!(
        lines
            .iter()
            .any(|line| line.kind == TranscriptKind::StreamText)
    );
    let tool = lines
        .iter()
        .find(|line| line.kind == TranscriptKind::Tool)
        .unwrap();
    assert!(tool.sealed);
    assert_eq!(tool.tool_status, None);
    assert!(tool.text.contains("file contents"));
}

#[test]
fn apply_switches_stream_kinds_and_collapses_thinking() {
    let mut state = TuiState::default();

    state.apply(TuiSignal::Thinking("think".into()));
    state.apply(TuiSignal::Text("answer".into()));

    assert_eq!(state.transcript.lines.len(), 1);
    assert_eq!(
        state.transcript.lines[0].kind,
        TranscriptKind::StreamThinking
    );
    assert!(state.transcript.lines[0].collapsed);
    assert_eq!(state.stream.stream_kind, TranscriptKind::StreamText);
    assert_eq!(state.stream.stream_line, "answer");
    assert!(state.stream.streaming);
    assert_eq!(state.stream.work_state, WorkState::StreamingText);
}

#[test]
fn sub_agent_meta_label_and_stop_reason_are_visible() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::SubAgentStatus {
        session_id: "sub_1".into(),
        status: "running".into(),
        label: Some("review diff".into()),
        stop_reason: None,
        in_tokens: 0,
        out_tokens: 0,
    });
    assert!(state.transcript.lines[0].text.contains("review diff"));
    state.apply(TuiSignal::SubAgentStatus {
        session_id: "sub_1".into(),
        status: "ok".into(),
        label: Some("review diff".into()),
        stop_reason: Some("completed".into()),
        in_tokens: 4,
        out_tokens: 2,
    });
    assert!(state.transcript.lines[0].text.contains("review diff"));
    assert!(state.transcript.lines[0].text.contains("[completed]"));
}

#[test]
fn sub_agent_output_invalidates_updated_line_cache() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::SubAgentStatus {
        session_id: "sub_1".into(),
        status: "launched".into(),
        label: None,
        stop_reason: None,
        in_tokens: 0,
        out_tokens: 0,
    });
    state.transcript.lines[0].cached_lines = Some(vec![Line::from("stale")]);
    state.cache.rebuild_next = None;

    state.apply(TuiSignal::SubAgentOutput {
        session_id: "sub_1".into(),
        status: "ok".into(),
        label: None,
        stop_reason: None,
        thinking: "child thinking".into(),
        text: "child text".into(),
        in_tokens: 12,
        out_tokens: 34,
    });

    assert!(state.transcript.lines[0].text.contains("ok"));
    assert!(state.transcript.lines[0].cached_lines.is_none());
    assert!(state.cache.pending.contains(&0));
    let detail = state.transcript.lines[0].sub_detail.as_ref().unwrap();
    assert_eq!(detail.thinking, "child thinking");
    assert_eq!(detail.text, "child text");
}

#[test]
fn sub_agent_status_updates_existing_line() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::SubAgentStatus {
        session_id: "sub_1".into(),
        status: "launched".into(),
        label: None,
        stop_reason: None,
        in_tokens: 0,
        out_tokens: 0,
    });
    state.transcript.lines[0].cached_lines = Some(vec![Line::from("stale")]);
    state.cache.rebuild_next = None;

    state.apply(TuiSignal::SubAgentStatus {
        session_id: "sub_1".into(),
        status: "running".into(),
        label: None,
        stop_reason: None,
        in_tokens: 3,
        out_tokens: 4,
    });

    assert_eq!(state.transcript.lines.len(), 1);
    assert_eq!(state.transcript.sub_agents.active_sessions.len(), 1);
    assert!(state.transcript.lines[0].text.contains("running"));
    assert!(state.transcript.lines[0].cached_lines.is_none());
    assert!(state.cache.pending.contains(&0));
}

#[test]
fn sub_agent_terminal_status_clears_active_state() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::SubAgentStatus {
        session_id: "sub_1".into(),
        status: "launched".into(),
        label: None,
        stop_reason: None,
        in_tokens: 0,
        out_tokens: 0,
    });

    state.apply(TuiSignal::SubAgentStatus {
        session_id: "sub_1".into(),
        status: "timed_out".into(),
        label: None,
        stop_reason: None,
        in_tokens: 0,
        out_tokens: 0,
    });

    assert!(state.transcript.sub_agents.active_sessions.is_empty());
    assert_eq!(state.stream.work_state, WorkState::WaitingModel);
    assert!(state.transcript.lines[0].text.contains("timed_out"));
}

#[test]
fn user_task_stop_emits_completion_notification() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            model: "pro".into(),
            ..Default::default()
        },
        ..TuiState::default()
    };
    state.input.buf = "do the task".into();
    state.input.cursor = state.input.buf.len();

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));
    assert!(state.take_task_notification().is_none());

    state.apply(TuiSignal::Stop);

    let notification = state.take_task_notification().unwrap();
    assert_eq!(notification.kind, TaskNotificationKind::Completed);
    assert_eq!(notification.title, "mink 任务完成");
    assert!(notification.body.contains("pro"));
    assert!(state.take_task_notification().is_none());
}

#[test]
fn user_task_error_emits_failure_notification() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "do the task".into();
    state.input.cursor = state.input.buf.len();

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    state.apply(TuiSignal::Error("network timeout".into()));

    let notification = state.take_task_notification().unwrap();
    assert_eq!(notification.kind, TaskNotificationKind::Failed);
    assert_eq!(notification.title, "mink 任务失败");
}

#[test]
fn local_help_does_not_emit_completion_notification() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "/help".into();
    state.input.cursor = state.input.buf.len();

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));

    state.apply(TuiSignal::Stop);

    assert!(state.take_task_notification().is_none());
}

#[test]
fn compact_stop_emits_completion_notification() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut state = TuiState::default();
    state.input.buf = "/compact".into();
    state.input.cursor = state.input.buf.len();

    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut state,
        &tx,
    ));
    assert_eq!(state.stream.work_state, WorkState::Compacting);

    state.apply(TuiSignal::Stop);

    let notification = state.take_task_notification().unwrap();
    assert_eq!(notification.kind, TaskNotificationKind::Completed);
}

#[test]
fn compact_error_then_stop_is_visible_and_restores_input() {
    let mut state = TuiState {
        stream: state::StreamState {
            work_state: WorkState::Compacting,
            ..Default::default()
        },
        ..TuiState::default()
    };
    state.apply(TuiSignal::Error(
        "Compact failed: compaction interrupted".into(),
    ));
    state.apply(TuiSignal::Stop);

    assert_eq!(state.stream.work_state, WorkState::Idle);
    assert!(state.transcript.lines.iter().any(|line| {
        line.kind == TranscriptKind::Error && line.text.contains("compaction interrupted")
    }));
}

#[test]
fn duplicate_sub_agent_output_does_not_drift_active_state() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::SubAgentStatus {
        session_id: "sub_1".into(),
        status: "launched".into(),
        label: None,
        stop_reason: None,
        in_tokens: 0,
        out_tokens: 0,
    });

    let output = TuiSignal::SubAgentOutput {
        session_id: "sub_1".into(),
        status: "ok".into(),
        label: None,
        stop_reason: None,
        thinking: String::new(),
        text: "done".into(),
        in_tokens: 1,
        out_tokens: 2,
    };
    state.apply(output.clone());
    state.apply(output);

    assert!(state.transcript.sub_agents.active_sessions.is_empty());
    assert_eq!(state.stream.work_state, WorkState::WaitingModel);
    assert_eq!(state.transcript.lines.len(), 1);
}

#[test]
fn sub_agent_output_initializes_sub_detail_when_missing() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::SubAgentStatus {
        session_id: "sub_missing_detail".into(),
        status: "ok".into(),
        label: Some("worker".into()),
        stop_reason: None,
        in_tokens: 0,
        out_tokens: 0,
    });
    assert!(state.transcript.lines[0].sub_detail.is_none());

    state.apply(TuiSignal::SubAgentOutput {
        session_id: "sub_missing_detail".into(),
        status: "ok".into(),
        label: Some("worker".into()),
        stop_reason: None,
        thinking: "trace".into(),
        text: "final answer".into(),
        in_tokens: 10,
        out_tokens: 20,
    });
    let detail = state.transcript.lines[0]
        .sub_detail
        .as_ref()
        .expect("sub_detail must be initialized by SubAgentOutput");
    assert_eq!(detail.thinking, "trace");
    assert_eq!(detail.text, "final answer");
}

#[test]
fn content_viewports_leave_light_padding_when_space_allows() {
    assert_eq!(content_viewport_height(10), 9);
    assert_eq!(content_viewport_height(2), 1);
    assert_eq!(content_viewport_height(1), 1);
    assert_eq!(detail_viewport_height(10), 9);
    assert_eq!(detail_viewport_height(2), 1);
    assert_eq!(detail_viewport_height(1), 1);
}

#[test]
fn short_tool_result_does_not_default_to_collapsed() {
    let line = TranscriptItem::new("short\noutput".into(), TranscriptKind::Tool);

    assert!(!line.collapsed);
    assert_eq!(
        line.collapse_policy,
        CollapsePolicy::Auto {
            threshold_lines: 20
        }
    );
}

#[test]
fn collapsed_summary_includes_tool_result_line_count() {
    let text = (0..25)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let line = TranscriptItem::new_tool_result("Bash".into(), text);

    let summary = collapsed_summary(&line, 80);

    assert!(summary.contains("Bash: 25 lines"));
    assert!(summary.contains("bytes"));
    assert!(summary.contains("line 0"));
}

#[test]
fn collapsed_summary_marks_truncated_tool_result() {
    let line = TranscriptItem::new_tool_result(
        "Read".into(),
        "first\n\n[... truncated: showing first/last portions of 10000 bytes ...]\nlast".into(),
    );

    let summary = collapsed_summary(&line, 120);

    assert!(summary.contains("Read: 4 lines"));
    assert!(summary.contains("truncated"));
    assert!(summary.contains("first"));
}

#[test]
fn tool_result_signal_preserves_tool_name_for_summaries() {
    let mut state = TuiState::default();

    state.apply(TuiSignal::ToolResult {
        tool_use_id: None,
        tool_name: "Edit".into(),
        content: "changed file".into(),
        status: Some(crate::runtime::ToolStatus::Succeeded),
        exit_code: None,
        result_kind: crate::ui::ToolResultKind::Edit,
        presentation: None,
        artifacts: Vec::new(),
    });

    assert_eq!(state.transcript.lines.len(), 1);
    assert_eq!(state.transcript.lines[0].tool_name.as_deref(), Some("Edit"));
    assert_eq!(state.transcript.lines[0].kind, TranscriptKind::Tool);
}

#[test]
fn structured_tool_result_updates_the_running_transcript_item() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::ToolCall {
        tool_use_id: Some("call-1".into()),
        tool_name: "PlanDraft".into(),
        summary: "PlanDraft".into(),
        input: None,
    });
    assert_eq!(state.transcript.lines.len(), 1);
    assert!(!state.transcript.lines[0].sealed);

    let presentation = crate::ui::ToolPresentation::Plan(crate::ui::PlanDisplay {
        transition: crate::ui::PlanTransitionDisplay::DraftSaved,
        content: Some("1. inspect\n2. implement".into()),
    });
    state.apply(TuiSignal::ToolResult {
        tool_use_id: Some("call-1".into()),
        tool_name: "PlanDraft".into(),
        content: "Plan draft saved.".into(),
        status: Some(crate::runtime::ToolStatus::Succeeded),
        exit_code: None,
        result_kind: crate::ui::ToolResultKind::Control,
        presentation: Some(presentation),
        artifacts: Vec::new(),
    });

    assert_eq!(state.transcript.lines.len(), 1);
    assert!(state.transcript.lines[0].sealed);
    assert_eq!(
        state.transcript.lines[0].tool_status,
        Some(crate::runtime::ToolStatus::Succeeded)
    );
    assert!(state.mirrors.plan.is_some());
}

#[test]
fn tool_result_with_mismatched_id_does_not_merge_by_tool_name() {
    use crate::ui::{TodoChangeDisplay, TodoCountsDisplay, TodoDisplay, ToolPresentation};

    let mut state = TuiState::default();
    state.apply(TuiSignal::ToolCall {
        tool_use_id: Some("call-1".into()),
        tool_name: "TodoAdvance".into(),
        summary: "TodoAdvance(2 transitions @r1)".into(),
        input: None,
    });
    let presentation = ToolPresentation::Todo(TodoDisplay {
        revision: 2,
        counts: TodoCountsDisplay {
            pending: 2,
            in_progress: 2,
            completed: 0,
        },
        items: Vec::new(),
        changes: vec![TodoChangeDisplay::Activated { id: "T0001".into() }],
    });
    state.apply(TuiSignal::ToolResult {
        tool_use_id: Some("provider-call-1".into()),
        tool_name: "TodoAdvance".into(),
        content: "<todo-event>raw protocol</todo-event>".into(),
        status: Some(crate::runtime::ToolStatus::Succeeded),
        exit_code: None,
        result_kind: ToolResultKind::Control,
        presentation: Some(presentation),
        artifacts: Vec::new(),
    });

    assert_eq!(state.transcript.lines.len(), 2);
    assert!(!state.transcript.lines[0].sealed);
    assert_eq!(
        state.transcript.lines[0].tool_use_id.as_deref(),
        Some("call-1")
    );
    assert!(state.transcript.lines[1].sealed);
    assert_eq!(
        state.transcript.lines[1].tool_use_id.as_deref(),
        Some("provider-call-1")
    );
    let rendered = render::transcript_item_lines(&state.transcript.lines[1], 80)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Todos r2"));
    assert!(rendered.contains("activated T0001"));
    assert!(!rendered.contains("<todo-event>"));
    assert_eq!(state.mirrors.todos.as_ref().unwrap().revision, 2);
}

#[test]
fn reused_tool_id_updates_the_latest_unsealed_call() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::ToolCall {
        tool_use_id: Some("call-1".into()),
        tool_name: "Read".into(),
        summary: "Read(old)".into(),
        input: None,
    });
    state.apply(TuiSignal::ToolResult {
        tool_use_id: Some("call-1".into()),
        tool_name: "Read".into(),
        content: "old result".into(),
        status: Some(crate::runtime::ToolStatus::Succeeded),
        exit_code: None,
        result_kind: ToolResultKind::FileRead,
        presentation: None,
        artifacts: Vec::new(),
    });
    state.transcript.inline.committed = 1;
    state.apply(TuiSignal::ToolCall {
        tool_use_id: Some("call-1".into()),
        tool_name: "Read".into(),
        summary: "Read(new)".into(),
        input: None,
    });
    state.apply(TuiSignal::ToolResult {
        tool_use_id: Some("call-1".into()),
        tool_name: "Read".into(),
        content: "new result".into(),
        status: Some(crate::runtime::ToolStatus::Succeeded),
        exit_code: None,
        result_kind: ToolResultKind::FileRead,
        presentation: None,
        artifacts: Vec::new(),
    });

    assert_eq!(state.transcript.lines.len(), 2);
    assert_eq!(state.transcript.lines[0].text, "old result");
    assert_eq!(state.transcript.lines[1].text, "new result");
    assert!(state.transcript.lines[1].sealed);
}

#[test]
fn collapsed_artifact_card_keeps_the_artifact_id_visible() {
    let mut item = TranscriptItem::new_tool_result("Bash".into(), "large output".into());
    item.collapsed = true;
    item.artifacts = vec![crate::ui::ArtifactDisplay {
        id: "bash-0001".into(),
        tool: "Bash".into(),
        bytes: 100_000,
        description: "full tool output".into(),
    }];

    let rendered = render::transcript_item_lines(&item, 80)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(rendered.contains("artifact://bash-0001"));
}

#[test]
fn todo_delta_preserves_items_not_mentioned_by_the_update() {
    use crate::ui::{
        TodoChangeDisplay, TodoCountsDisplay, TodoDisplay, TodoItemDisplay, TodoStatusDisplay,
    };

    let mut state = TuiState {
        mirrors: state::SessionMirrors {
            todos: Some(TodoDisplay {
                revision: 1,
                counts: TodoCountsDisplay {
                    pending: 2,
                    in_progress: 0,
                    completed: 0,
                },
                items: vec![
                    TodoItemDisplay {
                        id: "todo-1".into(),
                        content: "first".into(),
                        status: TodoStatusDisplay::Pending,
                    },
                    TodoItemDisplay {
                        id: "todo-2".into(),
                        content: "second".into(),
                        status: TodoStatusDisplay::Pending,
                    },
                ],
                changes: Vec::new(),
            }),
            ..Default::default()
        },
        ..Default::default()
    };

    state.apply_todo_presentation(&TodoDisplay {
        revision: 2,
        counts: TodoCountsDisplay {
            pending: 1,
            in_progress: 1,
            completed: 0,
        },
        items: vec![TodoItemDisplay {
            id: "todo-1".into(),
            content: "first".into(),
            status: TodoStatusDisplay::InProgress,
        }],
        changes: vec![TodoChangeDisplay::Activated {
            id: "todo-1".into(),
        }],
    });

    let todos = state.mirrors.todos.unwrap();
    assert_eq!(todos.items.len(), 2);
    assert_eq!(todos.items[0].status, TodoStatusDisplay::InProgress);
    assert_eq!(todos.items[1].content, "second");
    assert_eq!(todos.revision, 2);
}

#[test]
fn shared_tool_card_renderer_colors_tools_by_result_kind() {
    let mut item = TranscriptItem::new_tool_result("Edit".into(), "updated file".into());
    item.tool_status = Some(crate::runtime::ToolStatus::Succeeded);
    item.tool_result_kind = Some(ToolResultKind::Edit);

    let lines = render::transcript_item_lines(&item, 80);

    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
    assert_eq!(lines[0].spans[1].content.as_ref(), "Edit");
    assert_eq!(lines[0].spans[1].style.fg, Some(Color::Yellow));
}

#[test]
fn inline_tool_projection_auto_collapses_without_expand_marker() {
    let mut item = TranscriptItem::new_tool_result(
        "Bash".into(),
        (0..30)
            .map(|line| format!("output {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    item.tool_status = Some(crate::runtime::ToolStatus::Succeeded);
    item.tool_result_kind = Some(ToolResultKind::Command);

    let lines = render::transcript_item_lines(&item, 80);

    assert_eq!(lines.len(), 1);
    assert!(!line_text(&lines[0]).starts_with('▶'));
    assert!(line_text(&lines[0]).contains("Bash"));
}

#[test]
fn full_tui_restores_mouse_toggle_for_collapsible_cards() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let mut item = TranscriptItem::new_tool_result(
        "Bash".into(),
        (0..30)
            .map(|line| format!("output {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    item.tool_status = Some(crate::runtime::ToolStatus::Succeeded);
    item.tool_result_kind = Some(ToolResultKind::Command);
    let mut state = TuiState::default();
    state.push_line(item);
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| render(frame, &mut state, TuiMode::Full))
        .unwrap();
    assert!(state.transcript.lines[0].collapsed);
    // Scrolling back into an evicted region must be clickable on its first frame.
    state.transcript.lines[0].cached_lines = None;
    terminal
        .draw(|frame| render(frame, &mut state, TuiMode::Full))
        .unwrap();
    assert!(!state.viewport.click_map.is_empty());
    let row = state.viewport.content_y + state.viewport.click_map[0].start_row as u16;

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

    assert!(!state.transcript.lines[0].collapsed);
    assert!(state.transcript.lines[0].collapse_overridden);
}

#[test]
fn info_during_streaming_non_heartbeat_is_deferred_until_stream_ends() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::Text("first".into()));
    state.apply(TuiSignal::Info("Retrying (1/3)...".into()));
    assert!(state.stream.streaming);
    assert_eq!(state.transcript.lines.len(), 0);
    assert_eq!(
        state.transcript.pending_infos.as_slice(),
        &["Retrying (1/3)...".to_string()]
    );
    state.apply(TuiSignal::Text("second".into()));
    state.apply(TuiSignal::Stop);
    assert_eq!(state.transcript.lines.len(), 2);
    assert_eq!(state.transcript.lines[0].kind, TranscriptKind::StreamText);
    assert_eq!(state.transcript.lines[1].kind, TranscriptKind::Info);
    assert_eq!(state.transcript.lines[1].text, "Retrying (1/3)...");
}

#[test]
fn committed_boundary_stops_at_the_first_live_item() {
    let mut state = TuiState::default();
    state.push_line(TranscriptItem::new("sealed".into(), TranscriptKind::Text));
    state.push_line(TranscriptItem::new_tool_call(
        Some("call-1".into()),
        "Bash".into(),
        "Bash(test)".into(),
        None,
    ));
    state.push_line(TranscriptItem::new("later".into(), TranscriptKind::Text));

    assert_eq!(sealed_prefix_end(&state), 1);
    state.transcript.lines[1].sealed = true;
    assert_eq!(sealed_prefix_end(&state), 3);
    state.transcript.inline.committed = 2;
    assert_eq!(sealed_prefix_end(&state), 3);
}

#[test]
fn tool_call_headers_sanitize_name_and_summary_and_keep_input() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::ToolCall {
        tool_use_id: Some("call-1".into()),
        tool_name: "Ba\x1b]0;pwned\x07sh".into(),
        summary: "cmd \x1b[31mred".into(),
        input: Some(serde_json::json!({"command": "ls"})),
    });

    let item = &state.transcript.lines[0];
    assert_eq!(item.tool_name.as_deref(), Some("Bash"));
    assert_eq!(item.tool_summary.as_deref(), Some("cmd red"));
    assert_eq!(item.tool_input, Some(serde_json::json!({"command": "ls"})));
    assert!(!item.text.contains("pwned"));
}

#[test]
fn terminal_signal_seals_orphan_tool_calls() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::ToolCall {
        tool_use_id: Some("call-1".into()),
        tool_name: "Bash".into(),
        summary: "Bash(test)".into(),
        input: None,
    });

    state.apply(TuiSignal::Error("connection failed".into()));

    assert!(state.transcript.lines[0].sealed);
    assert_eq!(state.transcript.lines[0].tool_status, None);
    assert!(state.transcript.lines[0].text.contains("turn failed"));
    assert_eq!(sealed_prefix_end(&state), state.transcript.lines.len());
}

#[test]
fn commit_ready_moves_only_the_sealed_prefix_into_native_history() {
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Inline(4),
        },
    )
    .unwrap();
    let mut state = TuiState::default();
    state.push_line(TranscriptItem::new(
        "completed answer".into(),
        TranscriptKind::Text,
    ));
    state.push_line(TranscriptItem::new_tool_call(
        Some("call-1".into()),
        "Bash".into(),
        "Bash(test)".into(),
        None,
    ));

    assert!(commit_ready(&mut terminal, &mut state).unwrap());
    assert_eq!(state.transcript.inline.committed, 1);
    assert!(!commit_ready(&mut terminal, &mut state).unwrap());

    state.transcript.lines[1].sealed = true;
    state.stream.work_state = WorkState::WaitingModel;
    assert!(commit_ready(&mut terminal, &mut state).unwrap());
    assert_eq!(state.transcript.inline.committed, 2);
}

#[test]
fn inline_keeps_the_final_item_in_the_viewport_until_new_work_starts() {
    let mut state = TuiState::default();
    state.push_line(TranscriptItem::new(
        "final answer".into(),
        TranscriptKind::StreamText,
    ));

    assert_eq!(committable_prefix_end(&state), 0);

    state.stream.work_state = WorkState::WaitingModel;
    assert_eq!(committable_prefix_end(&state), 1);
}

#[test]
fn sub_agent_detail_tracks_session_after_output_update() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::SubAgentStatus {
        session_id: "sub_1".into(),
        status: "launched".into(),
        label: None,
        stop_reason: None,
        in_tokens: 0,
        out_tokens: 0,
    });
    state.view = View::SubAgentDetail {
        session_id: "sub_1".into(),
        scroll: 0,
    };

    state.apply(TuiSignal::SubAgentOutput {
        session_id: "sub_1".into(),
        status: "ok".into(),
        label: None,
        stop_reason: None,
        thinking: "child thinking".into(),
        text: "child text".into(),
        in_tokens: 12,
        out_tokens: 34,
    });

    assert!(matches!(
        state.view,
        View::SubAgentDetail {
            ref session_id,
            scroll: 0
        } if session_id == "sub_1"
    ));
    let detail = detail_lines_for_session(&state, "sub_1");
    let text: Vec<String> = detail.iter().map(line_text).collect();
    assert!(text.iter().any(|line| line.contains("ok")));
    assert!(text.iter().any(|line| line == "child thinking"));
    assert!(text.iter().any(|line| line == "child text"));
}

#[test]
fn sub_agent_detail_missing_session_renders_fallback() {
    let state = TuiState::default();

    let detail = detail_lines_for_session(&state, "missing");

    assert_eq!(detail.len(), 1);
    assert!(line_text(&detail[0]).contains("missing"));
}

#[test]
fn visible_lines_only_clones_requested_viewport_across_history_and_stream() {
    let history = vec![Line::from("h0"), Line::from("h1"), Line::from("h2")];
    let stream = vec![Line::from("s0"), Line::from("s1")];

    let lines = visible_lines(&history, &stream, 2, 3);

    let text: Vec<String> = lines.iter().map(line_text).collect();
    assert_eq!(text, vec!["h2", "s0", "s1"]);
}

#[test]
fn render_clamps_invalid_cursor_and_repairs_missing_cache() {
    let mut state = TuiState::default();
    state.input.buf = "a中b".into();
    state.input.cursor = 2;
    state
        .transcript
        .lines
        .push(TranscriptItem::new("first".into(), TranscriptKind::Text));
    state
        .transcript
        .lines
        .push(TranscriptItem::new("second".into(), TranscriptKind::Text));
    state.cache.width = 78;
    state.cache.rebuild_next = None;
    state.transcript.lines[0].cached_lines = Some(vec![Line::from("first")]);
    state.transcript.lines[0].cached_collapsed = state.transcript.lines[0].collapsed;

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| render(f, &mut state, TuiMode::Full))
        .unwrap();

    assert_eq!(state.input.cursor, "a".len());
    assert!(state.transcript.lines[1].cached_lines.is_some());
    assert!(state.cache.heights.total() >= 2);
}

#[test]
fn tui_display_preserves_tool_call_and_title_fields() {
    use crate::ui::{StatsSnapshot, ToolCallDisplay};

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let display = TuiDisplay::new(tx);

    let input = serde_json::json!({"path": "a.rs"});
    display.render_tool_call(&ToolCallDisplay {
        tool_use_id: "call_9",
        tool_name: "Read",
        summary: "Read a.rs",
        input: Some(&input),
    });
    match rx.try_recv().unwrap() {
        TuiSignal::ToolCall {
            tool_use_id,
            tool_name,
            summary,
            ..
        } => {
            assert_eq!(tool_use_id.as_deref(), Some("call_9"));
            assert_eq!(tool_name, "Read");
            assert_eq!(summary, "Read a.rs");
        }
        other => panic!("unexpected signal: {other:?}"),
    }

    let stats = StatsSnapshot {
        current_turn_count: 3,
        agent_request_count: 5,
        total_input_tokens: 100,
        total_output_tokens: 40,
        current_context_tokens: 1200,
        max_context_tokens: 64_000,
        total_cache_read_tokens: 20,
        total_cache_creation_tokens: 10,
        belief: 0.75,
    };
    display.render_title_update("flash", &stats);
    match rx.try_recv().unwrap() {
        TuiSignal::TitleUpdate(model, delivered) => {
            assert_eq!(model, "flash");
            assert_eq!(delivered.current_turn_count, stats.current_turn_count);
            assert_eq!(delivered.agent_request_count, stats.agent_request_count);
            assert_eq!(delivered.total_input_tokens, stats.total_input_tokens);
            assert_eq!(delivered.total_output_tokens, stats.total_output_tokens);
            assert_eq!(
                delivered.current_context_tokens,
                stats.current_context_tokens
            );
            assert_eq!(delivered.max_context_tokens, stats.max_context_tokens);
            assert_eq!(delivered.belief, stats.belief);
        }
        other => panic!("unexpected signal: {other:?}"),
    }
}

#[test]
fn streaming_text_strips_ansi_escape_split_across_chunks() {
    let mut state = TuiState::default();
    state.apply(TuiSignal::Text("hello \x1b[".into()));
    state.apply(TuiSignal::Text("31mworld\x1b[0m!".into()));
    state.finalize_stream();
    let combined: String = state
        .transcript
        .lines
        .iter()
        .map(|l| l.text.as_str())
        .collect();
    assert_eq!(combined, "hello world!");
}
