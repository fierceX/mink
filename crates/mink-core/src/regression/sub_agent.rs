use super::*;

#[tokio::test]
async fn sub_agent_recursion_is_rejected_without_running_child() -> anyhow::Result<()> {
    let h = harness_with("sub-recursion", true, 300).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("nested task".into());
    let runner: SubAgentRunner = Arc::new(|_, _, _, _, _| {
        Box::pin(async {
            panic!("runner must not execute when recursion is blocked");
        })
    });
    let processed = coordinator.process_with_runner(vec![result], runner).await;
    assert_eq!(processed.len(), 1);
    assert!(processed[0].content.contains("recursion blocked"));
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
    let runner: SubAgentRunner = Arc::new(|_, _, _, _, _| {
        Box::pin(async {
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
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
    assert!(processed[0].content.contains("Thinking: child thought"));
    assert!(processed[0].content.contains("Text: child text"));
    assert!(processed[0].succeeded());
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
    let runner: SubAgentRunner = Arc::new(|_, _, _, _, _| {
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
            .contains("Sub-agent task panicked: panic from test runner"),
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
    let runner: SubAgentRunner = Arc::new(|_, _, _, _, _| {
        panic!("sync panic from test runner");
    });
    let processed = coordinator.process_with_runner(vec![result], runner).await;

    assert_eq!(processed.len(), 1);
    assert!(processed[0].content.contains("] failed (in=0, out=0)"));
    assert!(
        processed[0]
            .content
            .contains("Sub-agent task panicked: sync panic from test runner"),
        "{}",
        processed[0].content
    );
    Ok(())
}

#[tokio::test]
async fn sub_agent_timeout_marks_incomplete() -> anyhow::Result<()> {
    let h = harness_with("sub-timeout", false, 0).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut result = internal_result("SubAgent");
    result.spawns_sub_agent = true;
    result.sub_agent_prompt = Some("slow task".into());
    let runner: SubAgentRunner = Arc::new(|_, _, _, _, _| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
                thinking: String::new(),
                text: "late".into(),
                usage: Default::default(),
            }
        })
    });
    let processed = coordinator.process_with_runner(vec![result], runner).await;
    assert_eq!(processed[0].content, "Sub-agent timed out after 0s.");
    assert!(!processed[0].succeeded());
    Ok(())
}

#[tokio::test]
async fn sub_agent_collection_enters_timeout_even_when_more_than_limit_are_launched()
-> anyhow::Result<()> {
    let h = harness_with("sub-timeout-many", false, 0).await?;
    let coordinator = SubAgentCoordinator::new(h.ctx.clone());
    let mut calls = Vec::new();
    for idx in 0..9 {
        let mut result = internal_result("SubAgent");
        result.spawns_sub_agent = true;
        result.sub_agent_prompt = Some(format!("slow task {idx}"));
        calls.push(result);
    }
    let runner: SubAgentRunner = Arc::new(|_, _, _, _, _| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
                thinking: String::new(),
                text: "late".into(),
                usage: Default::default(),
            }
        })
    });
    let processed = tokio::time::timeout(
        Duration::from_millis(100),
        coordinator.process_with_runner(calls, runner),
    )
    .await?;
    assert_eq!(processed.len(), 9);
    assert!(
        processed
            .iter()
            .all(|r| r.content == "Sub-agent timed out after 0s.")
    );
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
            messages,
            tools: Vec::new(),
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
        let runner: SubAgentRunner = Arc::new(move |_, _, _, _, _| {
            Box::pin(async move {
                SubAgentResult {
                    status: terminal,
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
    let runner: SubAgentRunner = Arc::new(move |_, _, _, _, _| {
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
    if completed.is_err() {
        task.abort();
        let _ = task.await;
    }
    assert!(
        completed.is_ok(),
        "collection did not observe interrupt while a child remained pending"
    );
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
    let runner: SubAgentRunner = Arc::new(move |_, _, _, _, _| {
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
    let runner: SubAgentRunner = Arc::new(move |_, _, _, _, _| {
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

    let runner: SubAgentRunner = Arc::new(|_, _, prompt, _, _| {
        Box::pin(async move {
            let (delay, input_tokens) = if prompt == "slow" {
                (120, 10)
            } else {
                (10, 20)
            };
            tokio::time::sleep(Duration::from_millis(delay)).await;
            SubAgentResult {
                status: SubAgentStatus::Succeeded,
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
