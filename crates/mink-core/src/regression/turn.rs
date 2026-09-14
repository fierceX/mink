use super::*;

#[tokio::test]
async fn full_turn_tool_loop_preserves_conversation_order() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::ToolCall(tool_call(
                    "Read",
                    "call_read",
                    json!({"path":"fixture.txt"}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    ));
    let h = harness_with_backend("turn-loop", llm.clone()).await?;
    tokio::fs::write(h.cwd.join("fixture.txt"), "alpha\nbeta\n").await?;
    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, effects) = executor.execute("read fixture", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(effects.is_empty());
    assert_eq!(executor.tool_call_count(), 1);
    let lines = h.ctx.store.lines().await?;
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["role"], "user");
    assert_eq!(lines[1]["role"], "assistant");
    assert_eq!(lines[1]["content"][0]["type"], "thinking");
    assert_eq!(lines[1]["content"][2]["type"], "tool_use");
    assert_eq!(lines[2]["role"], "user");
    assert_eq!(lines[2]["content"][0]["type"], "tool_result");
    assert_eq!(lines[3]["role"], "assistant");
    assert_eq!(lines[3]["content"][1]["text"], "done");
    Ok(())
}

#[tokio::test]
async fn turn_retry_thinking_usage_and_stop_are_persisted() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Text(TextEvent {
                content: "stale".into(),
            })),
            Ok(Event::Retry(RetryEvent {})),
            Ok(Event::Thinking(ThinkingEvent {
                content: "think".into(),
            })),
            Ok(Event::Text(TextEvent {
                content: "final".into(),
            })),
            Ok(Event::Usage(UsageEvent {
                input_tokens: 11,
                output_tokens: 7,
                cache_read_input_tokens: 3,
                cache_creation_input_tokens: 2,
            })),
            Ok(Event::Stop(StopEvent {
                reason: "stop".into(),
            })),
        ]],
    ));
    let h = harness_with_backend("turn-retry-usage", llm.clone()).await?;

    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, effects) = executor.execute("retry once", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(effects.is_empty());
    let lines = h.ctx.store.lines().await?;
    assert_eq!(lines[1]["content"][0]["thinking"], "think");
    assert_eq!(lines[1]["content"][1]["text"], "final");
    assert!(!serde_json::to_string(&lines[1])?.contains("stale"));
    let stats = h.ctx.stats.snapshot().await;
    assert_eq!(stats.total_input_tokens, 11);
    assert_eq!(stats.total_output_tokens, 7);
    assert_eq!(stats.total_cache_read_tokens, 3);
    assert_eq!(stats.total_cache_creation_tokens, 2);
    Ok(())
}

#[tokio::test]
async fn turn_error_event_returns_error_and_logs_event() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![Ok(Event::Error(ErrorEvent {
            message: "model error".into(),
        }))]],
    ));
    let h = harness_with_backend("turn-error-event", llm.clone()).await?;

    let mut executor = TurnExecutor::new(h.ctx.clone());
    let err = executor
        .execute("trigger model error", None)
        .await
        .unwrap_err()
        .to_string();

    assert_eq!(err, "model error");
    h.ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&h.ctx.events_path).await?;
    assert!(events.contains(r#""type":"error""#), "{events}");
    assert!(events.contains(r#""message":"model error""#), "{events}");
    Ok(())
}

#[tokio::test]
async fn turn_cancel_after_stream_returns_interrupted_without_assistant() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Text(TextEvent {
                content: "not persisted".into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]],
    ));
    let h = harness_with_backend("turn-cancel-after-stream", llm.clone()).await?;
    h.ctx.cancel.cancel();
    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, effects) = executor.execute("cancel now", None).await?;

    assert_eq!(decision, TurnDecision::Interrupted);
    assert!(effects.is_empty());
    let lines = h.ctx.store.lines().await?;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["role"], "user");
    Ok(())
}

#[tokio::test]
async fn turn_scavenges_text_tool_call_and_executes_it() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::Text(TextEvent {
                    content:
                        r#"<tool_call>{"name":"Read","arguments":{"path":"scavenge.txt"}}</tool_call>"#
                            .into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    ));
    let h = harness_with_backend("turn-scavenge-tool", llm.clone()).await?;
    tokio::fs::write(h.cwd.join("scavenge.txt"), "found\n").await?;
    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, effects) = executor.execute("recover tool call", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(effects.is_empty());
    assert_eq!(executor.tool_call_count(), 1);
    let lines = h.ctx.store.lines().await?;
    assert!(
        lines[1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block["type"] == "tool_use"),
        "{}",
        lines[1]
    );
    assert_eq!(lines[2]["content"][0]["type"], "tool_result");
    assert!(
        lines[2]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("found")
    );
    h.ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&h.ctx.events_path).await?;
    assert!(events.contains(r#""type":"scavenge""#), "{events}");
    Ok(())
}

#[tokio::test]
async fn turn_scavenged_tool_call_after_end_turn_continues_loop() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::Text(TextEvent {
                    content:
                        r#"<tool_call>{"name":"Read","arguments":{"path":"scavenge-end.txt"}}</tool_call>"#
                            .into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    ));
    let h = harness_with_backend("turn-scavenge-end-turn", llm.clone()).await?;
    tokio::fs::write(h.cwd.join("scavenge-end.txt"), "found\n").await?;
    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, effects) = executor.execute("recover after end_turn", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(effects.is_empty());
    assert_eq!(executor.tool_call_count(), 1);
    let lines = h.ctx.store.lines().await?;
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[2]["content"][0]["type"], "tool_result");
    assert_eq!(lines[3]["content"][1]["text"], "done");
    Ok(())
}

#[tokio::test]
async fn turn_stream_without_stop_event_fails_without_assistant_message() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![Ok(Event::Text(TextEvent {
            content: "partial".into(),
        }))]],
    ));
    let h = harness_with_backend("turn-missing-stop", llm.clone()).await?;

    let mut executor = TurnExecutor::new(h.ctx.clone());
    let err = executor
        .execute("missing stop", None)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("stream ended without stop event"), "{err}");
    let lines = h.ctx.store.lines().await?;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["role"], "user");
    Ok(())
}

#[tokio::test]
async fn turn_llm_first_event_timeout_fails_with_clear_error() -> anyhow::Result<()> {
    let h = harness_with_config(
        "turn-first-event-timeout",
        false,
        300,
        |cfg| {
            cfg.llm_first_event_timeout_secs = 1;
            cfg.llm_idle_timeout_secs = 10;
            cfg.llm_wait_heartbeat_secs = 0;
        },
        Some(Arc::new(PendingLlmBackend)),
    )
    .await?;
    let mut executor = TurnExecutor::new(h.ctx.clone());
    let err = executor
        .execute("model never starts", None)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("first event timeout"), "{err}");
    h.ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&h.ctx.events_path).await?;
    assert!(
        events.contains(r#""category":"llm_first_event_timeout""#),
        "{events}"
    );
    Ok(())
}

#[tokio::test]
async fn turn_llm_idle_timeout_fails_after_partial_stream() -> anyhow::Result<()> {
    let h = harness_with_config(
        "turn-idle-timeout",
        false,
        300,
        |cfg| {
            cfg.llm_first_event_timeout_secs = 10;
            cfg.llm_idle_timeout_secs = 1;
            cfg.llm_wait_heartbeat_secs = 0;
        },
        Some(Arc::new(IdleAfterTextLlmBackend)),
    )
    .await?;
    let mut executor = TurnExecutor::new(h.ctx.clone());
    let err = executor
        .execute("model stalls", None)
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("idle timeout"), "{err}");
    h.ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&h.ctx.events_path).await?;
    assert!(
        events.contains(r#""category":"llm_idle_timeout""#),
        "{events}"
    );
    Ok(())
}

#[tokio::test]
async fn turn_max_turns_exhaustion_is_failed_not_stop() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::ToolCall(tool_call(
                "Read",
                "call_1",
                json!({"path":"fixture.txt"}),
            ))),
            Ok(Event::Stop(StopEvent {
                reason: "tool_calls".into(),
            })),
        ]],
    ));
    let h = harness_with_config(
        "turn-max-turns",
        false,
        300,
        |cfg| {
            cfg.max_turns = 1;
        },
        Some(llm.clone()),
    )
    .await?;
    tokio::fs::write(h.cwd.join("fixture.txt"), "alpha\n").await?;
    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, effects) = executor.execute("read until exhausted", None).await?;

    assert_eq!(decision, TurnDecision::MaxTurnsExceeded);
    assert!(effects.is_empty());
    assert_eq!(executor.tool_call_count(), 1);
    Ok(())
}

#[tokio::test]
async fn disabled_tool_call_persists_error_result_instead_of_being_dropped() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::ToolCall(tool_call(
                    "Bash",
                    "call_bash",
                    json!({"command":"echo should-not-run"}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_calls".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    ));
    let h = harness_with_config(
        "disabled-tool-result",
        false,
        300,
        |cfg| {
            cfg.enabled_tools = Some(vec!["Read".into()]);
        },
        Some(llm.clone()),
    )
    .await?;

    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, effects) = executor.execute("try disabled bash", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(effects.is_empty());
    let lines = h.ctx.store.lines().await?;
    assert!(
        lines[1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block["type"] == "tool_use" && block["name"] == "Bash")
    );
    assert!(
        lines[2]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Tool 'Bash' is unavailable"),
        "{}",
        lines[2]
    );
    Ok(())
}

#[tokio::test]
async fn invalid_scavenged_tool_call_is_logged_and_ignored() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Text(TextEvent {
                content: r#"<tool_call>{"name":"Read","arguments":[]}</tool_call>"#.into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]],
    ));
    let h = harness_with_backend("invalid-scavenge", llm.clone()).await?;

    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, effects) = executor.execute("bad scavenged call", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(effects.is_empty());
    assert_eq!(executor.tool_call_count(), 0);
    h.ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&h.ctx.events_path).await?;
    assert!(
        events.contains("discarded invalid scavenged call Read"),
        "{events}"
    );
    Ok(())
}

#[tokio::test]
async fn duplicate_scavenged_tool_call_is_deduplicated_against_official_call() -> anyhow::Result<()>
{
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::ToolCall(tool_call(
                    "Read",
                    "call_read",
                    json!({"path":"dup.txt"}),
                ))),
                Ok(Event::Text(TextEvent {
                    content:
                        r#"<tool_call>{"name":"Read","arguments":{"path":"dup.txt"}}</tool_call>"#
                            .into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_calls".into(),
                })),
            ],
            vec![Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            }))],
        ],
    ));
    let h = harness_with_backend("duplicate-scavenge", llm.clone()).await?;
    tokio::fs::write(h.cwd.join("dup.txt"), "once\n").await?;
    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, effects) = executor.execute("dedupe scavenged", None).await?;
    assert_eq!(decision, TurnDecision::Stop);
    assert!(effects.is_empty());
    assert_eq!(executor.tool_call_count(), 1);
    Ok(())
}

#[tokio::test]
async fn edit_tool_result_uses_full_edit_preview_branch() -> anyhow::Result<()> {
    // The tag is only known after the snapshot store records the file, while
    // the shared backend must exist at context build. A deferred slot keeps a
    // single backend without a test-only executor side channel.
    let tag = Arc::new(std::sync::Mutex::new(String::new()));
    let llm = Arc::new(DeferredEditBackend {
        tag: tag.clone(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let h = harness_with_backend("edit-preview", llm).await?;
    tokio::fs::write(h.cwd.join("edit.txt"), "old\n").await?;
    let snapshot = h
        .ctx
        .snapshots
        .lock()
        .unwrap()
        .record(&h.cwd.join("edit.txt"), "old\n", [1]);
    *tag.lock().unwrap() = snapshot.tag;

    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, effects) = executor.execute("edit file", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(effects.is_empty());
    assert_eq!(
        tokio::fs::read_to_string(h.cwd.join("edit.txt")).await?,
        "new\n"
    );
    Ok(())
}

#[tokio::test]
async fn latched_fault_blocks_turn_before_tools_and_history() -> anyhow::Result<()> {
    let h = harness("persistence-fault-entry").await?;
    let marker = h.cwd.join("must-not-exist.txt");
    // Simulate a store that latched an unrecoverable publish/durability
    // fault: the next turn must be refused before the model request, any
    // history append or any tool side effect.
    let _ = h
        .ctx
        .persistence_fault
        .raise(std::path::Path::new("todos.json"), "injected publish fault");
    let mut executor = TurnExecutor::new(h.ctx.clone());

    let error = executor
        .execute("check entry seal", None)
        .await
        .expect_err("a latched persistence fault must fail the turn");

    assert!(error.to_string().contains("persistence fault"), "{error}");
    assert!(error.to_string().contains("restart the session"), "{error}");
    assert!(
        !marker.exists(),
        "the latched turn must not execute tools with side effects"
    );
    assert!(
        h.ctx.store.lines().await?.is_empty(),
        "the latched turn must not modify history"
    );
    Ok(())
}

#[tokio::test]
async fn session_fault_latch_is_shared_with_plan_and_todo_stores() -> anyhow::Result<()> {
    let h = harness("fault-latch-sharing").await?;
    let _ = h
        .ctx
        .persistence_fault
        .raise(std::path::Path::new("state.json"), "injected fault");

    let tool_ctx = crate::context::ToolContext::from(h.ctx.as_ref());
    let plan_error = tool_ctx
        .plan_store
        .set_draft("draft body", 1024)
        .expect_err("a latched session must reject Plan writes");
    assert!(
        plan_error.to_string().contains("persistence fault"),
        "{plan_error}"
    );

    let todo_error = h
        .ctx
        .todo_store
        .apply_structure(0, crate::session::todo::TodoChanges::default())
        .expect_err("a latched session must reject Todo writes");
    assert!(
        todo_error.to_string().contains("persistence fault"),
        "{todo_error}"
    );
    Ok(())
}

#[tokio::test]
async fn critical_events_keep_stream_json_stdout_output() -> anyhow::Result<()> {
    const CHILD_ENV: &str = "MINK_TEST_STREAM_JSON_CHILD";
    const TEST_NAME: &str = "regression::turn::critical_events_keep_stream_json_stdout_output";

    if std::env::var(CHILD_ENV).ok().as_deref() == Some("1") {
        // Child branch: write through the real stdout path, then verify the
        // persisted records. A leading newline separates our JSON from the
        // libtest progress line.
        {
            use std::io::Write as _;
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout)?;
            stdout.flush()?;
        }
        let h = crate::regression::harness_with_config(
            "critical-stream-json",
            false,
            300,
            |config| config.output_format = crate::config::OutputFormat::StreamJson,
            None,
        )
        .await?;
        let ctx = h.ctx.clone();
        ctx.log_critical_event(crate::events::EventLog::PrefixSnapshot {
            version: Some(1),
            fingerprint: "fp".into(),
            dependency_fingerprint: "dep".into(),
            system_prompt: "prompt".into(),
            tools_json: Vec::new(),
        })
        .await?;
        ctx.log_event(crate::events::EventLog::Stop {
            reason: "end_turn".into(),
        });
        ctx.flush_event_log().await?;

        let stored = tokio::fs::read_to_string(&ctx.events_path).await?;
        let records: Vec<serde_json::Value> = stored
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let prefix = records
            .iter()
            .find(|record| {
                record.get("type").and_then(serde_json::Value::as_str) == Some("prefix_snapshot")
            })
            .expect("prefix_snapshot persisted");
        assert_eq!(
            prefix
                .get("fingerprint")
                .and_then(serde_json::Value::as_str),
            Some("fp")
        );
        let stop = records
            .iter()
            .find(|record| record.get("type").and_then(serde_json::Value::as_str) == Some("stop"))
            .expect("stop persisted");
        assert_eq!(
            stop.get("reason").and_then(serde_json::Value::as_str),
            Some("end_turn")
        );
        return Ok(());
    }

    // Parent branch: run this exact test in a child process and parse the
    // bytes it actually wrote to stdout.
    let exe = std::env::current_exe()?;
    let output = tokio::process::Command::new(exe)
        .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), output)
        .await
        .expect("stream-json child test timed out")?;
    assert!(
        output.status.success(),
        "child failed: stdout=\n{}\nstderr=\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_records: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with('{'))
        .map(|line| serde_json::from_str(line).expect("stdout JSON line must parse strictly"))
        .collect();
    let prefixes: Vec<&serde_json::Value> = json_records
        .iter()
        .filter(|record| {
            record.get("type").and_then(serde_json::Value::as_str) == Some("prefix_snapshot")
        })
        .collect();
    let stops: Vec<&serde_json::Value> = json_records
        .iter()
        .filter(|record| record.get("type").and_then(serde_json::Value::as_str) == Some("stop"))
        .collect();
    assert_eq!(prefixes.len(), 1, "{json_records:?}");
    assert_eq!(stops.len(), 1, "{json_records:?}");
    assert_eq!(
        prefixes[0]
            .get("fingerprint")
            .and_then(serde_json::Value::as_str),
        Some("fp")
    );
    assert_eq!(
        prefixes[0]
            .get("dependency_fingerprint")
            .and_then(serde_json::Value::as_str),
        Some("dep")
    );
    assert_eq!(
        stops[0].get("reason").and_then(serde_json::Value::as_str),
        Some("end_turn")
    );
    let prefix_index = json_records
        .iter()
        .position(|record| {
            record.get("type").and_then(serde_json::Value::as_str) == Some("prefix_snapshot")
        })
        .unwrap();
    let stop_index = json_records
        .iter()
        .position(|record| record.get("type").and_then(serde_json::Value::as_str) == Some("stop"))
        .unwrap();
    assert!(
        prefix_index < stop_index,
        "prefix_snapshot must precede stop"
    );
    Ok(())
}

struct DeferredEditBackend {
    tag: Arc<std::sync::Mutex<String>>,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for DeferredEditBackend {
    fn name(&self) -> &str {
        "deferred-edit"
    }

    async fn stream(
        &self,
        _request: crate::llm::client::LlmRequest,
    ) -> anyhow::Result<crate::llm::client::LlmResponseStream> {
        use crate::protocol::{Event, StopEvent, TextEvent};
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let events = if call == 0 {
            let tag = self
                .tag
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            vec![
                Ok(Event::ToolCall(tool_call(
                    "Edit",
                    "call_edit",
                    serde_json::json!({"input": format!("[edit.txt#{tag}]\nPUT 1.=1:\n+new")}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_calls".into(),
                })),
            ]
        } else {
            vec![
                Ok(Event::Text(TextEvent {
                    content: "done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ]
        };
        Ok(crate::llm::client::LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}
