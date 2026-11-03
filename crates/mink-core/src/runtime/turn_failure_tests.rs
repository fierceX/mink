use super::*;
use crate::protocol::{Event, StopEvent, TextEvent, ToolCallEvent};
use std::sync::Arc;

fn unique_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "mink-failure-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn options(
    home: &std::path::Path,
    cwd: &std::path::Path,
    mock: crate::llm::mock::MockLlmBackend,
) -> AgentOptions {
    AgentOptions::new(home, cwd)
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1")
        .with_model("flash")
        .with_max_context_tokens(1_000_000)
        .with_llm_backend(Arc::new(mock))
}

fn tool_call(name: &str, id: &str, input: serde_json::Value) -> ToolCallEvent {
    ToolCallEvent {
        name: name.into(),
        id: id.into(),
        input_json: input,
        fields: Default::default(),
        parse_error: None,
        raw_arguments_digest: None,
    }
}

#[tokio::test]
async fn provider_terminal_stop_sets_provider_stopped() {
    let home = unique_dir("stop-home");
    let cwd = unique_dir("stop-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = AgentRuntime::start(options(
        &home,
        &cwd,
        crate::llm::mock::MockLlmBackend::new(
            "flash",
            vec![vec![Ok(Event::Stop(StopEvent {
                reason: "error".into(),
            }))]],
        ),
    ))
    .await
    .unwrap();
    let outcome = runtime.run_turn("stop").await.unwrap();
    assert_eq!(outcome.status, TurnStatus::Failed);
    assert_eq!(
        outcome.failure,
        Some(TurnFailureReason::ProviderStopped),
        "{outcome:?}"
    );
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn exhausted_format_window_sets_format_recovery_exhausted() {
    let home = unique_dir("format-home");
    let cwd = unique_dir("format-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let recovery = LlmRecoveryPolicy {
        format_window_size: 1,
        format_max_errors: 0,
        ..Default::default()
    };
    let runtime = AgentRuntime::start(
        options(
            &home,
            &cwd,
            crate::llm::mock::MockLlmBackend::new(
                "flash",
                vec![vec![Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                }))]],
            ),
        )
        .with_llm_recovery(recovery),
    )
    .await
    .unwrap();
    let outcome = runtime.run_turn("empty response").await.unwrap();
    assert_eq!(outcome.status, TurnStatus::Failed);
    assert_eq!(
        outcome.failure,
        Some(TurnFailureReason::FormatRecoveryExhausted),
        "{outcome:?}"
    );
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn impossible_context_budget_sets_context_unavailable() {
    let home = unique_dir("context-home");
    let cwd = unique_dir("context-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = AgentRuntime::start(
        options(
            &home,
            &cwd,
            crate::llm::mock::MockLlmBackend::new(
                "flash",
                vec![vec![Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                }))]],
            ),
        )
        .with_max_context_tokens(65_000)
        .with_context_reserve_tokens(64_000)
        .with_context_compact_pct(100),
    )
    .await
    .unwrap();
    let outcome = runtime.run_turn("does not fit").await.unwrap();
    assert_eq!(outcome.status, TurnStatus::Failed);
    assert_eq!(
        outcome.failure,
        Some(TurnFailureReason::ContextUnavailable),
        "{outcome:?}"
    );
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn max_turns_sets_max_turns_failure() {
    let home = unique_dir("turns-home");
    let cwd = unique_dir("turns-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    tokio::fs::write(cwd.join("probe.txt"), "x").await.unwrap();
    let runtime = AgentRuntime::start(
        options(
            &home,
            &cwd,
            crate::llm::mock::MockLlmBackend::new(
                "flash",
                vec![vec![
                    Ok(Event::ToolCall(tool_call(
                        "Read",
                        "read-1",
                        serde_json::json!({"path": "probe.txt"}),
                    ))),
                    Ok(Event::Stop(StopEvent {
                        reason: "tool_use".into(),
                    })),
                ]],
            ),
        )
        .with_max_turns(1),
    )
    .await
    .unwrap();
    let outcome = runtime.run_turn("read then stop").await.unwrap();
    assert_eq!(outcome.status, TurnStatus::MaxTurnsExceeded);
    assert_eq!(
        outcome.failure,
        Some(TurnFailureReason::MaxTurnsExceeded),
        "{outcome:?}"
    );
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn text_turn_keeps_failure_empty() {
    let home = unique_dir("ok-home");
    let cwd = unique_dir("ok-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = AgentRuntime::start(options(
        &home,
        &cwd,
        crate::llm::mock::MockLlmBackend::new(
            "flash",
            vec![vec![
                Ok(Event::Text(TextEvent {
                    content: "ok".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ]],
        ),
    ))
    .await
    .unwrap();
    let outcome = runtime.run_turn("hello").await.unwrap();
    assert_eq!(outcome.status, TurnStatus::Ok);
    assert_eq!(outcome.failure, None);
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}
