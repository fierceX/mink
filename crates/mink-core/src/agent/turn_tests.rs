use super::*;
use crate::llm::client::LlmBackend;
use crate::llm::mock::MockLlmBackend;
use crate::protocol::{Event, StopEvent, TextEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

struct OverflowThenRecoverBackend {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmBackend for OverflowThenRecoverBackend {
    fn name(&self) -> &str {
        "overflow-then-recover"
    }

    async fn stream(
        &self,
        request: crate::llm::client::LlmRequest,
    ) -> Result<crate::llm::client::LlmResponseStream> {
        if matches!(request.purpose, crate::runtime::LlmPurpose::Compaction) {
            return Ok(crate::llm::client::LlmResponseStream {
                events: Box::pin(futures::stream::iter(vec![
                    Ok(Event::Text(crate::protocol::TextEvent {
                        content: "Task focus: recover\nLatest request: continue\nProgress: compacted\nTool evidence: none\nReflections: none".into(),
                    })),
                    Ok(Event::Stop(crate::protocol::StopEvent {
                        reason: "end_turn".into(),
                    })),
                ])),
                attempt_count: 1,
            });
        }
        if self.calls.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
            anyhow::bail!("HTTP 400: maximum context length exceeded");
        }
        Ok(crate::llm::client::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(Event::Text(crate::protocol::TextEvent {
                    content: "recovered".into(),
                })),
                Ok(Event::Stop(crate::protocol::StopEvent {
                    reason: "stop".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn signal_recovery_decision_noops_when_signal_policy_is_off() {
    let ctx = crate::regression::test_context_for_agent("turn-signal-disabled")
        .await
        .unwrap();
    let mut executor = TurnExecutor::new(ctx);
    let mut belief = crate::agent::belief::BeliefTracker::new(16);
    belief.observe(&[crate::guard::collector::Signal {
        kind: crate::guard::collector::SignalKind::ToolFailed,
        severity: 1.0,
        source_tool: "Bash".into(),
        exit_code: Some(1),
        matched_pattern: None,
        message: "failed".into(),
    }]);

    let decision = executor
        .decide_signal_recovery(false, Some(&mut belief))
        .await
        .unwrap();
    assert!(decision.is_none());
}

#[tokio::test]
async fn abort_replan_success_clears_signal_recovery_guard() {
    let replan_child = vec![
        Ok(Event::Text(TextEvent {
            content: "Plan: re-read the failing module, add a unit test, then fix the root cause."
                .into(),
        })),
        Ok(Event::Stop(StopEvent {
            reason: "end_turn".into(),
        })),
    ];
    let llm = Arc::new(MockLlmBackend::new("flash", vec![replan_child]));
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "abort-replan-guard-clear",
        |_| {},
        llm.clone(),
    )
    .await
    .unwrap();
    let mut executor = TurnExecutor::new(ctx);
    executor.local.signal_recovery_guard = true;
    executor.local.guard_bypassed = false;
    executor.signal_processor.evidence_mut().hard_failures = 8;
    let mut belief = crate::agent::belief::BeliefTracker::new(16);
    for _ in 0..8 {
        belief.observe(&[crate::guard::collector::Signal {
            kind: crate::guard::collector::SignalKind::ToolFailed,
            severity: 1.0,
            source_tool: "Bash".into(),
            exit_code: Some(1),
            matched_pattern: None,
            message: "failed".into(),
        }]);
    }

    let decision = executor
        .decide_signal_recovery(true, Some(&mut belief))
        .await
        .unwrap();
    assert!(decision.is_none());
    assert!(!executor.local.signal_recovery_guard);
    assert!(!executor.local.guard_bypassed);
}

#[tokio::test]
async fn todo_sync_is_appended_once_when_file_revision_is_ahead() -> anyhow::Result<()> {
    let ctx = crate::regression::test_context_for_agent("turn-todo-sync").await?;
    ctx.store.add_user("existing stable history").await?;
    let stable_prefix = ctx.compaction.active_messages().await?;
    ctx.todo_store.apply_structure(
        0,
        crate::session::todo::TodoChanges {
            add: vec![crate::session::todo::TodoAdd {
                content: "active".into(),
            }],
            ..Default::default()
        },
    )?;
    ctx.todo_store.advance(
        1,
        crate::session::todo::TodoTransitions {
            activate: vec!["T0001".into()],
            ..Default::default()
        },
    )?;
    let executor = TurnExecutor::new(ctx.clone());
    let mut messages = ctx.compaction.active_messages().await?;

    assert!(executor.reconcile_todo_state(&mut messages).await?);
    assert!(!executor.reconcile_todo_state(&mut messages).await?);
    assert!(messages.starts_with(&stable_prefix));
    assert_eq!(crate::session::todo::visible_revision(&messages)?, 2);
    assert_eq!(
        ctx.store
            .lines()
            .await?
            .iter()
            .filter(|message| { message["_mink"]["todo_state_kind"].as_str() == Some("sync") })
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn todo_final_guard_reminds_once_but_does_not_force_a_loop() -> anyhow::Result<()> {
    let ctx = crate::regression::test_context_for_agent("turn-todo-final-guard").await?;
    ctx.todo_store.apply_structure(
        0,
        crate::session::todo::TodoChanges {
            add: vec![crate::session::todo::TodoAdd {
                content: "active".into(),
            }],
            ..Default::default()
        },
    )?;
    ctx.todo_store.advance(
        1,
        crate::session::todo::TodoTransitions {
            activate: vec!["T0001".into()],
            ..Default::default()
        },
    )?;
    let mut executor = TurnExecutor::new(ctx.clone());

    assert!(executor.decide_next("stop", None, false).await?.is_none());
    assert_eq!(
        executor.decide_next("stop", None, false).await?,
        Some(TurnDecision::Stop)
    );
    let messages = ctx.store.lines().await?;
    assert_eq!(
        messages
            .iter()
            .filter(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("<todo-final-reminder>"))
            })
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn todo_final_guard_on_last_turn_records_reminder_but_stops() -> anyhow::Result<()> {
    let ctx = crate::regression::test_context_for_agent("turn-todo-final-guard-last-turn").await?;
    ctx.todo_store.apply_structure(
        0,
        crate::session::todo::TodoChanges {
            add: vec![crate::session::todo::TodoAdd {
                content: "active".into(),
            }],
            ..Default::default()
        },
    )?;
    ctx.todo_store.advance(
        1,
        crate::session::todo::TodoTransitions {
            activate: vec!["T0001".into()],
            ..Default::default()
        },
    )?;
    let mut executor = TurnExecutor::new(ctx.clone());

    assert_eq!(
        executor.decide_next("stop", None, true).await?,
        Some(TurnDecision::Stop)
    );
    let messages = ctx.store.lines().await?;
    assert_eq!(
        messages
            .iter()
            .filter(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("<todo-final-reminder>"))
            })
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn todo_progress_guard_appends_at_most_one_reminder_per_turn() -> anyhow::Result<()> {
    let ctx = crate::regression::test_context_for_agent("turn-todo-progress-guard").await?;
    ctx.todo_store.apply_structure(
        0,
        crate::session::todo::TodoChanges {
            add: vec![crate::session::todo::TodoAdd {
                content: "active".into(),
            }],
            ..Default::default()
        },
    )?;
    ctx.todo_store.advance(
        1,
        crate::session::todo::TodoTransitions {
            activate: vec!["T0001".into()],
            ..Default::default()
        },
    )?;
    let mut executor = TurnExecutor::new(ctx.clone());
    executor.local.successful_work_calls_since_todo_advance = 8;

    executor.maybe_append_todo_progress_reminder().await?;
    executor.maybe_append_todo_progress_reminder().await?;
    let messages = ctx.store.lines().await?;
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("<todo-progress-reminder>")
    );
    Ok(())
}

#[tokio::test]
async fn context_overflow_compacts_and_retries_only_once() -> anyhow::Result<()> {
    let llm = Arc::new(OverflowThenRecoverBackend {
        calls: AtomicUsize::new(0),
    });
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-overflow-recovery",
        |config| {
            config.max_context_tokens = 64_000;
            config.context_reserve_tokens = 12_000;
            config.context_compact_tail_tokens = 1_000;
            config.context_compact_max_output_tokens = 2_048;
        },
        llm.clone(),
    )
    .await?;
    for index in 0..3 {
        ctx.store
            .add_user(&format!("old request {index}: {}", "x".repeat(1_000)))
            .await?;
        ctx.store
            .add_assistant(
                &format!("old response {index}: {}", "y".repeat(1_000)),
                "",
                &[],
            )
            .await?;
    }
    let mut executor = TurnExecutor::new(ctx.clone());

    let (decision, _) = executor.execute("continue", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(llm.calls.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(ctx.store.lines().await?.len(), 8);
    assert!(ctx.compaction.current_summary()?.is_some());
    Ok(())
}

#[test]
fn context_overflow_classifier_is_specific() {
    assert!(is_context_overflow_message(
        "HTTP 400: context_length_exceeded"
    ));
    assert!(is_context_overflow_message(
        "This model's maximum context length is 65536 tokens"
    ));
    assert!(!is_context_overflow_message(
        "HTTP 400: invalid tool schema"
    ));
    assert!(!is_context_overflow_message("request timed out"));
}

#[tokio::test]
async fn context_overflow_after_visible_output_is_not_recoverable() -> anyhow::Result<()> {
    let backend = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Text(crate::protocol::TextEvent {
                content: "partial".into(),
            })),
            Ok(Event::Retry(crate::protocol::RetryEvent {})),
            Ok(Event::Error(crate::protocol::ErrorEvent {
                message: "maximum context length exceeded".into(),
                provider_code: None,
                status: None,
            })),
        ]],
    ));
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-partial-overflow",
        |_| {},
        backend,
    )
    .await?;
    let mut executor = TurnExecutor::new(ctx);

    let window = super::format_recovery::FormatRecoveryWindow::new(10, 3);
    let error = match executor.stream_llm_response(&window, &[], "", &[], 0).await {
        Ok(_) => panic!("overflow after partial output should fail"),
        Err(error) => error,
    };

    assert!(error.downcast_ref::<ContextOverflowError>().is_none());
    Ok(())
}

// ── Bounded request recovery ──

use crate::llm::recovery::{LlmUpstreamError, UpstreamFailureKind};
use std::sync::Mutex;

/// One scripted attempt result, consumed in call order.
enum ScriptedAttempt {
    /// Typed upstream failure (retryable or permanent).
    Fail(anyhow::Error),
    /// A response stream that becomes available only after `delay` (used to
    /// prove the optional total request deadline bounds an in-flight request).
    Delayed(std::time::Duration, Vec<Result<Event>>),
    /// A complete response stream; the backend opens it immediately.
    Events(Vec<Result<Event>>),
    /// A stream that never yields (first-event/idle timeout paths).
    Pending,
}

struct ScriptedBackend {
    attempts: Mutex<std::vec::IntoIter<ScriptedAttempt>>,
    calls: AtomicUsize,
    /// Request messages of every physical call, in order (bug triage: the
    /// feedback diagnostics must actually reach the next request).
    requests: Mutex<Vec<Vec<serde_json::Value>>>,
    /// `attempt_count` reported on successful streams (custom backends may
    /// aggregate several internal requests).
    success_attempt_count: u32,
}

impl ScriptedBackend {
    fn new(attempts: Vec<ScriptedAttempt>) -> Arc<Self> {
        Arc::new(Self {
            attempts: Mutex::new(attempts.into_iter()),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            success_attempt_count: 1,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(AtomicOrdering::SeqCst)
    }

    fn requests(&self) -> Vec<Vec<serde_json::Value>> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LlmBackend for ScriptedBackend {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn stream(
        &self,
        request: crate::llm::client::LlmRequest,
    ) -> Result<crate::llm::client::LlmResponseStream> {
        self.calls.fetch_add(1, AtomicOrdering::SeqCst);
        self.requests.lock().unwrap().push(request.messages.clone());
        let next = self.attempts.lock().unwrap().next();
        match next {
            Some(ScriptedAttempt::Fail(error)) => Err(error),
            Some(ScriptedAttempt::Delayed(delay, events)) => {
                tokio::time::sleep(delay).await;
                Ok(crate::llm::client::LlmResponseStream {
                    events: Box::pin(futures::stream::iter(events)),
                    attempt_count: self.success_attempt_count,
                })
            }
            Some(ScriptedAttempt::Events(events)) => Ok(crate::llm::client::LlmResponseStream {
                events: Box::pin(futures::stream::iter(events)),
                attempt_count: self.success_attempt_count,
            }),
            Some(ScriptedAttempt::Pending) => Ok(crate::llm::client::LlmResponseStream {
                events: Box::pin(futures::stream::pending()),
                attempt_count: 1,
            }),
            // Script exhausted: an empty stream, which the runtime treats as
            // abnormal termination.
            None => Ok(crate::llm::client::LlmResponseStream {
                events: Box::pin(futures::stream::iter(Vec::new())),
                attempt_count: 1,
            }),
        }
    }
}

fn recoverable(status: u16) -> anyhow::Error {
    anyhow::Error::new(
        LlmUpstreamError::recoverable(format!("HTTP {status}: transient failure"))
            .with_status(status),
    )
}

fn permanent(status: u16) -> anyhow::Error {
    anyhow::Error::new(
        LlmUpstreamError::permanent(format!("HTTP {status}: permanent failure"))
            .with_status(status),
    )
}

fn protocol_damage() -> anyhow::Error {
    anyhow::Error::new(LlmUpstreamError::protocol_damaged("sse frame corrupt"))
}

fn complete(text: &str) -> ScriptedAttempt {
    ScriptedAttempt::Events(vec![
        Ok(Event::Text(crate::protocol::TextEvent {
            content: text.into(),
        })),
        Ok(Event::Stop(StopEvent {
            reason: "stop".into(),
        })),
    ])
}

async fn scripted_context(
    name: &str,
    backend: Arc<ScriptedBackend>,
    configure: impl FnOnce(&mut crate::config::ResolvedConfig),
) -> anyhow::Result<Arc<crate::context::AgentSharedContext>> {
    crate::regression::test_context_for_agent_with_config_and_backend(name, configure, backend)
        .await
}

fn write_call(id: &str, path: &str, content: &str) -> ToolCallEvent {
    crate::sse::toolcall::build_tool_call_event(
        "Write",
        id,
        &serde_json::json!({"path": path, "content": content}).to_string(),
    )
    .expect("valid Write call")
}

/// Two 502s then a success — exactly three physical attempts and two
/// runtime-managed Retry notifications.
#[tokio::test]
async fn retryable_failures_then_success_use_exactly_three_attempts() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Fail(recoverable(502)),
        ScriptedAttempt::Fail(recoverable(502)),
        complete("ok"),
    ]);
    let ctx = scripted_context("turn-retry-502", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 3;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("retry twice", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.calls(), 3);
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert_eq!(
        events.matches("\"type\":\"retry\"").count(),
        2,
        "one Retry notification per runtime-managed retry: {events}"
    );
    let lines = ctx.store.lines().await?;
    assert_eq!(lines.len(), 2, "one user + one accepted assistant round");
    Ok(())
}

/// Cancellation during backoff: setting the turn interrupt while the
/// runtime waits before a retry ends the request without waiting out the
/// full backoff and without a second physical request.
#[tokio::test]
async fn retry_backoff_is_cancellable() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Fail(recoverable(502)),
        ScriptedAttempt::Fail(recoverable(502)),
        complete("never reached"),
    ]);
    let ctx = scripted_context("turn-retry-cancel", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 3;
    })
    .await?;
    let interrupt = ctx.interrupt.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        interrupt.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let started = std::time::Instant::now();
    let result = TurnExecutor::new(ctx)
        .execute("cancel in backoff", None)
        .await;
    let elapsed = started.elapsed();
    canceller.await?;

    assert!(
        elapsed < std::time::Duration::from_millis(900),
        "the 1s backoff must be cut short by the interrupt: {elapsed:?}"
    );
    assert_eq!(backend.calls(), 1, "no second request after the cancel");
    let message = match result {
        Ok((TurnDecision::Interrupted, _)) => "interrupted".to_string(),
        Ok((decision, _)) => panic!("expected an interrupted turn, got {decision:?}"),
        Err(error) => format!("{error}"),
    };
    assert!(message.to_lowercase().contains("interrupt"), "{message}");
    Ok(())
}

/// Each configured retry budget yields exactly `retries + 1` requests.
#[tokio::test]
async fn retry_budget_yields_retries_plus_one_requests() -> anyhow::Result<()> {
    for (retries, expected) in [(0u32, 1usize), (1, 2)] {
        let backend = ScriptedBackend::new(vec![
            ScriptedAttempt::Fail(recoverable(502)),
            ScriptedAttempt::Fail(recoverable(502)),
            ScriptedAttempt::Fail(recoverable(502)),
            ScriptedAttempt::Fail(recoverable(502)),
            ScriptedAttempt::Fail(recoverable(502)),
        ]);
        let ctx = scripted_context("turn-retry-exhausted", backend.clone(), |cfg| {
            cfg.llm_recovery.request_max_retries = retries;
        })
        .await?;
        let mut executor = TurnExecutor::new(ctx);
        let error = executor
            .execute("exhaust the budget", None)
            .await
            .expect_err("persistent 502 must fail the turn")
            .to_string();
        assert!(
            error.contains("request_retry_exhausted"),
            "retries={retries}: {error}"
        );
        assert_eq!(backend.calls(), expected, "retries={retries}");
    }
    Ok(())
}

/// Permanent failures are never retried (401/403 remain one request).
#[tokio::test]
async fn permanent_failure_is_not_retried() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![ScriptedAttempt::Fail(permanent(401))]);
    let ctx = scripted_context("turn-retry-401", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 3;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx);
    let error = executor
        .execute("unauthorized", None)
        .await
        .expect_err("401 must fail the turn")
        .to_string();
    assert!(error.contains("HTTP 401"), "{error}");
    assert_eq!(backend.calls(), 1);
    Ok(())
}

/// A first-event timeout is a recoverable attempt failure; the retry
/// reuses the same projection and may complete the turn.
#[tokio::test]
async fn first_event_timeout_attempt_is_recoverable() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Pending,
        complete("recovered after timeout"),
    ]);
    let ctx = scripted_context("turn-retry-timeout", backend.clone(), |cfg| {
        cfg.llm_first_event_timeout_secs = 1;
        cfg.llm_wait_heartbeat_secs = 0;
        cfg.llm_recovery.request_max_retries = 1;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx);
    let (decision, _) = executor.execute("timeout then recover", None).await?;
    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.calls(), 2);
    Ok(())
}

/// A damaged attempt is discarded as a whole — no candidate call or
/// text survives — and the retry produces the accepted candidate.
#[tokio::test]
async fn damaged_attempt_discards_candidate_without_side_effects() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::Text(crate::protocol::TextEvent {
                content: "partial".into(),
            })),
            Ok(Event::ToolCall(write_call("leak", "leak.txt", "leaked"))),
            Err(protocol_damage()),
        ]),
        complete("clean"),
    ]);
    let ctx = scripted_context("turn-retry-damaged", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 3;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("damaged stream", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.calls(), 2);
    assert!(
        !ctx.cwd.join("leak.txt").exists(),
        "a damaged attempt must never execute candidate tools"
    );
    let lines = ctx.store.lines().await?;
    let assistant = serde_json::to_string(&lines[1])?;
    assert!(assistant.contains("clean"), "{assistant}");
    assert!(!assistant.contains("partial"), "{assistant}");
    assert!(
        !assistant.contains("tool_result"),
        "no orphan tool result: {assistant}"
    );
    Ok(())
}

/// A protocol-damaged attempt whose format window is already exhausted ends
/// the turn with `format_recovery_exhausted` instead of retrying forever.
#[tokio::test]
async fn damaged_attempt_with_exhausted_window_ends_turn() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![Err(protocol_damage())]),
        complete("should never be requested"),
    ]);
    let ctx = scripted_context("turn-damaged-window", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 3;
        cfg.llm_recovery.format_window_size = 1;
        cfg.llm_recovery.format_max_errors = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx);
    let (decision, _) = executor.execute("exhaust format budget", None).await?;
    match decision {
        TurnDecision::Failed(reason) => {
            assert!(reason.contains("format_recovery_exhausted"), "{reason}");
        }
        other => panic!("expected Failed(format_recovery_exhausted), got {other:?}"),
    }
    assert_eq!(
        backend.calls(),
        1,
        "the repeat pre-check must stop retrying"
    );
    Ok(())
}

/// A legal tool side effect from an earlier round is never replayed when
/// the next round's request is retried.
#[tokio::test]
async fn successful_tool_round_is_not_replayed_by_next_round_retry() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(write_call("write_once", "once.txt", "x"))),
            Ok(Event::Stop(StopEvent {
                reason: "tool_use".into(),
            })),
        ]),
        ScriptedAttempt::Fail(recoverable(502)),
        complete("finished"),
    ]);
    let ctx = scripted_context("turn-no-tool-replay", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 3;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("write then retry", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.calls(), 3);
    assert_eq!(
        tokio::fs::read_to_string(ctx.cwd.join("once.txt")).await?,
        "x"
    );
    let results = crate::regression::tool_result_ids(&ctx.store).await?;
    assert_eq!(results, vec!["write_once".to_string()]);
    Ok(())
}

/// Every physical attempt settles exactly one usage record; known usage
/// is reported, failed attempts stay unreported.
#[tokio::test]
async fn usage_settles_once_per_attempt() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Fail(recoverable(502)),
        ScriptedAttempt::Events(vec![
            Ok(Event::Usage(crate::protocol::UsageEvent {
                input_tokens: 11,
                output_tokens: 7,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            })),
            Ok(Event::Text(crate::protocol::TextEvent {
                content: "ok".into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "stop".into(),
            })),
        ]),
    ]);
    let ctx = scripted_context("turn-usage-attempts", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 3;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("usage per attempt", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    let records = ctx.usage.all_records()?;
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(
        records[0].status,
        crate::session::usage::UsageStatus::Unreported
    );
    assert_eq!(
        records[1].status,
        crate::session::usage::UsageStatus::Reported
    );
    assert_eq!(records[1].tokens.as_ref().unwrap().input_tokens, 11);
    Ok(())
}

/// A custom backend that aggregates internal requests keeps its own
/// `attempt_count`; the outer layer never multiplies it.
#[tokio::test]
async fn custom_backend_aggregated_attempt_count_is_preserved() -> anyhow::Result<()> {
    let backend = Arc::new(ScriptedBackend {
        attempts: Mutex::new(
            vec![
                ScriptedAttempt::Fail(recoverable(502)),
                ScriptedAttempt::Events(vec![
                    Ok(Event::Usage(crate::protocol::UsageEvent {
                        input_tokens: 5,
                        output_tokens: 2,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                    })),
                    Ok(Event::Text(crate::protocol::TextEvent {
                        content: "aggregated".into(),
                    })),
                    Ok(Event::Stop(StopEvent {
                        reason: "stop".into(),
                    })),
                ]),
            ]
            .into_iter(),
        ),
        calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
        success_attempt_count: 3,
    });
    let ctx = scripted_context("turn-aggregated-attempts", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 2;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (_decision, _) = executor.execute("aggregate", None).await?;

    assert_eq!(backend.calls(), 2, "outer calls stay bounded");
    let records = ctx.usage.all_records()?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].attempt_count, 1);
    assert_eq!(
        records[1].attempt_count, 3,
        "internal aggregation preserved"
    );
    Ok(())
}

/// Empty completion: a stop with no text and no calls is not a silent
/// success; it feeds back a bounded diagnostic and consumes the format budget.
#[tokio::test]
async fn empty_completion_is_format_feedback_not_silent_success() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![Ok(Event::Stop(StopEvent {
            reason: "end_turn".into(),
        }))]),
        complete("explicit answer"),
    ]);
    let ctx = scripted_context("turn-empty-stop", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 3;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("answer explicitly", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(
        backend.calls(),
        2,
        "the empty round is answered with feedback"
    );
    let lines = ctx.store.lines().await?;
    assert!(
        lines.iter().any(|line| {
            line["role"] == "user"
                && line["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("<incomplete-response>"))
        }),
        "{lines:?}"
    );
    Ok(())
}

/// The `kind()` accessor on the typed error is part of the public contract for
/// custom backends that request retries explicitly.
#[test]
fn typed_failures_expose_their_recovery_class() {
    assert_eq!(
        recoverable(503)
            .downcast_ref::<LlmUpstreamError>()
            .map(LlmUpstreamError::kind),
        Some(UpstreamFailureKind::Recoverable)
    );
    assert_eq!(
        protocol_damage()
            .downcast_ref::<LlmUpstreamError>()
            .map(LlmUpstreamError::kind),
        Some(UpstreamFailureKind::ProtocolDamaged)
    );
    assert_eq!(
        permanent(403)
            .downcast_ref::<LlmUpstreamError>()
            .map(LlmUpstreamError::kind),
        Some(UpstreamFailureKind::Permanent)
    );
}

/// Regression: the feedback diagnostics of the
/// Truncated / IdentityInvalid / Unconfirmed branches must be part of the very
/// next request, not only of the stored history.
#[tokio::test]
async fn feedback_diagnostics_reach_the_next_request() -> anyhow::Result<()> {
    // Case 1: length-truncated candidate -> <output-truncated>.
    let truncated = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::Text(crate::protocol::TextEvent {
                content: "working".into(),
            })),
            Ok(Event::ToolCall(bash_call_json(
                "trunc_call",
                r#"{"command":"echo hi"}"#,
            ))),
            Ok(Event::Stop(StopEvent {
                reason: "length".into(),
            })),
        ]),
        complete("recovered"),
    ]);
    let ctx = scripted_context("turn-feedback-truncated", truncated.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx);
    let (decision, _) = executor.execute("truncate", None).await?;
    assert_eq!(decision, TurnDecision::Stop);
    let requests = truncated.requests();
    assert_eq!(
        requests.len(),
        2,
        "one feedback round plus the accepted one"
    );
    let second = serde_json::to_string(&requests[1])?;
    assert!(second.contains("<output-truncated>"), "{second}");

    // Case 2: identity-invalid batch -> <tool-call-format-error>.
    let nameless = crate::sse::toolcall::build_tool_call_event("", "call_nameless", "{}")?;
    let invalid = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(nameless)),
            Ok(Event::Stop(StopEvent {
                reason: "tool_use".into(),
            })),
        ]),
        complete("recovered"),
    ]);
    let ctx = scripted_context("turn-feedback-identity", invalid.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx);
    let (decision, _) = executor.execute("bad identity", None).await?;
    assert_eq!(decision, TurnDecision::Stop);
    let second = serde_json::to_string(&invalid.requests()[1])?;
    assert!(second.contains("<tool-call-format-error>"), "{second}");

    // Case 3: unconfirmable response -> <incomplete-response>.
    let unconfirmed = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![Ok(Event::Stop(StopEvent {
            reason: "weird_reason".into(),
        }))]),
        complete("recovered"),
    ]);
    let ctx = scripted_context("turn-feedback-unconfirmed", unconfirmed.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx);
    let (decision, _) = executor.execute("unknown stop", None).await?;
    assert_eq!(decision, TurnDecision::Stop);
    let second = serde_json::to_string(&unconfirmed.requests()[1])?;
    assert!(second.contains("<incomplete-response>"), "{second}");
    Ok(())
}

/// Regression: the optional total request deadline bounds a
/// request that is already in flight, not only the gaps between attempts.
#[tokio::test]
async fn request_deadline_bounds_an_in_flight_request() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![ScriptedAttempt::Delayed(
        std::time::Duration::from_millis(1500),
        vec![
            Ok(Event::Text(crate::protocol::TextEvent {
                content: "too late".into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "stop".into(),
            })),
        ],
    )]);
    let ctx = scripted_context("turn-deadline-in-flight", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 3;
        cfg.llm_recovery.request_timeout_secs = Some(1);
    })
    .await?;
    let started = std::time::Instant::now();
    let error = TurnExecutor::new(ctx)
        .execute("slow provider", None)
        .await
        .expect_err("the total deadline must fail the request")
        .to_string();
    let elapsed = started.elapsed();

    assert!(error.contains("request_timeout"), "{error}");
    assert!(
        elapsed < std::time::Duration::from_millis(1400),
        "the 1s deadline must cut the in-flight request short: {elapsed:?}"
    );
    assert_eq!(backend.calls(), 1, "no retry after the deadline");
    Ok(())
}

/// Regression: a Write with unusable arguments is a model
/// format error and consumes the format budget like any other tool.
#[tokio::test]
async fn write_argument_error_consumes_the_format_budget() -> anyhow::Result<()> {
    let bad_write = crate::sse::toolcall::build_tool_call_event(
        "Write",
        "bad_write",
        r#"{"path":"never-written.txt"}"#,
    )?;
    let backend = ScriptedBackend::new(vec![ScriptedAttempt::Events(vec![
        Ok(Event::ToolCall(bad_write)),
        Ok(Event::Stop(StopEvent {
            reason: "tool_use".into(),
        })),
    ])]);
    let ctx = scripted_context("turn-write-format", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
        cfg.llm_recovery.format_window_size = 1;
        cfg.llm_recovery.format_max_errors = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("bad write", None).await?;

    match decision {
        TurnDecision::Failed(reason) => {
            assert!(reason.contains("format_recovery_exhausted"), "{reason}");
        }
        other => panic!("expected format_recovery_exhausted, got {other:?}"),
    }
    assert!(!ctx.cwd.join("never-written.txt").exists());
    assert_eq!(
        crate::regression::tool_result_ids(&ctx.store).await?,
        vec!["bad_write".to_string()]
    );
    Ok(())
}

/// Regression: a recognized tool call in the body whose
/// arguments cannot be parsed must be surfaced (format feedback), not silently
/// dropped while the turn completes.
#[tokio::test]
async fn scavenged_unparsable_call_is_surfaced_not_dropped() -> anyhow::Result<()> {
    // `arguments` is a JSON string rather than an object: scavenge recognizes
    // the call shape, but the payload is unusable.
    let body = r#"Let me run it: {"name":"Bash","arguments":"{\"command\":\"echo hi\"}"}"#;
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::Text(crate::protocol::TextEvent {
                content: body.into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]),
        complete("recovered"),
    ]);
    let ctx = scripted_context("turn-scavenge-bad", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("run it", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.calls(), 2, "the format feedback round happened");
    assert_eq!(executor.format_error_count(), 1);
    assert_eq!(
        crate::regression::tool_result_ids(&ctx.store).await?,
        vec!["scavenged_1_0".to_string()]
    );
    let second_request = serde_json::to_string(&backend.requests()[1])?;
    assert!(
        second_request.contains("invalid tool arguments"),
        "{second_request}"
    );
    Ok(())
}

/// Regression: a todo reminder appended by the
/// completion decision must be part of the next request, not only of the
/// stored history.
#[tokio::test]
async fn todo_final_reminder_reaches_the_next_request() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![complete("first pass"), complete("after reminder")]);
    let ctx = scripted_context("turn-todo-reminder-delivery", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    ctx.todo_store.apply_structure(
        0,
        crate::session::todo::TodoChanges {
            add: vec![crate::session::todo::TodoAdd {
                content: "active".into(),
            }],
            ..Default::default()
        },
    )?;
    ctx.todo_store.advance(
        1,
        crate::session::todo::TodoTransitions {
            activate: vec!["T0001".into()],
            ..Default::default()
        },
    )?;

    let mut executor = TurnExecutor::new(ctx);
    let (decision, _) = executor.execute("finish up", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.calls(), 2, "the reminder forces one more round");
    let second = serde_json::to_string(&backend.requests()[1])?;
    assert!(second.contains("<todo-final-reminder>"), "{second}");
    Ok(())
}

/// Regression: `[trajectory]` evidence injected by
/// the round-end decision must reach the next request.
#[tokio::test]
async fn injected_signal_evidence_reaches_the_next_request() -> anyhow::Result<()> {
    let soft_failure_batch = |n: usize| {
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(bash_call_json(
                &format!("soft{n}"),
                &format!(r#"{{"command":"echo 'Traceback (most recent call last): fake {n}'"}}"#),
            ))),
            Ok(Event::Stop(StopEvent {
                reason: "tool_use".into(),
            })),
        ])
    };
    let backend = ScriptedBackend::new(vec![
        soft_failure_batch(1),
        soft_failure_batch(2),
        complete("done"),
    ]);
    let ctx = scripted_context("turn-evidence-delivery", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut belief = crate::agent::belief::BeliefTracker::new(16);
    let mut executor = TurnExecutor::new(ctx);
    let (decision, _) = executor.execute("soft failures", Some(&mut belief)).await?;

    assert_eq!(decision, TurnDecision::Stop);
    let requests = backend.requests();
    assert!(requests.len() >= 2, "{requests:?}");
    let last = serde_json::to_string(requests.last().expect("at least one request"))?;
    assert!(
        last.contains("[trajectory]"),
        "the injected evidence must be in the final request: {last}"
    );
    Ok(())
}

/// Regression: an OpenAI-shaped body call with an
/// unparsable `function.arguments` string must not be replaced by `{}` and
/// executed; it must consume the format budget.
#[tokio::test]
async fn unparsable_nested_function_arguments_are_format_feedback() -> anyhow::Result<()> {
    let body =
        r#"Calling now: {"type":"function","function":{"name":"TodoRead","arguments":"not-json"}}"#;
    let backend = ScriptedBackend::new(vec![ScriptedAttempt::Events(vec![
        Ok(Event::Text(crate::protocol::TextEvent {
            content: body.into(),
        })),
        Ok(Event::Stop(StopEvent {
            reason: "end_turn".into(),
        })),
    ])]);
    let ctx = scripted_context("turn-nested-args-damage", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
        cfg.llm_recovery.format_window_size = 1;
        cfg.llm_recovery.format_max_errors = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("run it", None).await?;

    match decision {
        TurnDecision::Failed(reason) => {
            assert!(reason.contains("format_recovery_exhausted"), "{reason}");
        }
        other => panic!("the damaged candidate must consume the budget, got {other:?}"),
    }
    assert_eq!(
        crate::regression::tool_result_ids(&ctx.store).await?,
        vec!["scavenged_1_0".to_string()],
        "the candidate is reported as a failed result"
    );
    let serialized = serde_json::to_string(&ctx.store.lines().await?)?;
    assert!(
        serialized.contains("invalid tool arguments"),
        "TodoRead must not execute with fabricated empty arguments: {serialized}"
    );
    Ok(())
}

/// Regression: a `<tool_call>` wrapper whose inner
/// JSON is damaged is an explicit candidate parse failure — the turn must give
/// feedback (one format error) instead of completing silently.
#[tokio::test]
async fn damaged_wrapper_json_is_format_feedback_not_a_silent_stop() -> anyhow::Result<()> {
    let body = r#"Trying: <tool_call>{"name":"Read","arguments":[/tmp/x"}</tool_call>"#;
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::Text(crate::protocol::TextEvent {
                content: body.into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]),
        complete("recovered"),
    ]);
    let ctx = scripted_context("turn-wrapper-damage", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("try it", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.calls(), 2, "one feedback round happened");
    assert!(
        crate::regression::tool_result_ids(&ctx.store)
            .await?
            .is_empty()
    );
    let second = serde_json::to_string(&backend.requests()[1])?;
    assert!(second.contains("<tool-call-format-error>"), "{second}");
    assert!(second.contains("unparsable"), "{second}");
    Ok(())
}

/// Regression: a present-but-wrong-typed nested
/// `function.arguments` must consume the format budget, not execute the tool
/// with fabricated empty arguments.
#[tokio::test]
async fn non_string_nested_arguments_are_format_feedback() -> anyhow::Result<()> {
    let body =
        r#"Calling now: {"type":"function","function":{"name":"TodoRead","arguments":false}}"#;
    let backend = ScriptedBackend::new(vec![ScriptedAttempt::Events(vec![
        Ok(Event::Text(crate::protocol::TextEvent {
            content: body.into(),
        })),
        Ok(Event::Stop(StopEvent {
            reason: "end_turn".into(),
        })),
    ])]);
    let ctx = scripted_context("turn-argument-type-damage", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
        cfg.llm_recovery.format_window_size = 1;
        cfg.llm_recovery.format_max_errors = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("run it", None).await?;

    match decision {
        TurnDecision::Failed(reason) => {
            assert!(reason.contains("format_recovery_exhausted"), "{reason}");
        }
        other => panic!("the type error must consume the budget, got {other:?}"),
    }
    let serialized = serde_json::to_string(&ctx.store.lines().await?)?;
    assert!(
        serialized.contains("must be a JSON string"),
        "TodoRead must not execute with fabricated arguments: {serialized}"
    );
    Ok(())
}

/// Regression: a `<tool_call>` wrapper with a
/// missing brace is still an explicit candidate marker — feedback, not a
/// plain-text success.
#[tokio::test]
async fn missing_brace_wrapper_is_format_feedback() -> anyhow::Result<()> {
    let body = r#"Trying: <tool_call>{"name":"TodoRead","arguments":false</tool_call>"#;
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::Text(crate::protocol::TextEvent {
                content: body.into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]),
        complete("recovered"),
    ]);
    let ctx = scripted_context("turn-missing-brace-wrapper", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("try it", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.calls(), 2, "one feedback round happened");
    assert!(
        crate::regression::tool_result_ids(&ctx.store)
            .await?
            .is_empty()
    );
    let second = serde_json::to_string(&backend.requests()[1])?;
    assert!(second.contains("<tool-call-format-error>"), "{second}");
    Ok(())
}

/// Regression: valid JSON inside an explicit
/// wrapper but without a usable identity must enter the identity-failure
/// feedback path instead of completing silently.
#[tokio::test]
async fn wrapper_without_identity_is_format_feedback() -> anyhow::Result<()> {
    let body = r#"Trying: <tool_call>{"name":"","arguments":{}}</tool_call>"#;
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::Text(crate::protocol::TextEvent {
                content: body.into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]),
        complete("recovered"),
    ]);
    let ctx = scripted_context("turn-wrapper-identity", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("try it", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.calls(), 2, "one feedback round happened");
    assert!(
        crate::regression::tool_result_ids(&ctx.store)
            .await?
            .is_empty()
    );
    let second = serde_json::to_string(&backend.requests()[1])?;
    assert!(second.contains("<tool-call-format-error>"), "{second}");
    Ok(())
}

/// Text once, then Retry pings forever (audit F5): the idle deadline must still
/// fire because Retry is not progress.
struct RetryPingBackend {
    ping_every: std::time::Duration,
}

#[async_trait::async_trait]
impl LlmBackend for RetryPingBackend {
    fn name(&self) -> &str {
        "retry-ping"
    }

    async fn stream(
        &self,
        _request: crate::llm::client::LlmRequest,
    ) -> Result<crate::llm::client::LlmResponseStream> {
        let ping_every = self.ping_every;
        let events = futures::stream::unfold(0usize, move |index| async move {
            if index == 0 {
                return Some((
                    Ok(Event::Text(crate::protocol::TextEvent {
                        content: "partial".into(),
                    })),
                    1,
                ));
            }
            tokio::time::sleep(ping_every).await;
            Some((Ok(Event::Retry(crate::protocol::RetryEvent {})), index + 1))
        });
        Ok(crate::llm::client::LlmResponseStream {
            events: Box::pin(events),
            attempt_count: 1,
        })
    }
}

/// Regression: a backend that only pings Retry after its first event must not be
/// able to extend the configured idle timeout forever.
#[tokio::test]
async fn retry_notifications_do_not_extend_idle() -> anyhow::Result<()> {
    let backend: Arc<dyn LlmBackend> = Arc::new(RetryPingBackend {
        ping_every: std::time::Duration::from_millis(50),
    });
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-retry-idle",
        |cfg| {
            cfg.llm_recovery.request_max_retries = 0;
            cfg.llm_recovery.request_timeout_secs = None;
            cfg.llm_idle_timeout_secs = 1;
        },
        backend,
    )
    .await?;
    let started = std::time::Instant::now();
    let error = TurnExecutor::new(ctx)
        .execute("ping", None)
        .await
        .expect_err("the idle deadline must end the turn")
        .to_string();
    let elapsed = started.elapsed();

    assert!(error.contains("idle"), "{error}");
    assert!(
        (std::time::Duration::from_millis(900)..std::time::Duration::from_millis(1800))
            .contains(&elapsed),
        "the 1s idle deadline must fire despite the Retry pings: {elapsed:?}"
    );
    Ok(())
}

/// End to end: a DSML candidate whose declared-JSON parameter is
/// invalid must consume the format budget and never execute the tool.
#[tokio::test]
async fn dsml_invalid_json_parameter_is_format_feedback() -> anyhow::Result<()> {
    let body = "Trying <|DSML|invoke name=\"Bash\"><|DSML|parameter name=\"command\" string=\"false\">echo audit<|DSML|parameter></|DSML|invoke>";
    let backend = ScriptedBackend::new(vec![ScriptedAttempt::Events(vec![
        Ok(Event::Text(crate::protocol::TextEvent {
            content: body.into(),
        })),
        Ok(Event::Stop(StopEvent {
            reason: "end_turn".into(),
        })),
    ])]);
    let ctx = scripted_context("turn-dsml-invalid-json", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
        cfg.llm_recovery.format_window_size = 1;
        cfg.llm_recovery.format_max_errors = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("run it", None).await?;

    match decision {
        TurnDecision::Failed(reason) => {
            assert!(reason.contains("format_recovery_exhausted"), "{reason}");
        }
        other => panic!("the invalid JSON parameter must consume the budget, got {other:?}"),
    }
    let serialized = serde_json::to_string(&ctx.store.lines().await?)?;
    assert!(
        serialized.contains("parse DSML parameter"),
        "no executable call may be produced: {serialized}"
    );
    Ok(())
}

#[tokio::test]
async fn dsml_damaged_parameter_header_feeds_back_and_continues() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::Text(TextEvent {
                content: r#"<|DSML|invoke name="Bash"><|DSML|parameter name="command" string="true"</|DSML|invoke>"#.into(),
            })),
            Ok(Event::Stop(StopEvent { reason: "stop".into() })),
        ]),
        complete("done"),
    ]);
    let ctx = scripted_context("turn-dsml-header-feedback", backend.clone(), |cfg| {
        cfg.max_context_tokens = 0;
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx);
    let (decision, _) = executor.execute("continue", None).await?;
    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.calls(), 2);
    assert_eq!(executor.format_error_count(), 1);
    let second = serde_json::to_string(&backend.requests()[1])?;
    assert!(second.contains("DSML parameter"), "{second}");
    assert!(
        second.contains("header is invalid or not terminated"),
        "{second}"
    );
    Ok(())
}

// ── Format feedback and round closure ──

fn bash_call_json(id: &str, arguments: &str) -> ToolCallEvent {
    crate::sse::toolcall::build_tool_call_event("Bash", id, arguments)
        .expect("valid JSON arguments")
}

/// A bad-argument call feeds back a ModelFormat failure; the next round
/// corrects it and the corrected call executes exactly once.
#[tokio::test]
async fn format_feedback_round_trip_corrects_bad_arguments() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(bash_call_json(
                "bad_args",
                r#"{"command":123}"#,
            ))),
            Ok(Event::Stop(StopEvent {
                reason: "tool_use".into(),
            })),
        ]),
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(bash_call_json(
                "good_args",
                r#"{"command":"touch round_trip_marker"}"#,
            ))),
            Ok(Event::Stop(StopEvent {
                reason: "tool_use".into(),
            })),
        ]),
        complete("corrected"),
    ]);
    let ctx = scripted_context("turn-format-feedback", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("fix the arguments", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(
        ctx.cwd.join("round_trip_marker").exists(),
        "the corrected call executes exactly once"
    );
    assert_eq!(executor.format_error_count(), 1);
    assert_eq!(
        crate::regression::tool_result_ids(&ctx.store).await?,
        vec!["bad_args".to_string(), "good_args".to_string()]
    );
    let serialized = serde_json::to_string(&ctx.store.lines().await?)?;
    assert!(
        serialized.contains("invalid tool arguments"),
        "the model sees the decode diagnostic"
    );
    Ok(())
}

/// A candidate batch with a missing tool name cannot be paired reliably:
/// no call runs, no orphan result is written, and one bounded diagnostic is
/// fed back.
#[tokio::test]
async fn unpaired_candidate_batch_is_dropped_with_one_diagnostic() -> anyhow::Result<()> {
    let nameless = crate::sse::toolcall::build_tool_call_event("", "call_nameless", "{}")?;
    let valid = bash_call_json("call_valid", r#"{"command":"touch should_not_exist"}"#);
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(nameless)),
            Ok(Event::ToolCall(valid)),
            Ok(Event::Stop(StopEvent {
                reason: "tool_use".into(),
            })),
        ]),
        complete("recovered"),
    ]);
    let ctx = scripted_context("turn-identity-drop", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("drop the batch", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(!ctx.cwd.join("should_not_exist").exists());
    assert!(
        crate::regression::tool_result_ids(&ctx.store)
            .await?
            .is_empty(),
        "no orphan tool result for a dropped batch"
    );
    let serialized = serde_json::to_string(&ctx.store.lines().await?)?;
    assert!(serialized.contains("<tool-call-format-error>"));
    Ok(())
}

/// Duplicate call id: same whole-batch rejection for ambiguous call ids.
#[tokio::test]
async fn duplicate_call_ids_drop_the_whole_candidate_batch() -> anyhow::Result<()> {
    let first = bash_call_json("dup_id", r#"{"command":"touch dup_one"}"#);
    let second = bash_call_json("dup_id", r#"{"command":"touch dup_two"}"#);
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(first)),
            Ok(Event::ToolCall(second)),
            Ok(Event::Stop(StopEvent {
                reason: "tool_use".into(),
            })),
        ]),
        complete("recovered"),
    ]);
    let ctx = scripted_context("turn-duplicate-ids", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("duplicate ids", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(!ctx.cwd.join("dup_one").exists() && !ctx.cwd.join("dup_two").exists());
    assert!(
        crate::regression::tool_result_ids(&ctx.store)
            .await?
            .is_empty()
    );
    Ok(())
}

/// A length-truncated candidate is discarded as a whole, even when one
/// sub-call looks complete.
#[tokio::test]
async fn truncated_output_never_executes_complete_looking_subcalls() -> anyhow::Result<()> {
    let complete_looking =
        bash_call_json("truncated_write", r#"{"command":"touch truncated_marker"}"#);
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(complete_looking)),
            Ok(Event::Stop(StopEvent {
                reason: "length".into(),
            })),
        ]),
        complete("smaller step done"),
    ]);
    let ctx = scripted_context("turn-truncated-batch", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("hit the limit", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(!ctx.cwd.join("truncated_marker").exists());
    assert!(
        crate::regression::tool_result_ids(&ctx.store)
            .await?
            .is_empty()
    );
    let serialized = serde_json::to_string(&ctx.store.lines().await?)?;
    assert!(serialized.contains("<output-truncated>"));
    Ok(())
}

/// A known model-format call does not consume the recovery guard; the
/// guard stays active and still blocks the next unrecognized mutation.
#[tokio::test]
async fn recovery_guard_lets_known_format_calls_through_and_stays_active() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(Vec::new());
    let ctx = scripted_context("turn-guard-format", backend, |_| {}).await?;
    let mut executor = TurnExecutor::new(ctx);
    executor.local.signal_recovery_guard = true;

    let mut known_format = write_call("bad_args", "guard_format.txt", "x");
    known_format.parse_error = Some("parse tool input: invalid JSON".into());
    known_format.raw_arguments_digest = Some("digest-guard".into());
    let unrecognized = write_call("next_write", "guard_next.txt", "y");

    let (allowed, blocked) = executor.apply_signal_recovery_guard(vec![known_format, unrecognized]);
    assert_eq!(allowed.len(), 1, "{allowed:?}");
    assert_eq!(allowed[0].id, "bad_args");
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].tool_use_id, "next_write");
    assert!(
        executor.local.signal_recovery_guard,
        "the guard keeps waiting for a real call"
    );
    Ok(())
}

/// Legacy `function_call` compatibility never consumes the
/// format budget and its call ids remain pairable across rounds.
#[tokio::test]
async fn legacy_function_call_rounds_do_not_consume_the_format_budget() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(bash_call_json(
                "legacy_1",
                r#"{"command":"echo legacy"}"#,
            ))),
            Ok(Event::Stop(StopEvent {
                reason: "function_call".into(),
            })),
        ]),
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(bash_call_json(
                "legacy_2",
                r#"{"command":"echo legacy"}"#,
            ))),
            Ok(Event::Stop(StopEvent {
                reason: "function_call".into(),
            })),
        ]),
        complete("legacy done"),
    ]);
    let ctx = scripted_context("turn-legacy-function-call", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("legacy rounds", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(executor.format_error_count(), 0);
    assert_eq!(
        crate::regression::tool_result_ids(&ctx.store).await?,
        vec!["legacy_1".to_string(), "legacy_2".to_string()]
    );
    Ok(())
}

/// An exhausted format window never rolls back legal side effects from
/// the same batch; every real result is persisted before the turn fails.
#[tokio::test]
async fn exhausted_window_keeps_legal_side_effects_from_the_same_batch() -> anyhow::Result<()> {
    let legal_write = write_call("legal_write", "legal.txt", "x");
    let bad = bash_call_json("bad_args", r#"{"command":123}"#);
    let backend = ScriptedBackend::new(vec![ScriptedAttempt::Events(vec![
        Ok(Event::ToolCall(legal_write)),
        Ok(Event::ToolCall(bad)),
        Ok(Event::Stop(StopEvent {
            reason: "tool_use".into(),
        })),
    ])]);
    let ctx = scripted_context("turn-window-with-side-effects", backend.clone(), |cfg| {
        cfg.llm_recovery.request_max_retries = 0;
        cfg.llm_recovery.format_window_size = 1;
        cfg.llm_recovery.format_max_errors = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("one bad call", None).await?;

    match decision {
        TurnDecision::Failed(reason) => {
            assert!(reason.contains("format_recovery_exhausted"), "{reason}");
        }
        other => panic!("expected format_recovery_exhausted, got {other:?}"),
    }
    assert_eq!(
        tokio::fs::read_to_string(ctx.cwd.join("legal.txt")).await?,
        "x"
    );
    assert_eq!(
        crate::regression::tool_result_ids(&ctx.store).await?,
        vec!["legal_write".to_string(), "bad_args".to_string()]
    );
    Ok(())
}

/// The format window is a plain state object — the signal policy does not
/// change its boundaries or the feedback round trip.
#[tokio::test]
async fn format_window_is_independent_of_the_signal_policy() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(bash_call_json(
                "bad_args",
                r#"{"command":123}"#,
            ))),
            Ok(Event::Stop(StopEvent {
                reason: "tool_use".into(),
            })),
        ]),
        ScriptedAttempt::Events(vec![
            Ok(Event::ToolCall(bash_call_json(
                "good_args",
                r#"{"command":"touch signals_off_marker"}"#,
            ))),
            Ok(Event::Stop(StopEvent {
                reason: "tool_use".into(),
            })),
        ]),
        complete("done with signals off"),
    ]);
    let ctx = scripted_context("turn-format-signals-off", backend.clone(), |cfg| {
        cfg.signal_policy = crate::config::SignalPolicy::Off;
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("signals off", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert!(ctx.cwd.join("signals_off_marker").exists());
    assert_eq!(executor.format_error_count(), 1);
    Ok(())
}

/// Correction until the last allowed round does not overrun `max_turns`;
/// the terminal is the existing MaxTurnsExceeded, not a format failure.
#[tokio::test]
async fn last_round_format_feedback_without_exhaustion_returns_max_turns() -> anyhow::Result<()> {
    let backend = ScriptedBackend::new(vec![ScriptedAttempt::Events(vec![
        Ok(Event::ToolCall(bash_call_json(
            "bad_args",
            r#"{"command":123}"#,
        ))),
        Ok(Event::Stop(StopEvent {
            reason: "tool_use".into(),
        })),
    ])]);
    let ctx = scripted_context("turn-format-max-turns", backend.clone(), |cfg| {
        cfg.max_turns = 1;
        cfg.llm_recovery.request_max_retries = 0;
    })
    .await?;
    let mut executor = TurnExecutor::new(ctx);
    let (decision, _) = executor.execute("single round", None).await?;

    assert_eq!(decision, TurnDecision::MaxTurnsExceeded);
    assert_eq!(backend.calls(), 1);
    Ok(())
}
