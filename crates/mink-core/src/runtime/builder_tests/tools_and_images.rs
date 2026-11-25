use super::*;

#[tokio::test]
async fn runtime_event_sink_observes_existing_display_path() {
    let home = unique_temp_dir("event-home");
    let cwd = unique_temp_dir("event-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let sink = Arc::new(RecordingSink::default());
    let cfg = Config {
        log_events: true,
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
            .with_event_sink(sink.clone()),
    )
    .await
    .unwrap();

    runtime.compact().await.unwrap();
    runtime.shutdown().await.unwrap();

    {
        let events = sink.events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent { kind: crate::runtime::AgentEventKind::Info { message }, .. } if message == "Compressing..."
            )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, crate::runtime::AgentEventKind::Stop { .. }))
        );
    }

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn tool_result_events_carry_presentation_and_artifacts() {
    use crate::protocol::{Event, StopEvent, TextEvent};
    use serde_json::json;
    let home = unique_temp_dir("b3-home");
    let cwd = unique_temp_dir("b3-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let sink = Arc::new(RecordingSink::default());

    let mock = crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::ToolCall(tool_call_event(
                    "TodoWrite",
                    "call_todo",
                    json!({"base_revision":0,"add":[{"content":"first task"}]}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "todo done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            vec![
                Ok(Event::ToolCall(tool_call_event(
                    "Bash",
                    "call_bash",
                    json!({"command":"yes x | head -c 150000"}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "bash done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    );
    let runtime_config = runtime_config_with_mock(&home, &cwd, mock).with_event_sink(sink.clone());
    let runtime = build_runtime(runtime_config).await.unwrap();

    runtime.run_turn("add a todo").await.unwrap();
    runtime.run_turn("produce a big output").await.unwrap();
    runtime.shutdown().await.unwrap();

    {
        let events = sink.events.lock().unwrap();
        let todo_presentations: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                crate::runtime::AgentEventKind::ToolResult {
                    tool_name,
                    presentation,
                    ..
                } if tool_name == "TodoWrite" => Some(presentation.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(todo_presentations.len(), 1, "exactly one TodoWrite result");
        assert!(
            matches!(
                todo_presentations[0],
                Some(crate::ui::ToolPresentation::Todo(_))
            ),
            "TodoWrite ToolResult event must carry its structured presentation"
        );

        let bash_artifacts: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                crate::runtime::AgentEventKind::ToolResult {
                    tool_name,
                    artifacts,
                    ..
                } if tool_name == "Bash" => Some(artifacts.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(bash_artifacts.len(), 1, "exactly one Bash result");
        assert!(
            !bash_artifacts[0].is_empty(),
            "oversized Bash result must carry artifact references in the event"
        );
    }

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn registered_async_custom_tool_executes_through_the_core_pipeline() {
    let home = unique_temp_dir("custom-tool-home");
    let cwd = unique_temp_dir("custom-tool-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_custom_tool_use());
    config.custom_tools.push(Arc::new(AsyncEchoTool));
    let runtime = build_runtime(config).await.unwrap();

    let mut stream = runtime.stream_turn("echo").unwrap();
    let turn_id = stream.turn_id().clone();
    let mut sequences = Vec::new();
    let mut result = None;
    while let Some(event) = stream.recv().await {
        assert_eq!(event.turn_id.as_ref(), Some(&turn_id));
        sequences.push(event.sequence);
        if let crate::runtime::AgentEventKind::ToolResult {
            tool_use_id,
            tool_name,
            content,
            status,
            result_kind,
            ..
        } = event.kind
        {
            result = Some((tool_use_id, tool_name, content, status, result_kind));
        }
    }
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
    let outcome = stream.outcome().await.unwrap();
    assert!(outcome.text.contains("custom done"));
    let (tool_use_id, tool_name, content, status, result_kind) = result.unwrap();
    assert_eq!(tool_use_id.as_deref(), Some("call_echo_1"));
    assert_eq!(tool_name, "AsyncEcho");
    assert!(content.contains("echo:hello@"));
    assert!(status.is_success());
    assert_eq!(result_kind, crate::tools::metadata::ToolResultKind::Text);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn custom_tool_definition_is_evaluated_once_and_frozen() {
    let home = unique_temp_dir("custom-frozen-home");
    let cwd = unique_temp_dir("custom-frozen-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_single_tool("FrozenTool"));
    config.custom_tools.push(Arc::new(DriftingDefinitionTool {
        calls: calls.clone(),
    }));

    let runtime = build_runtime(config).await.unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let outcome = runtime.run_turn("use frozen definition").await.unwrap();
    assert_eq!(outcome.tool_call_count, 1);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn successful_mutating_custom_tool_invalidates_read_memos() {
    let home = unique_temp_dir("custom-mutating-home");
    let cwd = unique_temp_dir("custom-mutating-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_single_tool("MutatingEcho"));
    config.custom_tools.push(Arc::new(MutatingEchoTool));
    let runtime = build_runtime(config).await.unwrap();

    assert_eq!(
        runtime
            .ctx
            .tools
            .memo_mutation
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    runtime.run_turn("mutate").await.unwrap();
    assert_eq!(
        runtime
            .ctx
            .tools
            .memo_mutation
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn async_custom_tool_observes_stream_cancellation() {
    let home = unique_temp_dir("custom-cancel-home");
    let cwd = unique_temp_dir("custom-cancel-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_single_tool("Cancellable"));
    config.custom_tools.push(Arc::new(CancellableTool));
    let runtime = build_runtime(config).await.unwrap();

    let stream = runtime.stream_turn("wait").unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
    stream.cancel();
    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(2), stream.outcome())
        .await
        .expect("custom tool did not stop after cancellation")
        .unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn non_cooperative_custom_tool_is_dropped_on_interrupt() {
    let home = unique_temp_dir("custom-non-cooperative-home");
    let cwd = unique_temp_dir("custom-non-cooperative-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_single_tool("NonCooperative"));
    config.custom_tools.push(Arc::new(NonCooperativeTool));
    let runtime = build_runtime(config).await.unwrap();

    let stream = runtime.stream_turn("wait").unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(40)).await;
    stream.cancel();
    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(2), stream.outcome())
        .await
        .expect("non-cooperative custom tool was not dropped")
        .unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn custom_tool_timeout_is_local_and_next_turn_still_runs() {
    let home = unique_temp_dir("custom-timeout-home");
    let cwd = unique_temp_dir("custom-timeout-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut config = runtime_config_with_mock(
        &home,
        &cwd,
        crate::llm::mock::MockLlmBackend::new(
            "flash",
            vec![
                vec![
                    Ok(crate::protocol::Event::ToolCall(
                        crate::protocol::ToolCallEvent {
                            name: "NonCooperative".into(),
                            id: "call-timeout".into(),
                            input_json: serde_json::json!({}),
                            fields: Default::default(),
                            parse_error: None,
                            raw_arguments_digest: None,
                        },
                    )),
                    Ok(crate::protocol::Event::Stop(crate::protocol::StopEvent {
                        reason: "tool_use".into(),
                    })),
                ],
                vec![
                    Ok(crate::protocol::Event::Text(crate::protocol::TextEvent {
                        content: "recovered from tool timeout".into(),
                    })),
                    Ok(crate::protocol::Event::Stop(crate::protocol::StopEvent {
                        reason: "end_turn".into(),
                    })),
                ],
                vec![
                    Ok(crate::protocol::Event::Text(crate::protocol::TextEvent {
                        content: "next turn".into(),
                    })),
                    Ok(crate::protocol::Event::Stop(crate::protocol::StopEvent {
                        reason: "end_turn".into(),
                    })),
                ],
            ],
        ),
    );
    config.config.tool_timeout_secs = 5;
    config.custom_tools.push(Arc::new(NonCooperativeTool));
    let runtime = build_runtime(config).await.unwrap();

    let mut stream = runtime.stream_turn("timeout tool").unwrap();
    let mut timeout_result = None;
    while let Some(event) = stream.recv().await {
        if let crate::runtime::AgentEventKind::ToolResult {
            content, status, ..
        } = event.kind
        {
            timeout_result = Some((content, status));
        }
    }
    let timed_out = stream.outcome().await.unwrap();
    assert_eq!(timed_out.status, crate::agent::orchestrator::TurnStatus::Ok);
    let (content, status) = timeout_result.expect("timeout tool result");
    assert!(!status.is_success());
    assert!(content.contains("timed out after 5s"), "{content}");
    let next = runtime.run_turn("next").await.unwrap();
    assert_eq!(next.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert_eq!(next.text, "next turn");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn tool_use_turn_executes_tool_and_returns_outcome() {
    let home = unique_temp_dir("mock-tool-home");
    let cwd = unique_temp_dir("mock-tool-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_tool_use()))
        .await
        .unwrap();

    let outcome = runtime.run_turn("run echo hello").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    // At least one tool was executed
    assert!(
        outcome.tool_call_count >= 1,
        "expected at least 1 tool call, got {}",
        outcome.tool_call_count
    );
    // The LLM response after tool execution
    assert!(outcome.text.contains("all done"));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn tool_use_turn_emits_tool_call_and_tool_result_events() {
    let home = unique_temp_dir("mock-tool-ev-home");
    let cwd = unique_temp_dir("mock-tool-ev-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let sink = Arc::new(RecordingSink::default());

    let mut rt_config = runtime_config_with_mock(&home, &cwd, mock_llm_tool_use());
    rt_config.event_sink = Some(sink.clone());

    let runtime = build_runtime(rt_config).await.unwrap();
    runtime.run_turn("run echo hello").await.unwrap();
    runtime.shutdown().await.unwrap();

    let (has_tool_call, has_tool_result, has_final) = {
        let events = sink.events.lock().unwrap();
        (
            events
                .iter()
                .any(|e| matches!(e.kind, crate::runtime::AgentEventKind::ToolCall { .. })),
            events
                .iter()
                .any(|e| matches!(e.kind, crate::runtime::AgentEventKind::ToolResult { .. })),
            events
                .iter()
                .any(|e| matches!(e.kind, crate::runtime::AgentEventKind::Final { .. })),
        )
    };

    assert!(has_tool_call, "expected ToolCall event");
    assert!(has_tool_result, "expected ToolResult event");
    assert!(has_final, "expected Final event");

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn stream_turn_without_event_sink_emits_tool_events() {
    let home = unique_temp_dir("mock-tool-stream-home");
    let cwd = unique_temp_dir("mock-tool-stream-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_tool_use()))
        .await
        .unwrap();

    let mut stream = runtime.stream_turn("run echo hello").unwrap();
    let mut saw_tool_call = false;
    let mut saw_tool_result = false;
    while let Some(event) =
        tokio::time::timeout(tokio::time::Duration::from_secs(10), stream.recv())
            .await
            .expect("stream event timed out")
    {
        match event.kind {
            crate::runtime::AgentEventKind::ToolCall { .. } => saw_tool_call = true,
            crate::runtime::AgentEventKind::ToolResult { .. } => saw_tool_result = true,
            crate::runtime::AgentEventKind::Final { .. } => break,
            _ => {}
        }
    }

    let outcome = stream.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert!(saw_tool_call, "expected streaming ToolCall event");
    assert!(saw_tool_result, "expected streaming ToolResult event");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn partial_tool_results_survive_join_failure() {
    let home = unique_temp_dir("partial-fatal-home");
    let cwd = unique_temp_dir("partial-fatal-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    tokio::fs::write(cwd.join("fixture.txt"), "alpha\nbeta\n")
        .await
        .unwrap();
    use crate::protocol::{Event, StopEvent, TextEvent};
    use serde_json::json;
    let mock = crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::ToolCall(tool_call_event(
                    "Read",
                    "call_read",
                    json!({"path":"fixture.txt"}),
                ))),
                Ok(Event::ToolCall(tool_call_event(
                    "PanicTool",
                    "call_panic",
                    json!({}),
                ))),
                Ok(Event::ToolCall(tool_call_event(
                    "PanicTool",
                    "call_later",
                    json!({"release":true}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "should not be reached".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    );
    let mut config = runtime_config_with_mock(&home, &cwd, mock);
    config
        .custom_tools
        .push(Arc::new(PanicTool(tokio::sync::Notify::new())));
    let runtime = build_runtime(config).await.unwrap();

    let outcome = runtime.run_turn("read then panic").await.unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed,
        "a panicking custom tool must fail the turn: {outcome:?}"
    );
    assert_eq!(
        outcome.failure,
        Some(crate::runtime::TurnFailureReason::Internal)
    );
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("join failed")),
        "fatal cause must surface: {outcome:?}"
    );

    let conversation = tokio::fs::read_to_string(&outcome.session.conversation_path)
        .await
        .unwrap();
    assert!(conversation.contains("call_read"), "{conversation}");
    assert!(
        conversation.contains("alpha"),
        "successful Read result must survive the later failure: {conversation}"
    );
    assert!(
        !conversation.contains("tool execution failed"),
        "no synthetic failure may replace the successful Read: {conversation}"
    );
    assert!(
        conversation.contains("execution outcome unknown"),
        "the panicked call must have an unknown outcome: {conversation}"
    );

    let blocks: Vec<serde_json::Value> = conversation
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter_map(|row| row["content"].as_array().cloned())
        .flatten()
        .filter(|block| block["type"] == "tool_result")
        .collect();
    assert_eq!(
        blocks
            .iter()
            .map(|b| b["tool_use_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["call_read", "call_panic", "call_later"]
    );
    assert_eq!(blocks[1]["_mink"]["execution_outcome"], "unknown");
    assert!(
        blocks[2]["content"]
            .as_str()
            .unwrap()
            .contains("later sibling finished")
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn goal_retains_dispatched_effect_after_tool_panics_and_session_reopens() {
    assert_unknown_goal_effect(false).await;
}

#[tokio::test]
async fn goal_retains_dispatched_effect_after_extension_timeout_and_reopen() {
    assert_unknown_goal_effect(true).await;
}

async fn assert_unknown_goal_effect(timeout: bool) {
    use crate::protocol::{Event, StopEvent};
    use crate::runtime::{AgentOptions, AgentRuntime, GoalSpec, SessionPolicy};
    let home = unique_temp_dir("unknown-goal-home");
    let cwd = unique_temp_dir("unknown-goal-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::ToolCall(tool_call_event(
                "PanicTool",
                "effect",
                serde_json::json!({"mark":true, "stall":timeout}),
            ))),
            Ok(Event::ToolCall(tool_call_event(
                "Write",
                "never",
                serde_json::json!({"path":"later","content":"bad"}),
            ))),
            Ok(Event::Stop(StopEvent {
                reason: "tool_use".into(),
            })),
        ]],
    ));
    let options = || {
        AgentOptions::new(&home, &cwd)
            .with_llm_backend(backend.clone())
            .with_tool_timeout_secs(1)
            .with_tool(Arc::new(PanicTool(tokio::sync::Notify::new())))
    };
    let runtime = AgentRuntime::start(options()).await.unwrap();
    runtime
        .handle()
        .confirmed_test_goal(GoalSpec::new("record result"))
        .unwrap();
    runtime
        .ctx
        .stores
        .goal_store
        .begin_iteration("test-turn", 0)
        .unwrap();
    let mut executor = crate::agent::turn::TurnExecutor::new(runtime.ctx.clone());
    assert!(executor.execute("record result", None).await.is_err());
    assert_eq!(std::fs::read(cwd.join("attempted")).unwrap(), b"once");
    assert!(!cwd.join("later").exists());
    assert_eq!(
        runtime
            .ctx
            .stores
            .goal_store
            .snapshot()
            .unwrap()
            .pending_effects,
        ["effect"]
    );
    let rows = runtime.ctx.stores.store.lines().await.unwrap();
    let results = rows
        .iter()
        .filter_map(|row| row["content"].as_array())
        .flatten()
        .filter(|block| block["type"] == "tool_result")
        .collect::<Vec<_>>();
    assert_eq!(results[0]["_mink"]["execution_outcome"], "unknown");
    assert_eq!(results[1]["tool_use_id"], "never");
    assert!(
        results[1]["content"]
            .as_str()
            .unwrap()
            .contains("not executed")
    );
    let id = runtime.session_info().session_id.clone();
    drop(executor);
    runtime.shutdown().await.unwrap();
    let resumed = AgentRuntime::start(options().with_session(SessionPolicy::Resume(id)))
        .await
        .unwrap();
    let goal = resumed.ctx.stores.goal_store.snapshot().unwrap();
    assert_eq!(goal.pending_effects, ["effect"]);
    assert_eq!(goal.status, crate::runtime::GoalStatus::Blocked);
    resumed.shutdown().await.unwrap();
    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(cwd).unwrap();
}
