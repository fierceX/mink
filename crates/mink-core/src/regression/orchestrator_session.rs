use super::*;

async fn send_set_model(
    tx: &tokio::sync::mpsc::UnboundedSender<OrchCmd>,
    model: &str,
) -> anyhow::Result<()> {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tx.send(OrchCmd::SetModel {
        model: model.into(),
        done: done_tx,
    })?;
    done_rx.await??;
    Ok(())
}

async fn send_user_input(
    tx: &tokio::sync::mpsc::UnboundedSender<OrchCmd>,
    input: &str,
) -> anyhow::Result<crate::agent::orchestrator::TurnRunResult> {
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let turn_id = crate::runtime::TurnId::new("test-turn");
    let emitter = std::sync::Arc::new(crate::runtime::TurnEventEmitter::new(
        turn_id.clone(),
        None,
        None,
    ));
    tx.send(OrchCmd::UserInput {
        input: input.into(),
        turn_id,
        emitter,
        done: done_tx,
    })?;
    Ok(done_rx.await?)
}

#[tokio::test]
async fn orchestrator_user_input_runs_turn_and_logs_tracking() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Text(TextEvent {
                content: "hello".into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]],
    ));
    let h = harness_with_backend("orch-user-input", llm.clone()).await?;
    run_orchestrator_user_input(h.ctx.clone(), "say hi").await?;
    let lines = h.ctx.store.lines().await?;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["role"], "user");
    assert_eq!(lines[1]["role"], "assistant");
    h.ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&h.ctx.events_path).await?;
    assert!(events.contains(r#""type":"turn_start""#), "{events}");
    assert!(events.contains(r#""type":"turn_tracking""#), "{events}");
    assert!(events.contains(r#""type":"turn_final""#), "{events}");
    assert!(events.contains(r#""status":"ok""#), "{events}");
    assert!(events.contains(r#""decision":"Stop""#), "{events}");
    Ok(())
}

#[tokio::test]
async fn orchestrator_model_command_updates_display() -> anyhow::Result<()> {
    let h = harness("orch-model-command").await?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = OrchActor::new(h.ctx.clone(), rx);
    let handle = tokio::spawn(actor.run());
    send_set_model(&tx, "pro").await?;
    send_set_model(&tx, "unknown").await?;
    drop(tx);
    handle.await??;
    assert!(
        h.display
            .info
            .lock()
            .unwrap()
            .iter()
            .any(|msg| msg == "Switched to pro model."),
        "{:?}",
        h.display.info.lock().unwrap()
    );
    assert!(
        h.display
            .title_models
            .lock()
            .unwrap()
            .iter()
            .any(|model| model == "pro"),
        "{:?}",
        h.display.title_models.lock().unwrap()
    );
    assert!(
        h.display
            .info
            .lock()
            .unwrap()
            .iter()
            .any(|msg| msg == "Switched to unknown model."),
        "{:?}",
        h.display.info.lock().unwrap()
    );
    assert!(
        h.display
            .title_models
            .lock()
            .unwrap()
            .iter()
            .any(|model| model == "unknown"),
        "{:?}",
        h.display.title_models.lock().unwrap()
    );
    Ok(())
}

#[tokio::test]
async fn orchestrator_forced_model_title_survives_turn_refreshes() -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let llm = Arc::new(MockLlmBackend::new(
        "pro",
        vec![vec![
            Ok(Event::Text(TextEvent {
                content: "done".into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]],
    ));
    let h = harness_with_backend("orch-forced-model-title", llm.clone()).await?;
    let actor = OrchActor::new(h.ctx.clone(), rx);
    let handle = tokio::spawn(actor.run());
    send_set_model(&tx, "pro").await?;
    let result = send_user_input(&tx, "say hi").await?;
    assert_eq!(result.status, crate::agent::orchestrator::TurnStatus::Ok);
    drop(tx);
    handle.await??;

    let title_models = h.display.title_models.lock().unwrap();
    assert!(
        !title_models.iter().any(|model| model == "flash"),
        "{title_models:?}"
    );
    assert!(
        title_models.iter().filter(|model| *model == "pro").count() >= 2,
        "{title_models:?}"
    );
    Ok(())
}

#[tokio::test]
async fn orchestrator_active_model_is_used_by_spawned_sub_agent() -> anyhow::Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(ActiveModelRoutingBackend {
        requests: requests.clone(),
        agent_request_count: AtomicU64::new(0),
    });
    let h = harness_with_config(
        "orch-sub-agent-active-model",
        false,
        300,
        |_| {},
        Some(backend),
    )
    .await?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = OrchActor::new(h.ctx.clone(), rx);
    let handle = tokio::spawn(actor.run());
    send_set_model(&tx, "pro").await?;
    let outcome = send_user_input(&tx, "delegate this task").await?;
    drop(tx);
    handle.await??;

    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert_eq!(
        requests.lock().unwrap().as_slice(),
        &[
            CapturedRoutedRequest {
                purpose: "agent",
                model: "deepseek-v4-pro".into(),
                alias: Some("pro".into()),
            },
            CapturedRoutedRequest {
                purpose: "sub_agent",
                model: "deepseek-v4-pro".into(),
                alias: Some("pro".into()),
            },
            CapturedRoutedRequest {
                purpose: "agent",
                model: "deepseek-v4-pro".into(),
                alias: Some("pro".into()),
            },
        ]
    );
    Ok(())
}

#[tokio::test]
async fn orchestrator_flash_command_resets_forced_model_display() -> anyhow::Result<()> {
    let h = harness("orch-flash-command").await?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = OrchActor::new(h.ctx.clone(), rx);
    let handle = tokio::spawn(actor.run());
    send_set_model(&tx, "pro").await?;
    send_set_model(&tx, "flash").await?;
    drop(tx);
    handle.await??;
    let info = h.display.info.lock().unwrap();
    assert!(
        info.iter().any(|msg| msg == "Switched to flash model."),
        "{info:?}"
    );
    Ok(())
}

#[tokio::test]
async fn orchestrator_renders_failed_turn_decision() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![Ok(Event::Stop(StopEvent {
            reason: "max_tokens".into(),
        }))]],
    ));
    let h = harness_with_backend("orch-failed-turn", llm.clone()).await?;
    run_orchestrator_user_input(h.ctx.clone(), "hit limit").await?;
    assert!(
        h.display
            .info
            .lock()
            .unwrap()
            .iter()
            .any(|msg| msg == "error:stop: max_tokens"),
        "{:?}",
        h.display.info.lock().unwrap()
    );
    Ok(())
}

#[tokio::test]
async fn orchestrator_logs_stream_error_from_turn() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![Err(anyhow::anyhow!("stream connection timeout"))]],
    ));
    let h = harness_with_backend("orch-stream-error", llm.clone()).await?;
    run_orchestrator_user_input(h.ctx.clone(), "fail stream").await?;
    assert!(
        h.display
            .info
            .lock()
            .unwrap()
            .iter()
            .any(|msg| msg == "error:Turn execution error: stream connection timeout"),
        "{:?}",
        h.display.info.lock().unwrap()
    );
    h.ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&h.ctx.events_path).await?;
    assert!(events.contains(r#""type":"turn_error""#), "{events}");
    assert!(events.contains(r#""category":"Network""#), "{events}");
    Ok(())
}

#[tokio::test]
async fn orchestrator_cancel_signal_shuts_actor_down() -> anyhow::Result<()> {
    let h = harness("orch-cancel").await?;
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = OrchActor::new(h.ctx.clone(), rx);
    let handle = tokio::spawn(actor.run());
    h.ctx.cancel.cancel();
    handle.await??;
    assert!(
        h.display
            .info
            .lock()
            .unwrap()
            .iter()
            .any(|msg| msg == "Shutting down..."),
        "{:?}",
        h.display.info.lock().unwrap()
    );
    Ok(())
}

#[tokio::test]
async fn orchestrator_manual_compact_empty_session_reports_skip() -> anyhow::Result<()> {
    let h = harness("orch-compact-empty").await?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = OrchActor::new(h.ctx.clone(), rx);
    let handle = tokio::spawn(actor.run());
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tx.send(OrchCmd::Compact { done: done_tx })?;
    done_rx.await??;
    drop(tx);
    handle.await??;
    let info = h.display.info.lock().unwrap();
    assert!(info.iter().any(|msg| msg == "Compressing..."), "{info:?}");
    assert!(
        info.iter().any(|msg| msg == "Compact skipped: empty"),
        "{info:?}"
    );
    Ok(())
}

#[tokio::test]
async fn orchestrator_manual_compact_uses_active_model_and_shared_backend() -> anyhow::Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(RecordingCompactionBackend {
        requests: requests.clone(),
    });
    let h = harness_with_config(
        "orch-compact-active-model",
        false,
        300,
        |config| config.context_compact_tail_tokens = 1,
        Some(backend),
    )
    .await?;
    for index in 0..3 {
        h.ctx
            .store
            .add_user(&format!("user history {index}: {}", "x".repeat(256)))
            .await?;
        h.ctx
            .store
            .add_assistant(&format!("assistant history {index}"), "", &[])
            .await?;
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = OrchActor::new(h.ctx.clone(), rx);
    let handle = tokio::spawn(actor.run());
    send_set_model(&tx, "pro").await?;
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tx.send(OrchCmd::Compact { done: done_tx })?;
    done_rx.await??;
    drop(tx);
    handle.await??;

    assert_eq!(
        requests.lock().unwrap().as_slice(),
        &[CapturedModelTarget {
            model: "deepseek-v4-pro".into(),
            alias: Some("pro".into()),
        }]
    );
    Ok(())
}

#[tokio::test]
async fn orchestrator_manual_compact_failure_is_logged() -> anyhow::Result<()> {
    let h = harness_with_config(
        "orch-compact-failure-event",
        false,
        300,
        |config| config.context_compact_tail_tokens = 1,
        Some(Arc::new(FailingCompactionBackend)),
    )
    .await?;
    for index in 0..3 {
        h.ctx
            .store
            .add_user(&format!("user history {index}: {}", "x".repeat(256)))
            .await?;
        h.ctx
            .store
            .add_assistant(&format!("assistant history {index}"), "", &[])
            .await?;
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = OrchActor::new(h.ctx.clone(), rx);
    let handle = tokio::spawn(actor.run());
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tx.send(OrchCmd::Compact { done: done_tx })?;
    let outcome = done_rx.await?;
    assert!(
        outcome
            .unwrap_err()
            .to_string()
            .contains("planned compaction failure")
    );
    drop(tx);
    handle.await??;

    let events = tokio::fs::read_to_string(&h.ctx.events_path).await?;
    assert!(events.contains(r#""type":"compact""#), "{events}");
    assert!(events.contains(r#""version":2"#), "{events}");
    assert!(events.contains(r#""trigger":"manual""#), "{events}");
    assert!(
        events.contains("failed: planned compaction failure"),
        "{events}"
    );
    Ok(())
}

#[tokio::test]
async fn plan_confirm_and_clear_preserve_immutable_prefix() -> anyhow::Result<()> {
    let backend = Arc::new(PlanPipelineBackend {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let h = harness_with_backend("plan-actions", backend).await?;
    let prefix = PrefixManager::new(h.ctx.clone());
    let (stable_prompt, stable_tools) = prefix.ensure().await?;
    let stable_fingerprint = h
        .ctx
        .immutable_prefix
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .fingerprint()
        .to_string();
    assert!(!stable_prompt.contains("<current-plan>"));

    let mut executor = TurnExecutor::new(h.ctx.clone());

    // Turn 1: PlanDraft through the real tool pipeline.
    let (decision, effects) = executor.execute("draft the plan", None).await?;
    assert_eq!(decision, TurnDecision::Stop);
    assert!(effects.is_empty());
    assert_eq!(
        tokio::fs::read_to_string(&h.ctx.plan_draft_path).await?,
        "1. ship it\n"
    );
    assert!(!h.ctx.plan_path.exists());

    // Turn 2: PlanConfirm commits the file and appends the transition.
    let (decision, effects) = executor.execute("confirm the plan", None).await?;
    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(effects, vec!["Plan confirmed."]);
    assert_eq!(
        tokio::fs::read_to_string(&h.ctx.plan_path).await?,
        "1. ship it\n"
    );
    assert!(!h.ctx.plan_draft_path.exists());
    let rows = h.ctx.store.lines().await?;
    let confirmed_index = rows
        .iter()
        .position(|message| {
            message
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|content| content.contains("<plan-transition state=\"confirmed\">"))
        })
        .expect("confirmed transition persisted");
    assert!(rows[..confirmed_index].iter().any(|message| {
        message
            .get("content")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block.get("tool_use_id").and_then(serde_json::Value::as_str)
                        == Some("call_plan_confirm")
                })
            })
    }));

    // Turn 3: PlanClear removes the file and records its transition.
    let (decision, effects) = executor.execute("clear the plan", None).await?;
    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(effects, vec!["Plan cleared."]);
    assert!(!h.ctx.plan_path.exists());
    let rows = h.ctx.store.lines().await?;
    let cleared_index = rows
        .iter()
        .position(|message| {
            message
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|content| content.contains("<plan-transition state=\"cleared\">"))
        })
        .expect("cleared transition persisted");
    assert!(cleared_index > confirmed_index);
    assert!(rows[..cleared_index].iter().any(|message| {
        message
            .get("content")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block.get("tool_use_id").and_then(serde_json::Value::as_str)
                        == Some("call_plan_clear")
                })
            })
    }));

    // The immutable prefix never changes across plan transitions.
    let (after_prompt, after_tools) = prefix.ensure().await?;
    assert_eq!(after_prompt, stable_prompt);
    assert_eq!(after_tools, stable_tools);
    assert_eq!(
        h.ctx
            .immutable_prefix
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .fingerprint(),
        stable_fingerprint
    );
    Ok(())
}
#[tokio::test]
async fn plan_compaction_obeys_the_existing_single_turn_guard() -> anyhow::Result<()> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(PlanTurnBackend {
        compaction_requests: requests.clone(),
        forbid_compaction: false,
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let h = harness_with_config(
        "plan-single-compaction",
        false,
        300,
        |config| {
            config.context_compact_pct = 1;
            config.context_compact_tail_tokens = 1;
        },
        Some(backend),
    )
    .await?;
    for index in 0..4 {
        h.ctx
            .store
            .add_user(&format!("history {index}: {}", "x".repeat(12_000)))
            .await?;
        h.ctx
            .store
            .add_assistant(&format!("history response {index}"), "", &[])
            .await?;
    }
    tokio::fs::write(&h.ctx.plan_draft_path, "1. execute\n").await?;

    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, effects) = executor.execute("confirm the plan", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(matches!(effects.as_slice(), ["Plan confirmed."]));
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(h.ctx.plan_path.exists());
    assert!(!h.ctx.plan_draft_path.exists());
    Ok(())
}

#[tokio::test]
async fn plan_confirm_does_not_force_compaction() -> anyhow::Result<()> {
    let h = harness_with_config(
        "plan-compaction-error",
        false,
        300,
        |config| {
            config.context_compact_pct = 100;
            config.context_compact_tail_tokens = 1;
        },
        Some(Arc::new(PlanTurnBackend {
            compaction_requests: Arc::new(Mutex::new(Vec::new())),
            forbid_compaction: true,
            calls: std::sync::atomic::AtomicUsize::new(0),
        })),
    )
    .await?;
    for index in 0..3 {
        h.ctx.store.add_user(&format!("history {index}")).await?;
        h.ctx
            .store
            .add_assistant(&format!("history response {index}"), "", &[])
            .await?;
    }
    tokio::fs::write(&h.ctx.plan_draft_path, "1. execute\n").await?;

    let mut executor = TurnExecutor::new(h.ctx.clone());
    let (decision, _) = executor.execute("confirm the plan", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(h.ctx.plan_path.exists());
    assert!(!h.ctx.plan_draft_path.exists());
    let rows = h.ctx.store.lines().await?;
    let transition_index = rows
        .iter()
        .position(|message| {
            message
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|content| content.contains("<plan-transition state=\"confirmed\">"))
        })
        .expect("confirmed transition persisted");
    assert!(
        rows[..transition_index].iter().any(|message| {
            message
                .get("content")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block.get("tool_use_id").and_then(serde_json::Value::as_str)
                            == Some("call_plan_confirm")
                    })
                })
        }),
        "tool result must precede confirmed transition"
    );
    Ok(())
}

#[tokio::test]
async fn plan_confirm_without_draft_returns_error_result() -> anyhow::Result<()> {
    let h = harness("plan-empty").await?;
    let tool_ctx = crate::context::ToolContext::from(h.ctx.as_ref());
    let error = crate::tools::runner::ToolExec::execute(
        &crate::tools::plan::PlanConfirmTool,
        &serde_json::json!({}),
        &tool_ctx,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "no plan draft found to confirm");
    Ok(())
}

#[tokio::test]
async fn plan_draft_empty_content_cancels_and_reports_cancellation() -> anyhow::Result<()> {
    let h = harness("plan-draft-cancel").await?;
    let tool_ctx = crate::context::ToolContext::from(h.ctx.as_ref());
    crate::tools::runner::ToolExec::execute(
        &crate::tools::plan::PlanDraftTool,
        &serde_json::json!({"content": "1. inspect\n"}),
        &tool_ctx,
    )?;
    assert!(h.ctx.plan_draft_path.exists());

    let outcome = crate::tools::runner::ToolExec::execute(
        &crate::tools::plan::PlanDraftTool,
        &serde_json::json!({"content": ""}),
        &tool_ctx,
    )?;

    assert_eq!(outcome.content, "Plan draft cancelled.");
    assert!(matches!(
        outcome.presentation,
        Some(crate::ui::ToolPresentation::Plan(crate::ui::PlanDisplay {
            transition: crate::ui::PlanTransitionDisplay::DraftCancelled,
            content: None,
        }))
    ));
    assert!(!h.ctx.plan_draft_path.exists());
    Ok(())
}

#[tokio::test]
async fn todo_tools_persist_incremental_state_and_reject_stale_writes() -> anyhow::Result<()> {
    let h = harness("todo-persistence").await?;
    let prefix = PrefixManager::new(h.ctx.clone());
    let stable_prefix = prefix.ensure().await?;
    assert!(!stable_prefix.0.contains("<current-todos"));
    let tool_ctx = crate::context::ToolContext::from(h.ctx.as_ref());
    let created = crate::tools::runner::ToolExec::execute(
        &crate::tools::todo::TodoWriteTool,
        &json!({
            "base_revision": 0,
            "add": [
                {"content": "inspect"},
                {"content": "implement"},
                {"content": "verify"}
            ]
        }),
        &tool_ctx,
    )?;
    assert!(created.content.contains("revision=\"1\""));
    assert!(created.content.contains("T0001"));
    assert!(created.content.contains("T0002"));
    assert!(matches!(
        created.presentation,
        Some(crate::ui::ToolPresentation::Todo(crate::ui::TodoDisplay {
            revision: 1,
            ..
        }))
    ));

    crate::tools::runner::ToolExec::execute(
        &crate::tools::todo::TodoAdvanceTool,
        &json!({
            "base_revision": 1,
            "activate": ["T0001", "T0002"]
        }),
        &tool_ctx,
    )?;
    let read = crate::tools::runner::ToolExec::execute(
        &crate::tools::todo::TodoReadTool,
        &json!({}),
        &tool_ctx,
    )?;
    assert!(read.content.contains("in_progress=\"2\""));
    assert!(read.content.contains("T0003: verify"));
    assert!(matches!(
        read.presentation,
        Some(crate::ui::ToolPresentation::Todo(crate::ui::TodoDisplay {
            revision: 2,
            ..
        }))
    ));

    crate::tools::runner::ToolExec::execute(
        &crate::tools::todo::TodoWriteTool,
        &json!({
            "base_revision": 2,
            "update": [
                {"id": "T0003", "content": "run focused tests"}
            ]
        }),
        &tool_ctx,
    )?;
    crate::tools::runner::ToolExec::execute(
        &crate::tools::todo::TodoAdvanceTool,
        &json!({
            "base_revision": 3,
            "complete": ["T0001"]
        }),
        &tool_ctx,
    )?;
    let before = h.ctx.todo_store.snapshot();
    let error = crate::tools::runner::ToolExec::execute(
        &crate::tools::todo::TodoWriteTool,
        &json!({"base_revision": 3, "remove": ["T0002"]}),
        &tool_ctx,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("stale todo revision"), "{error}");
    assert_eq!(h.ctx.todo_store.snapshot(), before);
    let todo_path = crate::session::paths::paths_for(&h.ctx.home, &h.ctx.cwd, "regression").todos;
    let reloaded = crate::session::todo::TodoStore::load(todo_path)?;
    assert_eq!(reloaded.snapshot(), before);
    assert_eq!(reloaded.snapshot().revision, 4);
    assert_eq!(prefix.ensure().await?, stable_prefix);
    Ok(())
}

/// Shared backend for plan-transition turns: scripted agent responses plus
/// compaction accounting (or a hard failure when compaction must not run).
struct PlanTurnBackend {
    compaction_requests: Arc<Mutex<Vec<CapturedModelTarget>>>,
    forbid_compaction: bool,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmBackend for PlanTurnBackend {
    fn name(&self) -> &str {
        "plan-turn"
    }

    async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        if matches!(request.purpose, LlmPurpose::Compaction) {
            if self.forbid_compaction {
                anyhow::bail!("compaction must not run for this input");
            }
            self.compaction_requests
                .lock()
                .unwrap()
                .push(CapturedModelTarget {
                    model: request.model,
                    alias: request.model_alias,
                });
            return Ok(LlmResponseStream {
                events: Box::pin(futures::stream::iter(vec![
                    Ok(Event::Text(TextEvent {
                        content: "Current objective and completed work retained.".into(),
                    })),
                    Ok(Event::Stop(StopEvent {
                        reason: "end_turn".into(),
                    })),
                ])),
                attempt_count: 1,
            });
        }
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let events = if call == 0 {
            vec![
                Ok(Event::ToolCall(tool_call(
                    "PlanConfirm",
                    "call_plan_confirm",
                    json!({}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ]
        } else {
            vec![Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            }))]
        };
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

/// Scripted backend for the three-input Plan pipeline (Draft → Confirm →
/// Clear), one shared backend for all requests of the session.
struct PlanPipelineBackend {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl LlmBackend for PlanPipelineBackend {
    fn name(&self) -> &str {
        "plan-pipeline"
    }

    async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        if matches!(request.purpose, LlmPurpose::Compaction) {
            anyhow::bail!("plan pipeline test must not compact");
        }
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let events = match call {
            0 => vec![
                Ok(Event::ToolCall(tool_call(
                    "PlanDraft",
                    "call_plan_draft",
                    json!({"content": "1. ship it\n"}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            1 => vec![
                Ok(Event::Text(TextEvent {
                    content: "drafted".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            2 => vec![
                Ok(Event::ToolCall(tool_call(
                    "PlanConfirm",
                    "call_plan_confirm",
                    json!({}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            3 => vec![
                Ok(Event::Text(TextEvent {
                    content: "confirmed".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            4 => vec![
                Ok(Event::ToolCall(tool_call(
                    "PlanClear",
                    "call_plan_clear",
                    json!({}),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            _ => vec![
                Ok(Event::Text(TextEvent {
                    content: "cleared".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        };
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}
