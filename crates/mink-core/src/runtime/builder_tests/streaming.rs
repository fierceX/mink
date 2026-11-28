use super::*;

#[tokio::test]
async fn web_commands_respect_the_committed_input_revision() {
    use crate::agent::orchestrator::TurnStatus;
    use crate::runtime::HumanInput;
    let home = unique_temp_dir("web-revision-home");
    let cwd = unique_temp_dir("web-revision-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    config.config.web.client = true;
    let runtime = build_runtime(config).await.unwrap();
    let handle = runtime.handle();
    for (id, edit) in [
        ("edited", Some("/web-fetch file:///edited".to_string())),
        ("withdrawn", None),
    ] {
        let (receipt, stream) = handle
            .stream_input(HumanInput {
                request_id: id.into(),
                text: "/web-fetch file:///original".into(),
                target_turn_id: None,
                attachment_ids: vec![],
            })
            .unwrap();
        handle
            .input_inbox()
            .update(id, receipt.revision, edit)
            .unwrap();
        let mut stream = stream.unwrap();
        while stream.recv().await.is_some() {}
        let outcome = stream.outcome().await.unwrap();
        assert_eq!(
            outcome.status,
            if id == "edited" {
                TurnStatus::Failed
            } else {
                TurnStatus::Interrupted
            }
        );
    }
    let rows = runtime.ctx.stores.store.lines().await.unwrap();
    let inputs: Vec<_> = rows
        .iter()
        .filter_map(|row| row["content"].as_array())
        .flatten()
        .filter(|block| block["name"] == "WebFetch")
        .map(|block| block["input"]["url"].as_str().unwrap())
        .collect();
    assert_eq!(inputs, ["file:///edited"]);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_completed_old_stream_does_not_interrupt_new_turn() {
    let home = unique_temp_dir("mock-drop-completed-home");
    let cwd = unique_temp_dir("mock-drop-completed-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let mut old = runtime.stream_turn("first").unwrap();
    while old.recv().await.is_some() {}
    let second = runtime.stream_turn("second").unwrap();
    drop(old);
    let outcome = second.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert_eq!(outcome.text, "second");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn runtime_start_rejects_compaction_boundary_beyond_history() {
    let home = unique_temp_dir("invalid-compaction-boundary-home");
    let cwd = unique_temp_dir("invalid-compaction-boundary-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let first = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let session = first.session_info().clone();
    first.shutdown().await.unwrap();
    tokio::fs::write(
        session.summary_path.with_file_name("context-state.json"),
        r#"{"active_start":1,"summary":"bad"}"#,
    )
    .await
    .unwrap();

    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    config.session = SessionPolicy::Resume(session.session_id.clone());
    let error = build_runtime(config).await.err().unwrap().to_string();
    assert!(error.contains("exceeds history length"), "{error}");

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn interrupt_mid_turn_returns_interrupted_and_next_turn_works() {
    let home = unique_temp_dir("mock-int-home");
    let cwd = unique_temp_dir("mock-int-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let (config, entered, block) = runtime_config_with_blocking_mock(&home, &cwd);
    let runtime = build_runtime(config).await.unwrap();

    let runtime_handle = runtime.handle();
    let turn_handle = runtime_handle.clone();
    let task = tokio::spawn(async move { turn_handle.run_turn("blocking turn").await });

    // Deterministic phase barrier: the backend signals when the turn reaches
    // the LLM stream call (no sleep-based guessing).
    tokio::time::timeout(tokio::time::Duration::from_secs(2), entered.notified())
        .await
        .expect("turn must reach the LLM stream call");

    // The orchestrator's turn executor polls this flag every 25 ms.
    runtime_handle.interrupt_current_turn();

    let outcome = task.await.unwrap().unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );

    // Release the backend so the next turn can complete promptly.
    block.store(false, std::sync::atomic::Ordering::SeqCst);
    // Next turn must still run successfully.
    let outcome2 = runtime.run_turn("recovery turn").await.unwrap();
    assert_eq!(outcome2.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn interrupt_during_stream_establishment_returns_interrupted_and_next_turn_works() {
    let home = unique_temp_dir("pending-establish-home");
    let cwd = unique_temp_dir("pending-establish-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let backend = Arc::new(PendingEstablishBackend::default());
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        backend.clone(),
        |_| {},
    ))
    .await
    .unwrap();

    let handle = runtime.handle();
    let turn_handle = handle.clone();
    let task = tokio::spawn(async move { turn_handle.run_turn("blocking establish").await });
    // Deterministic barrier: wait until the non-returning stream() is entered.
    tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        backend.entered.notified(),
    )
    .await
    .expect("request must reach backend.stream()");
    handle.interrupt_current_turn();

    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(2), task)
        .await
        .expect("interrupt during establishment must end the turn")
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );
    assert!(*backend.calls.lock().unwrap() >= 1);
    let unknown = outcome
        .usage_records
        .iter()
        .filter(|record| record.status == crate::session::usage::UsageStatus::Unreported)
        .collect::<Vec<_>>();
    assert_eq!(
        unknown.len(),
        1,
        "interrupted establishment must leave one unknown-usage record: {:?}",
        outcome.usage_records
    );
    assert!(unknown[0].tokens.is_none());

    let outcome2 = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        runtime.run_turn("after interrupt"),
    )
    .await
    .expect("next turn must start")
    .unwrap();
    assert_eq!(outcome2.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn interrupt_with_disabled_first_event_timeout_still_ends_establishment() {
    let home = unique_temp_dir("pending-no-timeout-home");
    let cwd = unique_temp_dir("pending-no-timeout-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let backend = Arc::new(PendingEstablishBackend::default());
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        backend.clone(),
        |cfg| {
            cfg.llm_first_event_timeout_secs = 0;
            cfg.llm_idle_timeout_secs = 0;
        },
    ))
    .await
    .unwrap();

    let handle = runtime.handle();
    let turn_handle = handle.clone();
    let task = tokio::spawn(async move { turn_handle.run_turn("blocking establish").await });
    tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        backend.entered.notified(),
    )
    .await
    .expect("request must reach backend.stream()");
    handle.interrupt_current_turn();

    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(2), task)
        .await
        .expect("interrupt must work even when the timeout is disabled")
        .unwrap()
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
async fn first_event_deadline_survives_stream_establishment() {
    let home = unique_temp_dir("deadline-home");
    let cwd = unique_temp_dir("deadline-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(SlowEstablishBackend),
        |cfg| {
            cfg.llm_first_event_timeout_secs = 1;
            // Single attempt: this test pins the per-attempt deadline, not the
            // retry budget (covered by dedicated recovery tests).
            cfg.llm_recovery.request_max_retries = 0;
        },
    ))
    .await
    .unwrap();

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_secs(3),
        runtime.run_turn("slow establish"),
    )
    .await
    .expect("turn must fail once the first-event budget is exhausted")
    .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed
    );
    assert_eq!(
        outcome.failure,
        Some(crate::runtime::TurnFailureReason::RequestRetryExhausted)
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "establishment must consume the same first-event budget instead of \
         restarting it: {elapsed:?}"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn interrupt_during_response_header_wait_ends_turn() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let server = {
        let accepted = accepted.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted.store(true, std::sync::atomic::Ordering::SeqCst);
                        // Hold the connection open without sending headers.
                        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(3));
                        drop(stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        })
    };

    let home = unique_temp_dir("header-wait-home");
    let cwd = unique_temp_dir("header-wait-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: format!("http://{addr}"),
        max_context_tokens: 1_000_000,
        log_events: true,
        ..Config::default()
    };
    let runtime = build_runtime(AgentRuntimeConfig::from_config(
        cfg,
        home.clone(),
        cwd.clone(),
    ))
    .await
    .unwrap();

    let handle = runtime.handle();
    let turn_handle = handle.clone();
    let task = tokio::spawn(async move { turn_handle.run_turn("wait for headers").await });
    // Wait until the request actually reached the local server, so the
    // interrupt lands in the response-header wait window.
    for _ in 0..200 {
        if accepted.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    assert!(
        accepted.load(std::sync::atomic::Ordering::SeqCst),
        "request never reached the local server"
    );
    handle.interrupt_current_turn();

    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(3), task)
        .await
        .expect("interrupt during header wait must end the turn")
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );

    let _ = release_tx.send(());
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = server.join();
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// manual 压缩：摘要不可用且投影本来就超预算时，转确定性应急 checkpoint，
/// 而不是返回 Skipped 让调用方继续带着超窗投影。
#[tokio::test]
async fn manual_compaction_falls_back_to_emergency_checkpoint() {
    let home = unique_temp_dir("manual-emergency-home");
    let cwd = unique_temp_dir("manual-emergency-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        max_context_tokens: 40_000,
        context_reserve_tokens: 8_000,
        context_compact_pct: 100,
        context_compact_tail_tokens: 1_000,
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(FailingSummaryBackend)),
    )
    .await
    .unwrap();
    for index in 0..4 {
        runtime
            .ctx
            .stores
            .store
            .add_user(&format!("request {index}: {}", "x".repeat(60_000)))
            .await
            .unwrap();
        runtime
            .ctx
            .stores
            .store
            .add_assistant(
                &format!("progress {index}: {}", "y".repeat(60_000)),
                "",
                &[],
            )
            .await
            .unwrap();
    }

    let outcome = runtime.compact().await.unwrap();
    assert!(
        matches!(outcome, crate::runtime::CompactOutcome::Compacted { .. }),
        "manual 压缩必须落到应急 checkpoint：{outcome:?}"
    );
    let summary = runtime
        .ctx
        .stores
        .compaction
        .read_summary()
        .await
        .unwrap_or_default();
    assert!(
        summary.contains("emergency-context-excerpt"),
        "投影必须是应急摘录：{summary}"
    );
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn interrupt_manual_compaction_releases_gate_for_next_turn() {
    let home = unique_temp_dir("compact-interrupt-home");
    let cwd = unique_temp_dir("compact-interrupt-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        max_context_tokens: 64_000,
        context_reserve_tokens: 8_000,
        context_compact_tail_tokens: 1,
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(InterruptibleCompactionBackend)),
    )
    .await
    .unwrap();
    for index in 0..3 {
        runtime
            .ctx
            .stores
            .store
            .add_user(&format!("request {index}: {}", "x".repeat(2_000)))
            .await
            .unwrap();
        runtime
            .ctx
            .stores
            .store
            .add_assistant(&format!("progress {index}: {}", "y".repeat(2_000)), "", &[])
            .await
            .unwrap();
    }
    let handle = runtime.handle();
    let compact_handle = handle.clone();
    let compact = tokio::spawn(async move { compact_handle.compact().await });
    tokio::time::sleep(tokio::time::Duration::from_millis(40)).await;
    handle.interrupt_current_turn();
    let error = tokio::time::timeout(tokio::time::Duration::from_secs(2), compact)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err()
        .to_string();
    assert!(error.contains("compaction interrupted"), "{error}");

    let outcome = runtime.run_turn("still works").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// manual 应急路径的原始错误必须交付给调用方一次，而且 actor
/// 不能因此退出（第二次命令得到同一原始诊断，而不是 channel closed）。
#[tokio::test]
async fn manual_error_is_delivered_without_dropping_actor() {
    let home = unique_temp_dir("manual-error-home");
    let cwd = unique_temp_dir("manual-error-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        max_context_tokens: 40_000,
        context_reserve_tokens: 8_000,
        context_compact_tail_tokens: 256_000,
        llm_recovery: crate::config::LlmRecoveryPolicy {
            request_max_retries: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(FailingSummaryBackend)),
    )
    .await
    .unwrap();
    runtime
        .ctx
        .stores
        .store
        .add_user(&"x".repeat(150_000))
        .await
        .unwrap();
    runtime
        .ctx
        .stores
        .store
        .append_runtime_message(serde_json::json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "audit-unpaired",
                "name": "Read",
                "input": {"path": "example"}
            }]
        }))
        .await
        .unwrap();

    let error = runtime.compact().await.unwrap_err().to_string();
    assert!(
        error.contains("incomplete tool exchange"),
        "original error was lost: {error}"
    );

    // actor 存活：同一命令再次返回原始诊断，而不是 channel closed。
    let second = runtime.compact().await.unwrap_err().to_string();
    assert!(
        second.contains("incomplete tool exchange"),
        "actor died after the first failure: {second}"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// 投影读取失败（plan.md 不可读）也必须交付原始错误，且 actor 存活。
#[tokio::test]
async fn manual_projection_read_failure_is_delivered_and_actor_survives() {
    use crate::protocol::{Event, StopEvent, TextEvent};
    let home = unique_temp_dir("manual-projection-home");
    let cwd = unique_temp_dir("manual-projection-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        max_context_tokens: 64_000,
        context_reserve_tokens: 8_000,
        context_compact_tail_tokens: 1,
        llm_recovery: crate::config::LlmRecoveryPolicy {
            request_max_retries: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone()).with_llm_backend(Arc::new(
            crate::llm::mock::MockLlmBackend::new(
                "flash",
                vec![vec![
                    Ok(Event::Text(TextEvent {
                        content: "compacted summary".into(),
                    })),
                    Ok(Event::Stop(StopEvent {
                        reason: "stop".into(),
                    })),
                ]],
            ),
        )),
    )
    .await
    .unwrap();
    for index in 0..3 {
        runtime
            .ctx
            .stores
            .store
            .add_user(&format!("request {index}: {}", "x".repeat(2_000)))
            .await
            .unwrap();
        runtime
            .ctx
            .stores
            .store
            .add_assistant(&format!("progress {index}: {}", "y".repeat(2_000)), "", &[])
            .await
            .unwrap();
    }
    let plan_path = runtime.handle().session.plan_path.clone();
    tokio::fs::create_dir_all(&plan_path)
        .await
        .expect("plan.md is replaced by a directory to force a read failure");

    let error = runtime.compact().await.unwrap_err().to_string();
    assert!(
        error.contains("cannot read active plan checkpoint"),
        "projection read error was lost: {error}"
    );

    // actor 存活：后续命令仍被受理。
    runtime.handle().set_model("flash").await.unwrap();

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// 闩锁会话的 manual 压缩必须在入口拒绝：fatal 诊断交付一次，不调用
/// 摘要 backend，也不写入任何状态；actor 保持存活以返回同一诊断。
#[tokio::test]
async fn manual_compaction_delivers_latched_fault_without_model_call_or_write() {
    let home = unique_temp_dir("manual-latched-home");
    let cwd = unique_temp_dir("manual-latched-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(PanicIfCalledBackend)),
    )
    .await
    .unwrap();
    let state_path = runtime
        .handle()
        .session
        .summary_path
        .with_file_name("context-state.json");
    let before = std::fs::read(&state_path).ok();
    let _ = runtime
        .ctx
        .stores
        .persistence_fault
        .raise(std::path::Path::new("todos.json"), "injected fault");

    let error = runtime.compact().await.unwrap_err().to_string();
    assert!(
        error.contains("persistence fault"),
        "latched fault was lost: {error}"
    );
    assert_eq!(
        std::fs::read(&state_path).ok(),
        before,
        "a latched manual compaction must not write state"
    );

    // actor 存活：再次命令仍返回同一 fatal 诊断（而不是 channel closed）。
    let second = runtime.compact().await.unwrap_err().to_string();
    assert!(second.contains("persistence fault"), "{second}");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn dropping_compact_caller_future_keeps_gate_busy_until_operation_ends() {
    let home = unique_temp_dir("compact-drop-home");
    let cwd = unique_temp_dir("compact-drop-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = seeded_compaction_runtime(&home, &cwd).await;
    let handle = runtime.handle();

    let mut compact = Box::pin(handle.compact());
    // First poll acquires the permit, sends the command and then waits on
    // the non-returning compaction request.
    assert!(
        tokio::time::timeout(tokio::time::Duration::from_millis(150), &mut compact)
            .await
            .is_err(),
        "compaction should still be running"
    );
    // The caller abandons waiting; the operation must keep the gate.
    drop(compact);
    let busy = match handle.stream_turn("must be busy") {
        Ok(_) => panic!("gate must stay busy while the abandoned compaction runs"),
        Err(error) => error,
    };
    assert!(
        matches!(busy, crate::runtime::RuntimeError::Busy { .. }),
        "{busy:?}"
    );

    // Interrupt ends the compaction and releases the gate.
    handle.interrupt_current_turn();
    let mut attempts = 0;
    let outcome = loop {
        match handle.stream_turn("after interrupt") {
            Ok(stream) => break stream.outcome().await.unwrap(),
            Err(_) => {
                attempts += 1;
                assert!(attempts < 250, "gate was never released after interrupt");
                tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
            }
        }
    };
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn compact_send_failure_releases_gate() {
    let home = unique_temp_dir("compact-send-fail-home");
    let cwd = unique_temp_dir("compact-send-fail-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let real = runtime.handle();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    drop(rx);
    let broken = crate::runtime::AgentRuntimeHandle {
        cmd_tx: tx,
        ..real.clone()
    };

    let error = broken.compact().await.unwrap_err();
    assert!(
        matches!(error, crate::runtime::RuntimeError::Command(_)),
        "{error:?}"
    );

    // The shared gate must be free again for the real handle.
    let outcome = runtime.run_turn("after failed send").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn compact_when_done_sender_dropped_releases_gate_and_errors() {
    let home = unique_temp_dir("compact-done-drop-home");
    let cwd = unique_temp_dir("compact-done-drop-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let real = runtime.handle();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = crate::runtime::AgentRuntimeHandle {
        cmd_tx: tx,
        ..real.clone()
    };
    let compact_handle = handle.clone();
    let task = tokio::spawn(async move { compact_handle.compact().await });

    let done = match rx.recv().await.expect("compact command must be sent") {
        crate::agent::orchestrator::OrchCmd::Compact { done } => done,
        _ => panic!("expected an OrchCmd::Compact"),
    };
    // Orchestrator drops the completion sender without reporting a result.
    drop(done);

    let result = task.await.unwrap();
    assert!(
        matches!(result, Err(crate::runtime::RuntimeError::Command(_))),
        "{result:?}"
    );

    // The shared gate must be free again for the real handle.
    let outcome = runtime.run_turn("after done drop").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn shutdown_ends_compaction_whose_caller_is_gone() {
    let home = unique_temp_dir("compact-shutdown-home");
    let cwd = unique_temp_dir("compact-shutdown-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = seeded_compaction_runtime(&home, &cwd).await;
    let handle = runtime.handle();

    let mut compact = Box::pin(handle.compact());
    assert!(
        tokio::time::timeout(tokio::time::Duration::from_millis(150), &mut compact)
            .await
            .is_err(),
        "compaction should still be running"
    );
    drop(compact);

    let result = tokio::time::timeout(tokio::time::Duration::from_secs(5), runtime.shutdown())
        .await
        .expect("shutdown must finish even when the compaction caller is gone");
    result.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn explicit_stream_cancel_releases_the_turn_gate_after_outcome() {
    let home = unique_temp_dir("mock-cancel-stream-home");
    let cwd = unique_temp_dir("mock-cancel-stream-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let (config, _entered, block) = runtime_config_with_blocking_mock(&home, &cwd);
    let runtime = build_runtime(config).await.unwrap();

    let stream = runtime.stream_turn("blocking").unwrap();
    stream.cancel();
    let cancelled = stream.outcome().await.unwrap();
    assert_eq!(
        cancelled.status,
        crate::agent::orchestrator::TurnStatus::Interrupted
    );
    // Release the backend so the recovery turn can complete promptly.
    block.store(false, std::sync::atomic::Ordering::SeqCst);
    let recovered = runtime.run_turn("recovery").await.unwrap();
    assert_eq!(
        recovered.status,
        crate::agent::orchestrator::TurnStatus::Ok,
        "{:?}",
        recovered.error
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn public_interrupt_with_writer_recovery_finishes_turn_and_allows_next_turn() {
    use crate::session::event_log::{EventLogWriter, EventLogWriterGate};

    let home = unique_temp_dir("interrupt-critical-home");
    let cwd = unique_temp_dir("interrupt-critical-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let backend = Arc::new(crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(crate::protocol::Event::Text(crate::protocol::TextEvent {
                    content: "first".into(),
                })),
                Ok(crate::protocol::Event::Stop(crate::protocol::StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            vec![
                Ok(crate::protocol::Event::Text(crate::protocol::TextEvent {
                    content: "second".into(),
                })),
                Ok(crate::protocol::Event::Stop(crate::protocol::StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    ));
    let gate = EventLogWriterGate::new();
    let writer = EventLogWriter::start_paused(home.join("stalled-events.jsonl"), gate.clone());

    let config = runtime_config_with_backend(&home, &cwd, backend, |_| {});
    let runtime = crate::runtime::context_build::TEST_EVENT_LOG_WRITER
        .scope(Some(writer.clone()), build_runtime(config))
        .await
        .unwrap();
    let handle = runtime.handle();

    // Stall the writer only after construction: the turn below must then
    // remain inside the critical acknowledgement wait.
    gate.set_paused(true);
    let first = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.run_turn("first").await })
    };

    let mut waited = 0;
    while writer.critical_queued() == 0 && waited < 400 {
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        waited += 1;
    }
    assert!(
        writer.critical_queued() >= 1,
        "the prefix commit must reach the writer queue before we interrupt"
    );

    let started = std::time::Instant::now();
    handle.interrupt_current_turn();
    // Resume the writer right after the interrupt; then this case asserts the
    // turn ends as Interrupted and the next turn runs, not a permanent stall.
    gate.set_paused(false);
    let outcome = tokio::time::timeout(tokio::time::Duration::from_secs(2), first)
        .await
        .expect("interrupt_current_turn must release the stalled critical commit")
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Interrupted,
        "interrupt must classify as Interrupted, not as an event-log error: {:?}",
        outcome.error
    );

    assert!(
        started.elapsed() < std::time::Duration::from_millis(1200),
        "interrupt must not wait for the production deadline: {:?}",
        started.elapsed()
    );
    let second = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        handle.run_turn("second"),
    )
    .await
    .expect("second turn must not hang after recovery")
    .unwrap();
    assert_eq!(
        second.status,
        crate::agent::orchestrator::TurnStatus::Ok,
        "{:?}",
        second.error
    );

    tokio::time::timeout(tokio::time::Duration::from_secs(10), runtime.shutdown())
        .await
        .expect("shutdown must not hang after recovery")
        .unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn slow_consumer_bounds_pending_progress_bytes_and_outcome_drains() {
    // Two fixed loads from the Q13 verification spec (8 MiB / 32 MiB).
    for mebibytes in [8usize, 32] {
        let home = unique_temp_dir(&format!("storm-{mebibytes}-home"));
        let cwd = unique_temp_dir(&format!("storm-{mebibytes}-cwd"));
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let chunks = mebibytes * 1024; // 1 KiB per delta
        let backend = Arc::new(TextStormBackend {
            chunks,
            chunk_bytes: 1024,
            emitted: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            finished: Arc::new(tokio::sync::Notify::new()),
        });
        // 32 MiB of deltas also produce 32k diagnostic audit events; this test
        // targets the client-side progress budget, so the audit log is off to
        // avoid queue-loss noise unrelated to the budget under test.
        let runtime = build_runtime(runtime_config_with_backend(
            &home,
            &cwd,
            backend.clone(),
            |cfg| cfg.log_events = false,
        ))
        .await
        .unwrap();

        // Slow consumer: never call recv(); wait until the producer has
        // emitted every delta so the backlog is at its peak.
        let stream = runtime.stream_turn("text storm").unwrap();
        tokio::time::timeout(
            tokio::time::Duration::from_secs(60),
            backend.finished.notified(),
        )
        .await
        .expect("producer must finish the storm");
        let emitted = backend.emitted.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            emitted, chunks,
            "producer must finish the {mebibytes} MiB storm"
        );

        let (pending, dropped) = stream.progress_backlog();
        assert!(
            pending <= 1 << 20,
            "pending progress bytes must stay within the budget at {mebibytes} MiB: {pending}"
        );
        assert!(
            dropped > 0,
            "{mebibytes} MiB of deltas against a 1 MiB budget must drop over-limit deltas"
        );

        // outcome() drains the rest: the result is available and the backlog
        // is released as the stream is consumed.
        let outcome = stream.outcome().await.unwrap();
        assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

        runtime.shutdown().await.unwrap();
        let _ = tokio::fs::remove_dir_all(home).await;
        let _ = tokio::fs::remove_dir_all(cwd).await;
    }
}

#[tokio::test]
async fn retry_does_not_bypass_first_event_deadline() {
    let home = unique_temp_dir("retry-deadline-home");
    let cwd = unique_temp_dir("retry-deadline-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(RetryThenPendingBackend { retries: 1 }),
        |cfg| {
            cfg.llm_first_event_timeout_secs = 1;
            // Single attempt: this test pins the per-attempt deadline, not the
            // retry budget (covered by dedicated recovery tests).
            cfg.llm_recovery.request_max_retries = 0;
            cfg.llm_idle_timeout_secs = 30;
        },
    ))
    .await
    .unwrap();

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        runtime.run_turn("retry then silence"),
    )
    .await
    .expect("the first-event deadline must still fire after a Retry event")
    .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed
    );
    assert_eq!(
        outcome.failure,
        Some(crate::runtime::TurnFailureReason::RequestRetryExhausted)
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "Retry must not extend the first-event budget: {elapsed:?}"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn repeated_retry_events_do_not_extend_first_event_deadline() {
    let home = unique_temp_dir("retry-multi-home");
    let cwd = unique_temp_dir("retry-multi-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(RetryThenPendingBackend { retries: 3 }),
        |cfg| {
            cfg.llm_first_event_timeout_secs = 1;
            // Single attempt: this test pins the per-attempt deadline, not the
            // retry budget (covered by dedicated recovery tests).
            cfg.llm_recovery.request_max_retries = 0;
            cfg.llm_idle_timeout_secs = 30;
        },
    ))
    .await
    .unwrap();

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        runtime.run_turn("many retries then silence"),
    )
    .await
    .expect("repeated Retry events must not bypass the first-event deadline")
    .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "repeated Retry must not extend the first-event budget: {elapsed:?}"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn retry_followed_by_real_output_still_succeeds() {
    let home = unique_temp_dir("retry-output-home");
    let cwd = unique_temp_dir("retry-output-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    use crate::protocol::{Event, RetryEvent, StopEvent, TextEvent};
    let backend = crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Retry(RetryEvent {})),
            Ok(Event::Text(TextEvent {
                content: "after retry".into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]],
    );
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, backend))
        .await
        .unwrap();

    let outcome = runtime.run_turn("retry then output").await.unwrap();

    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert!(outcome.text.contains("after retry"), "{}", outcome.text);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn latched_fault_fails_next_turn_without_model_call() {
    let home = unique_temp_dir("latched-turn-home");
    let cwd = unique_temp_dir("latched-turn-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(PanicIfCalledBackend),
        |_| {},
    ))
    .await
    .unwrap();
    let _ = runtime
        .ctx
        .stores
        .persistence_fault
        .raise(std::path::Path::new("todos.json"), "injected fault");

    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        runtime.run_turn("plain text must not succeed"),
    )
    .await
    .expect("a latched turn must finish promptly")
    .unwrap();

    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed
    );
    assert_eq!(
        outcome.failure,
        Some(crate::runtime::TurnFailureReason::Persistence)
    );
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("persistence fault"),
        "{:?}",
        outcome.error
    );
    assert!(
        runtime.ctx.stores.store.lines().await.unwrap().is_empty(),
        "history must stay untouched for a latched turn"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn high_frequency_retry_events_do_not_starve_deadline_checks() {
    let home = unique_temp_dir("retry-storm-home");
    let cwd = unique_temp_dir("retry-storm-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(HighFrequencyRetryBackend),
        |cfg| {
            cfg.llm_first_event_timeout_secs = 1;
            // Single attempt: this test pins the per-attempt deadline, not the
            // retry budget (covered by dedicated recovery tests).
            cfg.llm_recovery.request_max_retries = 0;
            cfg.llm_idle_timeout_secs = 30;
        },
    ))
    .await
    .unwrap();

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(
        tokio::time::Duration::from_secs(2),
        runtime.run_turn("retry storm"),
    )
    .await
    .expect("continuous Retry events must not starve the first-event deadline")
    .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Failed
    );
    assert_eq!(
        outcome.failure,
        Some(crate::runtime::TurnFailureReason::RequestRetryExhausted)
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "the deadline must fire on the consumption path, not only when idle: {elapsed:?}"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// Loopback: two 502s then a 200 through the real OpenAI-compatible
/// backend must issue exactly three physical requests; the runtime attempt
/// loop — not the backend — owns retries.
#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn builtin_http_retry_uses_exactly_three_requests() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_server = seen.clone();
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\ndata: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\ndata: [DONE]\n\n";
    let ok_response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        sse_body.len(),
        sse_body
    );
    let responses = vec![
        "HTTP/1.1 502 Bad Gateway\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad gateway"
            .to_string(),
        "HTTP/1.1 502 Bad Gateway\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad gateway"
            .to_string(),
        ok_response,
    ];
    let server = tokio::spawn(async move {
        for response in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(response.as_bytes()).await;
            seen_server.fetch_add(1, AtomicOrdering::SeqCst);
        }
    });

    let home = unique_temp_dir("http-retry-home");
    let cwd = unique_temp_dir("http-retry-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: format!("http://{addr}"),
        max_context_tokens: 1_000_000,
        log_events: true,
        ..Config::default()
    };
    let runtime = build_runtime(AgentRuntimeConfig::from_config(
        cfg,
        home.clone(),
        cwd.clone(),
    ))
    .await
    .unwrap();

    let outcome = runtime.run_turn("retry twice").await.unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Ok,
        "{:?}",
        outcome.error
    );
    assert_eq!(outcome.text, "recovered");
    assert_eq!(
        seen.load(AtomicOrdering::SeqCst),
        3,
        "two retryable failures plus the accepted attempt = three requests"
    );

    server.abort();
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}
#[tokio::test]
async fn guidance_waits_for_accepted_response_and_continues_same_turn() {
    let home = unique_temp_dir("guidance-home");
    let cwd = unique_temp_dir("guidance-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(GuidanceBoundaryBackend {
        entered: entered.clone(),
        release: release.clone(),
        requests: requests.clone(),
    });
    let runtime = build_runtime(runtime_config_with_backend(&home, &cwd, backend, |_| {}))
        .await
        .unwrap();
    let handle = runtime.handle();
    let input = crate::runtime::HumanInput {
        request_id: "task".into(),
        text: "first task".into(),
        target_turn_id: None,
        attachment_ids: vec![],
    };
    let (receipt, stream) = handle.stream_input(input).unwrap();
    let mut stream = stream.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let guide = crate::runtime::HumanInput {
        request_id: "guide".into(),
        text: "keep the public API".into(),
        target_turn_id: Some(receipt.turn_id.clone()),
        attachment_ids: vec![],
    };
    let (received, none) = handle.stream_input(guide.clone()).unwrap();
    assert!(none.is_none());
    assert_eq!(received.status, crate::runtime::InputStatus::Pending);
    assert_eq!(
        handle.stream_input(guide).unwrap().0.revision,
        received.revision
    );
    assert_eq!(
        runtime.ctx.stores.store.lines().await.unwrap().len(),
        1,
        "guidance must not mutate an in-flight request"
    );
    release.notify_one();
    while stream.recv().await.is_some() {}
    let outcome = stream.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::runtime::TurnStatus::Ok);
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(
        requests.lock().unwrap()[1]
            .iter()
            .any(|message| message["content"] == "keep the public API")
    );
    let history = runtime.ctx.stores.store.lines().await.unwrap();
    assert_eq!(
        history
            .iter()
            .filter(
                |message| message.pointer("/_mink/input_id") == Some(&serde_json::json!("guide"))
            )
            .count(),
        1
    );
    assert_eq!(
        handle
            .input_inbox()
            .entries()
            .iter()
            .filter(|entry| entry.status == crate::runtime::InputStatus::Applied)
            .count(),
        2
    );
    assert!(
        handle
            .stream_input(crate::runtime::HumanInput {
                request_id: "late".into(),
                text: "late".into(),
                target_turn_id: Some(receipt.turn_id),
                attachment_ids: vec![]
            })
            .is_err()
    );
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}
#[tokio::test]
async fn interrupted_guidance_remains_unapplied_and_explicitly_resumable() {
    let home = unique_temp_dir("guidance-stop-home");
    let cwd = unique_temp_dir("guidance-stop-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let runtime = build_runtime(runtime_config_with_backend(
        &home,
        &cwd,
        Arc::new(GuidanceBoundaryBackend {
            entered: entered.clone(),
            release,
            requests: requests.clone(),
        }),
        |_| {},
    ))
    .await
    .unwrap();
    let handle = runtime.handle();
    let (receipt, stream) = handle
        .stream_input(crate::runtime::HumanInput {
            request_id: "task".into(),
            text: "task".into(),
            target_turn_id: None,
            attachment_ids: vec![],
        })
        .unwrap();
    let mut stream = stream.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    handle
        .stream_input(crate::runtime::HumanInput {
            request_id: "guide".into(),
            text: "pending guidance".into(),
            target_turn_id: Some(receipt.turn_id),
            attachment_ids: vec![],
        })
        .unwrap();
    stream.cancel();
    while stream.recv().await.is_some() {}
    assert_eq!(
        stream.outcome().await.unwrap().status,
        crate::runtime::TurnStatus::Interrupted
    );
    let guide = handle
        .input_inbox()
        .entries()
        .into_iter()
        .find(|e| e.input_id == "guide")
        .unwrap();
    assert_eq!(guide.status, crate::runtime::InputStatus::Unapplied);
    let (_, stream) = handle.resume_input("guide", guide.revision).unwrap();
    let mut stream = stream.unwrap();
    while stream.recv().await.is_some() {}
    assert_eq!(
        stream.outcome().await.unwrap().status,
        crate::runtime::TurnStatus::Ok
    );
    assert_eq!(requests.lock().unwrap().len(), 2);
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}
#[tokio::test]
async fn guidance_during_tool_execution_preserves_exchange_and_round_limit() {
    use crate::protocol::{Event, StopEvent, TextEvent};
    for max_turns in [1, 3] {
        let home = unique_temp_dir("guidance-tool-home");
        let cwd = unique_temp_dir("guidance-tool-cwd");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let backend = crate::llm::mock::MockLlmBackend::new(
            "mock",
            vec![
                vec![
                    Ok(Event::ToolCall(tool_call_event(
                        "BoundaryTool",
                        "boundary",
                        serde_json::json!({}),
                    ))),
                    Ok(Event::Stop(StopEvent {
                        reason: "tool_use".into(),
                    })),
                ],
                vec![
                    Ok(Event::Text(TextEvent {
                        content: "after guide".into(),
                    })),
                    Ok(Event::Stop(StopEvent {
                        reason: "end_turn".into(),
                    })),
                ],
            ],
        );
        let mut config = runtime_config_with_mock(&home, &cwd, backend);
        config.config.max_turns = max_turns;
        config.custom_tools.push(Arc::new(BoundaryTool {
            entered: entered.clone(),
            release: release.clone(),
        }));
        let runtime = build_runtime(config).await.unwrap();
        let handle = runtime.handle();
        let (task, stream) = handle
            .stream_input(crate::runtime::HumanInput {
                request_id: "task".into(),
                text: "task".into(),
                target_turn_id: None,
                attachment_ids: vec![],
            })
            .unwrap();
        let mut stream = stream.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        handle
            .stream_input(crate::runtime::HumanInput {
                request_id: "guide".into(),
                text: "keep API".into(),
                target_turn_id: Some(task.turn_id),
                attachment_ids: vec![],
            })
            .unwrap();
        assert!(
            !runtime
                .ctx
                .stores
                .store
                .lines()
                .await
                .unwrap()
                .iter()
                .any(|row| row.pointer("/_mink/input_id") == Some(&serde_json::json!("guide")))
        );
        release.notify_one();
        while stream.recv().await.is_some() {}
        let outcome = stream.outcome().await.unwrap();
        let history = runtime.ctx.stores.store.lines().await.unwrap();
        let tool = history
            .iter()
            .position(|row| {
                row["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
            })
            .unwrap();
        let receipt = handle
            .input_inbox()
            .entries()
            .into_iter()
            .find(|entry| entry.input_id == "guide")
            .unwrap();
        if max_turns == 1 {
            assert_ne!(outcome.status, crate::runtime::TurnStatus::Ok);
            assert_eq!(receipt.status, crate::runtime::InputStatus::Unapplied);
        } else {
            assert_eq!(outcome.status, crate::runtime::TurnStatus::Ok);
            assert_eq!(receipt.status, crate::runtime::InputStatus::Applied);
            let guide = history
                .iter()
                .position(|row| row.pointer("/_mink/input_id") == Some(&serde_json::json!("guide")))
                .unwrap();
            assert!(tool < guide);
        }
        runtime.shutdown().await.unwrap();
        let _ = tokio::fs::remove_dir_all(home).await;
        let _ = tokio::fs::remove_dir_all(cwd).await;
    }
}
