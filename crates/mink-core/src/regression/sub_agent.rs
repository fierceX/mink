use super::*;

#[tokio::test]
async fn sub_agent_recursion_is_rejected_without_running_child() -> anyhow::Result<()> {
    let h = harness_with("sub-recursion", true, 300).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("nested task".into());
    let runner: SubAgentRunner = Arc::new(|_, _| {
        Box::pin(async {
            panic!("runner must not execute when recursion is blocked");
        })
    });
    let processed = coordinator.process_with_runner(vec![result], runner).await;
    assert_eq!(processed.len(), 1);
    assert!(processed[0].content.contains("exceeds sub_agent_max_depth"));
    assert!(!processed[0].succeeded());
    Ok(())
}

#[tokio::test]
async fn sub_agent_success_formats_result_and_records_usage() -> anyhow::Result<()> {
    let h = harness_with("sub-success", false, 300).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("task".into());
    let runner: SubAgentRunner = Arc::new(|_, _| {
        Box::pin(async {
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
                stop_reason: SubAgentStopReason::Completed,
                diagnostic: None,
                elapsed_ms: 0,
                usage_known: true,
                elapsed_known: true,
                thinking: "child thought".into(),
                text: "child text".into(),
                usage: crate::session::stats::Stats {
                    agent_request_count: 2,
                    total_input_tokens: 10,
                    total_output_tokens: 5,
                    total_cache_read_tokens: 3,
                    total_cache_creation_tokens: 1,
                    ..Default::default()
                },
            }
        })
    });
    let processed = coordinator.process_with_runner(vec![result], runner).await;

    assert_eq!(processed.len(), 1);
    assert!(processed[0].content.contains("] ok (in=10, out=5)"));
    assert!(processed[0].content.contains("child text"));
    assert!(!processed[0].content.contains("Thinking:"));
    assert!(processed[0].succeeded());
    let metadata = processed[0]
        .state_metadata
        .as_ref()
        .expect("sub-agent metadata");
    assert_eq!(metadata["sub_agent"]["status"], "ok");
    assert_eq!(metadata["sub_agent"]["stop_reason"], "completed");
    assert_eq!(metadata["sub_agent"]["label"], "task");
    assert_eq!(metadata["sub_agent"]["depth"], 1);
    assert_eq!(metadata["sub_agent"]["input_tokens"], 10);
    assert_eq!(metadata["sub_agent"]["output_tokens"], 5);
    let stats = h.ctx.stats.snapshot().await;
    assert_eq!(stats.sub_agent_request_count, 1);
    assert_eq!(stats.agent_request_count, 2);
    assert_eq!(stats.total_input_tokens, 10);
    assert_eq!(stats.total_output_tokens, 5);
    Ok(())
}

#[tokio::test]
async fn sub_agent_runner_panic_is_reported_as_failed_result() -> anyhow::Result<()> {
    let h = harness_with("sub-panic", false, 300).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("panic task".into());
    let runner: SubAgentRunner = Arc::new(|_, _| {
        Box::pin(async {
            panic!("panic from test runner");
        })
    });
    let processed = coordinator.process_with_runner(vec![result], runner).await;

    assert_eq!(processed.len(), 1);
    assert!(processed[0].content.contains("] failed (in=0, out=0)"));
    assert!(
        processed[0]
            .content
            .contains("sub_agent_error: task panicked: panic from test runner"),
        "{}",
        processed[0].content
    );
    assert!(!processed[0].succeeded());
    assert!(
        h.display
            .info
            .lock()
            .unwrap()
            .iter()
            .any(|msg| msg.starts_with("error:[sub-agent ") && msg.contains("failed")),
        "{:?}",
        h.display.info.lock().unwrap()
    );
    Ok(())
}

#[tokio::test]
async fn sub_agent_runner_sync_panic_is_reported_as_failed_result() -> anyhow::Result<()> {
    let h = harness_with("sub-sync-panic", false, 300).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("sync panic task".into());
    let runner: SubAgentRunner = Arc::new(|_, _| {
        panic!("sync panic from test runner");
    });
    let processed = coordinator.process_with_runner(vec![result], runner).await;

    assert_eq!(processed.len(), 1);
    assert!(processed[0].content.contains("] failed (in=0, out=0)"));
    assert!(
        processed[0]
            .content
            .contains("sub_agent_error: task panicked: sync panic from test runner"),
        "{}",
        processed[0].content
    );
    Ok(())
}

#[tokio::test]
async fn sub_agent_timeout_reports_task_phase() -> anyhow::Result<()> {
    let h = harness_with("sub-timeout", false, 1).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("slow task".into());
    let runner: SubAgentRunner = Arc::new(|_, _| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
                stop_reason: SubAgentStopReason::Completed,
                diagnostic: None,
                elapsed_ms: 0,
                usage_known: true,
                elapsed_known: true,
                thinking: String::new(),
                text: "late".into(),
                usage: Default::default(),
            }
        })
    });
    let processed = coordinator.process_with_runner(vec![result], runner).await;
    assert!(
        processed[0].content.contains("] timed_out (in="),
        "{}",
        processed[0].content
    );
    assert!(
        processed[0]
            .content
            .contains("sub_agent_timeout phase=setup"),
        "{}",
        processed[0].content
    );
    assert!(!processed[0].succeeded());
    let metadata = processed[0]
        .state_metadata
        .as_ref()
        .expect("timeout metadata");
    assert_eq!(metadata["sub_agent"]["usage_known"], false);
    assert!(metadata["sub_agent"].get("elapsed_known").is_none());
    assert!(
        metadata["sub_agent"]["elapsed_ms"].as_u64().unwrap() <= 1200,
        "timeout elapsed must exclude the 250ms grace: {}",
        metadata["sub_agent"]["elapsed_ms"]
    );
    Ok(())
}

#[tokio::test]
async fn setup_timeout_cancels_before_grace_and_removes_partial_directory() -> anyhow::Result<()> {
    let h = harness_with("sub-setup-cleanup", false, 1).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("slow setup".into());
    let dir = std::env::temp_dir().join(format!(
        "mink-subagent-setup-cleanup-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runner_dir = dir.clone();
    let runner_observed = observed.clone();
    let runner: SubAgentRunner = Arc::new(move |_, spec| {
        let dir = runner_dir.clone();
        let observed = runner_observed.clone();
        Box::pin(async move {
            std::fs::create_dir_all(&dir).unwrap();
            spec.cancel.cancelled().await;
            let _ = std::fs::remove_dir_all(&dir);
            observed.store(true, std::sync::atomic::Ordering::SeqCst);
            SubAgentResult {
                status: SubAgentStatus::Failed,
                stop_reason: SubAgentStopReason::Error,
                diagnostic: Some("cancelled during setup".into()),
                elapsed_ms: 0,
                usage_known: true,
                elapsed_known: true,
                thinking: String::new(),
                text: String::new(),
                usage: Default::default(),
            }
        })
    });
    let processed = coordinator.process_with_runner(vec![result], runner).await;
    assert!(
        processed[0]
            .content
            .contains("sub_agent_timeout phase=setup"),
        "{}",
        processed[0].content
    );
    assert!(
        observed.load(std::sync::atomic::Ordering::SeqCst),
        "the setup runner must observe cancellation before the timeout result is published"
    );
    assert!(
        !dir.exists(),
        "setup cancellation must remove the partial directory"
    );
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

#[tokio::test]
async fn twelve_sub_agents_queue_and_complete_in_input_order() -> anyhow::Result<()> {
    let h = harness_with("sub-queue", false, 10).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut calls = Vec::new();
    for idx in 0..12 {
        let mut result = internal_result("SubAgent");
        result.spawns_sub_agent = true;
        result.sub_agent_prompt = Some(format!("task {idx}"));
        calls.push(result);
    }
    let runner: SubAgentRunner = Arc::new(|_, spec| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
                stop_reason: SubAgentStopReason::Completed,
                diagnostic: None,
                elapsed_ms: 0,
                usage_known: true,
                elapsed_known: true,
                thinking: String::new(),
                text: format!("done {}", spec.prompt),
                usage: Default::default(),
            }
        })
    });
    let processed = coordinator.process_with_runner(calls, runner).await;
    assert_eq!(processed.len(), 12);
    for (idx, result) in processed.iter().enumerate() {
        assert!(result.succeeded(), "slot {idx}: {}", result.content);
        assert!(
            result.content.contains(&format!("done task {idx}")),
            "slot {idx}: {}",
            result.content
        );
        assert!(!result.content.contains("limit_reached"));
    }
    Ok(())
}

#[tokio::test]
async fn sub_agent_executor_with_mock_llm_captures_child_output() -> anyhow::Result<()> {
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Text(TextEvent {
                content: "child answer".into(),
            })),
            Ok(Event::Usage(UsageEvent {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: 3,
                cache_creation_input_tokens: 1,
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]],
    ));
    let h = harness_with_backend("sub-executor-mock", llm.clone()).await?;
    h.ctx.store.add_user("parent context").await?;
    let parent = h.ctx.clone();
    let executor = SubAgentExecutor::new(
        parent.clone(),
        "sub_mock".into(),
        true,
        parent.config.clone(),
    )
    .await?;
    let result = executor.execute("child task".into()).await;
    assert_eq!(result.status, SubAgentStatus::Succeeded);
    assert_eq!(result.text, "child answer");
    assert!(
        result.thinking.is_empty(),
        "unexpected thinking: {}",
        result.thinking
    );
    assert_eq!(h.ctx.store.lines().await?.len(), 1);
    let records = h.ctx.usage.all_records()?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, crate::session::usage::UsageKind::SubAgent);
    assert_eq!(records[0].origin_session_id, "sub_mock");
    Ok(())
}

#[tokio::test]
#[ignore = "requires MINK_REAL_API=1 and DEEPSEEK_API_KEY"]
async fn real_deepseek_api_smoke_streams_response() -> anyhow::Result<()> {
    if std::env::var("MINK_REAL_API").ok().as_deref() != Some("1") {
        eprintln!("skipping real API regression: set MINK_REAL_API=1");
        return Ok(());
    }
    let api_key = match std::env::var("DEEPSEEK_API_KEY") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => {
            eprintln!("skipping real API regression: DEEPSEEK_API_KEY is not set");
            return Ok(());
        }
    };
    let h = harness("real-api").await?;
    let base_url = std::env::var("DEEPSEEK_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com/v1".to_string());
    let api_url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let messages = vec![json!({"role":"user","content":"Reply with one short word: pong"})];
    let response = OpenAiCompatibleBackend::deepseek_defaults()
        .stream(LlmRequest {
            purpose: LlmPurpose::Agent,
            model: "deepseek-v4-flash".into(),
            model_alias: Some("flash".into()),
            api_url,
            api_key,
            system_prompt: "You are a concise regression smoke test.".into(),
            messages: Arc::new(messages),
            tools: Arc::new(Vec::new()),
            max_tokens: h.ctx.max_tokens(),
            cancel: h.ctx.cancel.clone(),
            verbose: h.ctx.verbose(),
            display: h.ctx.display.clone(),
        })
        .await?;
    let mut stream = response.events;
    let mut saw_text = false;
    let mut saw_stop = false;
    while let Some(event) = futures::StreamExt::next(&mut stream).await {
        match event? {
            Event::Text(text) if !text.content.trim().is_empty() => saw_text = true,
            Event::Stop(_) => {
                saw_stop = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_text, "real API stream did not yield text");
    assert!(saw_stop, "real API stream did not yield stop");
    Ok(())
}

#[tokio::test]
async fn interrupted_child_reports_interrupted_not_success() -> anyhow::Result<()> {
    let h = harness_with("child-interrupt-status", false, 30).await?;
    let child = SubAgentExecutor::new(
        h.ctx.clone(),
        "interrupted-child".into(),
        false,
        h.ctx.config.clone(),
    )
    .await?;
    h.ctx.interrupt.store(true, Ordering::SeqCst);

    let result = tokio::time::timeout(Duration::from_secs(3), child.execute("task".into()))
        .await
        .expect("interrupted child must return promptly");

    assert_eq!(
        result.status,
        SubAgentStatus::Interrupted,
        "interrupted child must report Interrupted, got {:?}",
        result
    );
    Ok(())
}

#[tokio::test]
async fn coordinator_maps_sub_agent_terminal_states_to_tool_status() -> anyhow::Result<()> {
    use crate::tools::metadata::{ToolFailureKind, ToolStatus};

    let cases = [
        (SubAgentStatus::Interrupted, ToolStatus::Interrupted),
        (
            SubAgentStatus::TimedOut,
            ToolStatus::Failed(ToolFailureKind::Timeout),
        ),
        (
            SubAgentStatus::Failed,
            ToolStatus::Failed(ToolFailureKind::Unknown),
        ),
        (SubAgentStatus::Succeeded, ToolStatus::Succeeded),
    ];
    for (terminal, expected) in cases {
        let h = harness_with("child-status-map", false, 30).await?;
        let coordinator = SubAgentCoordinator::new(h.ctx.clone());
        let mut result = internal_result("SubAgent");
        result.spawns_sub_agent = true;
        result.sub_agent_prompt = Some("task".into());
        let runner: SubAgentRunner = Arc::new(move |_, _| {
            Box::pin(async move {
                SubAgentResult {
                    status: terminal,
                    stop_reason: SubAgentStopReason::Error,
                    diagnostic: None,
                    elapsed_ms: 0,
                    usage_known: true,
                    elapsed_known: true,
                    thinking: String::new(),
                    text: "child output".into(),
                    usage: Default::default(),
                }
            })
        });
        let processed = coordinator.process_with_runner(vec![result], runner).await;
        assert_eq!(processed.len(), 1);
        assert_eq!(
            processed[0].status, expected,
            "terminal {terminal:?} mapped incorrectly: {}",
            processed[0].content
        );
    }
    Ok(())
}

#[tokio::test]
async fn interrupt_wakes_pending_subagent_collection() -> anyhow::Result<()> {
    let h = harness_with("pending-sub-interrupt", false, 30).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("pending".into());

    // Deterministic phase barrier: the runner signals it is parked, so the
    // assertion does not guess the start moment with a sleep.
    let entered = Arc::new(tokio::sync::Notify::new());
    let entered_runner = entered.clone();
    let runner: SubAgentRunner = Arc::new(move |_, _| {
        let entered = entered_runner.clone();
        Box::pin(async move {
            entered.notify_one();
            futures::future::pending::<SubAgentResult>().await
        })
    });

    let mut task =
        tokio::spawn(async move { coordinator.process_with_runner(vec![result], runner).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
    h.ctx.interrupt.store(true, Ordering::SeqCst);

    let completed = tokio::time::timeout(Duration::from_millis(800), &mut task).await;
    let processed = match completed {
        Ok(Ok(processed)) => processed,
        Ok(Err(error)) => panic!("collection task failed: {error}"),
        Err(_) => {
            task.abort();
            let _ = task.await;
            panic!("collection did not observe interrupt while a child remained pending");
        }
    };
    assert_eq!(processed.len(), 1);
    assert_eq!(
        processed[0].status,
        crate::tools::metadata::ToolStatus::Interrupted,
        "pending children must map interrupt to Interrupted, not ToolFailed"
    );
    assert!(
        processed[0].signals.is_empty(),
        "capacity failure signal leaked"
    );
    assert!(!processed[0].content.contains("limit_reached"));
    let metadata = processed[0]
        .state_metadata
        .as_ref()
        .expect("pending result metadata");
    assert_eq!(metadata["sub_agent"]["status"], "cancelled");
    assert_eq!(metadata["sub_agent"]["stop_reason"], "aborted");
    assert_eq!(metadata["sub_agent"]["label"], "pending");
    assert_eq!(metadata["sub_agent"]["depth"], 1);
    assert_eq!(metadata["sub_agent"]["usage_known"], false);
    assert_eq!(metadata["sub_agent"]["elapsed_known"], false);
    Ok(())
}

#[tokio::test]
async fn runtime_cancel_wakes_pending_subagent_collection() -> anyhow::Result<()> {
    let h = harness_with("pending-sub-cancel", false, 30).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("pending".into());

    let entered = Arc::new(tokio::sync::Notify::new());
    let entered_runner = entered.clone();
    let runner: SubAgentRunner = Arc::new(move |_, _| {
        let entered = entered_runner.clone();
        Box::pin(async move {
            entered.notify_one();
            futures::future::pending::<SubAgentResult>().await
        })
    });

    let mut task =
        tokio::spawn(async move { coordinator.process_with_runner(vec![result], runner).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
    h.ctx.cancel.cancel();

    let completed = tokio::time::timeout(Duration::from_millis(800), &mut task).await;
    if completed.is_err() {
        task.abort();
        let _ = task.await;
    }
    assert!(
        completed.is_ok(),
        "collection did not observe runtime cancel while a child remained pending"
    );
    Ok(())
}

#[tokio::test]
async fn dropping_collection_releases_pending_subagent_task() -> anyhow::Result<()> {
    struct NotifyOnDrop(Arc<tokio::sync::Notify>);
    impl Drop for NotifyOnDrop {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    let h = harness_with("pending-sub-drop", false, 30).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("pending".into());

    let entered = Arc::new(tokio::sync::Notify::new());
    let released = Arc::new(tokio::sync::Notify::new());
    let runner_entered = entered.clone();
    let runner_released = released.clone();
    let runner: SubAgentRunner = Arc::new(move |_, _| {
        let entered = runner_entered.clone();
        let released = runner_released.clone();
        Box::pin(async move {
            let _guard = NotifyOnDrop(released);
            entered.notify_one();
            futures::future::pending::<SubAgentResult>().await
        })
    });

    let task =
        tokio::spawn(async move { coordinator.process_with_runner(vec![result], runner).await });
    tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
    task.abort();
    let _ = task.await;

    tokio::time::timeout(Duration::from_secs(2), released.notified())
        .await
        .expect("aborted collection must release the spawned child task");
    Ok(())
}

#[tokio::test]
async fn sub_agent_results_follow_input_order_and_count_usage_per_child() -> anyhow::Result<()> {
    let h = harness_with("sub-order", false, 30).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut normal = internal_result("Read");
    normal.content = "read ok".into();
    let mut first = internal_result("SubAgent");
    first.spawns_sub_agent = true;
    first.sub_agent_prompt = Some("slow".into());
    let mut second = internal_result("SubAgent");
    second.spawns_sub_agent = true;
    second.sub_agent_prompt = Some("fast".into());

    let runner: SubAgentRunner = Arc::new(|_, spec| {
        Box::pin(async move {
            let prompt = spec.prompt;
            let (delay, input_tokens) = if prompt == "slow" {
                (120, 10)
            } else {
                (10, 20)
            };
            tokio::time::sleep(Duration::from_millis(delay)).await;
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
                stop_reason: SubAgentStopReason::Completed,
                diagnostic: None,
                elapsed_ms: 0,
                usage_known: true,
                elapsed_known: true,
                thinking: String::new(),
                text: format!("answer for {prompt}"),
                usage: crate::session::stats::Stats {
                    agent_request_count: 1,
                    total_input_tokens: input_tokens,
                    total_output_tokens: 1,
                    ..Default::default()
                },
            }
        })
    });

    let processed = coordinator
        .process_with_runner(vec![normal, first, second], runner)
        .await;

    assert_eq!(processed.len(), 3);
    assert_eq!(processed[0].tool_name, "Read");
    assert_eq!(processed[0].content, "read ok");
    assert!(
        processed[1].content.contains("answer for slow"),
        "{}",
        processed[1].content
    );
    assert!(
        processed[2].content.contains("answer for fast"),
        "{}",
        processed[2].content
    );
    assert!(processed.iter().all(|result| result.succeeded()));

    // Each child is accounted exactly once (completion order differs):
    // request_count counts children, token totals sum both.
    let stats = h.ctx.stats.snapshot().await;
    assert_eq!(stats.sub_agent_request_count, 2);
    assert_eq!(stats.total_input_tokens, 30);
    assert_eq!(stats.total_output_tokens, 2);
    Ok(())
}

#[tokio::test]
async fn timed_out_child_does_not_reuse_parent_usage() -> anyhow::Result<()> {
    let h = harness_with("sub-timeout-parent-stats", false, 1).await?;
    h.ctx
        .stats
        .record_usage(&crate::protocol::UsageEvent {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 2,
            cache_creation_input_tokens: 1,
        })
        .await;
    let before = h.ctx.stats.snapshot().await;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("slow task".into());
    let runner: SubAgentRunner = Arc::new(|_, _| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
                stop_reason: SubAgentStopReason::Completed,
                diagnostic: None,
                elapsed_ms: 0,
                usage_known: true,
                elapsed_known: true,
                thinking: String::new(),
                text: "late".into(),
                usage: crate::session::stats::Stats {
                    agent_request_count: 99,
                    total_input_tokens: 999,
                    total_output_tokens: 999,
                    ..Default::default()
                },
            }
        })
    });
    let processed = coordinator.process_with_runner(vec![result], runner).await;
    assert!(!processed[0].succeeded());
    assert!(
        processed[0].content.contains("(in=0, out=0)"),
        "timeout usage must not inherit parent stats: {}",
        processed[0].content
    );
    let metadata = processed[0]
        .state_metadata
        .as_ref()
        .expect("sub-agent metadata");
    assert_eq!(metadata["sub_agent"]["input_tokens"], 0);
    assert_eq!(metadata["sub_agent"]["output_tokens"], 0);
    assert_eq!(metadata["sub_agent"]["usage_known"], false);
    assert!(metadata["sub_agent"].get("elapsed_known").is_none());
    let after = h.ctx.stats.snapshot().await;
    assert_eq!(after.agent_request_count, before.agent_request_count);
    assert_eq!(after.total_input_tokens, before.total_input_tokens);
    assert_eq!(after.total_output_tokens, before.total_output_tokens);
    Ok(())
}

#[tokio::test]
async fn slow_child_timeout_does_not_cancel_its_sibling() -> anyhow::Result<()> {
    let h = harness_with("sub-slow-sibling", false, 1).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut slow = internal_result("SubAgent");
    slow.spawns_sub_agent = true;
    slow.sub_agent_prompt = Some("slow".into());
    let mut fast = internal_result("SubAgent");
    fast.spawns_sub_agent = true;
    fast.sub_agent_prompt = Some("fast".into());
    let runner: SubAgentRunner = Arc::new(|_, spec| {
        Box::pin(async move {
            if spec.prompt == "slow" {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
                stop_reason: SubAgentStopReason::Completed,
                diagnostic: None,
                elapsed_ms: 0,
                usage_known: true,
                elapsed_known: true,
                thinking: String::new(),
                text: format!("done {}", spec.prompt),
                usage: Default::default(),
            }
        })
    });
    let processed = coordinator
        .process_with_runner(vec![slow, fast], runner)
        .await;
    assert!(
        processed[0].content.contains("] timed_out"),
        "{}",
        processed[0].content
    );
    assert!(!processed[0].succeeded());
    assert!(processed[1].succeeded(), "{}", processed[1].content);
    assert!(processed[1].content.contains("done fast"));
    Ok(())
}

#[tokio::test]
async fn queued_child_gets_its_full_budget_after_admission() -> anyhow::Result<()> {
    let h = harness_with("sub-queue-budget", false, 2).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut calls = Vec::new();
    for idx in 0..9 {
        let mut result = internal_result("SubAgent");
        result.spawns_sub_agent = true;
        result.sub_agent_prompt = Some(format!("task {idx}"));
        calls.push(result);
    }
    let runner: SubAgentRunner = Arc::new(|_, spec| {
        Box::pin(async move {
            // The ninth child waits for a permit (~500ms), then needs 1.8s;
            // the old shared 2s batch deadline would have killed it.
            let delay = if spec.prompt == "task 8" { 1800 } else { 500 };
            tokio::time::sleep(Duration::from_millis(delay)).await;
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
                stop_reason: SubAgentStopReason::Completed,
                diagnostic: None,
                elapsed_ms: 0,
                usage_known: true,
                elapsed_known: true,
                thinking: String::new(),
                text: format!("done {}", spec.prompt),
                usage: Default::default(),
            }
        })
    });
    let processed = coordinator.process_with_runner(calls, runner).await;
    assert_eq!(processed.len(), 9);
    for (idx, result) in processed.iter().enumerate() {
        assert!(result.succeeded(), "slot {idx}: {}", result.content);
    }
    let elapsed = |idx: usize| {
        processed[idx]
            .state_metadata
            .as_ref()
            .and_then(|meta| meta["sub_agent"]["elapsed_ms"].as_u64())
            .expect("elapsed_ms")
    };
    // Permit starts the clock: the first wave runs ~500ms, while the queued
    // ninth child runs ~1800ms after admission. Use relative, CI-tolerant
    // bounds rather than pinning scheduler latency.
    assert!(elapsed(0) >= 300, "first={}", elapsed(0));
    assert!(elapsed(0) <= 1500, "first={}", elapsed(0));
    assert!(elapsed(8) >= 1500, "ninth={}", elapsed(8));
    assert!(elapsed(8) <= 2500, "ninth={}", elapsed(8));
    assert!(
        elapsed(8) > elapsed(0),
        "ninth={} first={}",
        elapsed(8),
        elapsed(0)
    );
    Ok(())
}

#[tokio::test]
async fn nested_delegation_is_allowed_when_depth_budget_remains() -> anyhow::Result<()> {
    let h = harness_with_config(
        "sub-depth2",
        false,
        30,
        |config| {
            config.sub_agent_max_depth = 2;
        },
        None,
    )
    .await?;
    let child = SubAgentExecutor::new(
        h.ctx.clone(),
        "depth2-child".into(),
        false,
        h.ctx.config.clone(),
    )
    .await?;
    assert_eq!(child.context().sub_agent_depth, 1);
    assert!(
        child.context().tool_surface.has("SubAgent"),
        "a child below the configured maximum must retain SubAgent"
    );

    let coordinator = SubAgentCoordinator::new(child.context().clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("grandchild task".into());
    let runner: SubAgentRunner = Arc::new(|_, spec| {
        assert_eq!(spec.depth, 2);
        Box::pin(async move {
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
                stop_reason: SubAgentStopReason::Completed,
                diagnostic: None,
                elapsed_ms: 0,
                usage_known: true,
                elapsed_known: true,
                thinking: String::new(),
                text: "grandchild done".into(),
                usage: Default::default(),
            }
        })
    });
    let processed = coordinator.process_with_runner(vec![result], runner).await;
    assert!(processed[0].succeeded(), "{}", processed[0].content);
    Ok(())
}

#[tokio::test]
async fn sub_agent_model_is_forwarded_and_invalid_values_do_not_spawn() -> anyhow::Result<()> {
    let h = harness_with("sub-model", false, 30).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());

    let mut selected = internal_result("SubAgent");
    selected.spawns_sub_agent = true;
    selected.sub_agent_prompt = Some("task".into());
    selected.sub_agent_model = Some("pro".into());
    let runner: SubAgentRunner = Arc::new(|_, spec| {
        assert_eq!(spec.model.as_deref(), Some("pro"));
        Box::pin(async move {
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
                stop_reason: SubAgentStopReason::Completed,
                diagnostic: None,
                elapsed_ms: 0,
                usage_known: true,
                elapsed_known: true,
                thinking: String::new(),
                text: "model ok".into(),
                usage: Default::default(),
            }
        })
    });
    let processed = coordinator
        .process_with_runner(vec![selected], runner)
        .await;
    assert!(processed[0].succeeded(), "{}", processed[0].content);

    let mut invalid = internal_result("SubAgent");
    invalid.spawns_sub_agent = true;
    invalid.sub_agent_prompt = Some("task".into());
    invalid.sub_agent_model = Some("not-a-configured-model".into());
    let runner: SubAgentRunner = Arc::new(|_, _| {
        panic!("an invalid model must not start a child");
    });
    let processed = coordinator.process_with_runner(vec![invalid], runner).await;
    assert_eq!(
        processed[0].status,
        crate::tools::metadata::ToolStatus::Failed(
            crate::tools::metadata::ToolFailureKind::ArgumentInvalid
        )
    );
    assert!(processed[0].content.contains("sub_agent_model_not_allowed"));
    assert_eq!(
        processed[0].failure_source,
        Some(crate::tools::metadata::ToolFailureSource::ModelFormat)
    );
    Ok(())
}
