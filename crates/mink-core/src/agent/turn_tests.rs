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

// ── Repeated compaction inside one user input ──

const COMPACTED_SUMMARY: &str = "Task focus: keep working\nLatest request: continue\nProgress: compacted\nTool evidence: none\nReflections: none";

/// One agent round for the repeated-compaction backend.
enum CompactionStep {
    /// A Write round whose payload dominates the round's token estimate.
    Write(&'static str),
    /// A provider error envelope with no visible output (overflow path).
    ProviderError(&'static str),
    /// A terminal assistant text round.
    Text(&'static str),
}

/// Purpose-aware backend: compaction requests are answered inline (and
/// counted) so a single input can compact repeatedly; agent rounds follow
/// the script in call order.
struct RepeatedCompactionBackend {
    script: Mutex<std::vec::IntoIter<CompactionStep>>,
    compactions: AtomicUsize,
    agent_calls: AtomicUsize,
}

impl RepeatedCompactionBackend {
    fn new(script: Vec<CompactionStep>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into_iter()),
            compactions: AtomicUsize::new(0),
            agent_calls: AtomicUsize::new(0),
        })
    }

    fn compactions(&self) -> usize {
        self.compactions.load(AtomicOrdering::SeqCst)
    }

    fn agent_calls(&self) -> usize {
        self.agent_calls.load(AtomicOrdering::SeqCst)
    }
}

fn stream_of(events: Vec<Result<Event>>) -> crate::llm::client::LlmResponseStream {
    crate::llm::client::LlmResponseStream {
        events: Box::pin(futures::stream::iter(events)),
        attempt_count: 1,
    }
}

#[async_trait::async_trait]
impl LlmBackend for RepeatedCompactionBackend {
    fn name(&self) -> &str {
        "repeated-compaction"
    }

    async fn stream(
        &self,
        request: crate::llm::client::LlmRequest,
    ) -> Result<crate::llm::client::LlmResponseStream> {
        if matches!(request.purpose, crate::runtime::LlmPurpose::Compaction) {
            self.compactions.fetch_add(1, AtomicOrdering::SeqCst);
            return Ok(stream_of(vec![
                Ok(Event::Text(TextEvent {
                    content: COMPACTED_SUMMARY.into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ]));
        }
        self.agent_calls.fetch_add(1, AtomicOrdering::SeqCst);
        let step = self.script.lock().unwrap().next();
        Ok(stream_of(match step {
            Some(CompactionStep::Write(path)) => vec![
                Ok(Event::ToolCall(write_call(
                    &format!("write_{path}"),
                    path,
                    &"x".repeat(100_000),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_calls".into(),
                })),
            ],
            Some(CompactionStep::ProviderError(message)) => {
                vec![Ok(Event::Error(crate::protocol::ErrorEvent {
                    message: message.into(),
                    provider_code: None,
                    status: None,
                }))]
            }
            Some(CompactionStep::Text(text)) => vec![
                Ok(Event::Text(TextEvent {
                    content: text.into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "stop".into(),
                })),
            ],
            None => Vec::new(),
        }))
    }
}

/// Window smaller than a few Write rounds: one input crosses the request
/// budget repeatedly, so the same input must be able to compact again.
async fn repeated_compaction_context(
    name: &str,
    backend: Arc<RepeatedCompactionBackend>,
) -> anyhow::Result<Arc<crate::context::AgentSharedContext>> {
    crate::regression::test_context_for_agent_with_config_and_backend(
        name,
        |cfg| {
            cfg.max_context_tokens = 120_000;
            cfg.context_reserve_tokens = 20_000;
            cfg.context_compact_pct = 100;
            cfg.context_compact_tail_tokens = 1_000;
        },
        backend,
    )
    .await
}

/// Regression: a long single input used to die on the request budget after its
/// one allowed compaction. It must keep compacting and finish the work.
#[tokio::test]
async fn single_input_compacts_repeatedly_instead_of_failing_the_turn() -> anyhow::Result<()> {
    let backend = RepeatedCompactionBackend::new(vec![
        CompactionStep::Write("a.txt"),
        CompactionStep::Write("b.txt"),
        CompactionStep::Write("c.txt"),
        CompactionStep::Write("d.txt"),
        CompactionStep::Write("e.txt"),
        CompactionStep::Write("f.txt"),
        CompactionStep::Write("g.txt"),
        CompactionStep::Text("done"),
    ]);
    let ctx = repeated_compaction_context("turn-repeat-compaction", backend.clone()).await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("write every file", None).await?;

    assert_eq!(
        decision,
        TurnDecision::Stop,
        "a growing input must not fail while more compactions are possible"
    );
    assert_eq!(backend.agent_calls(), 8);
    assert!(
        backend.compactions() >= 2,
        "the same input must be able to compact more than once: {}",
        backend.compactions()
    );
    assert_eq!(
        crate::regression::tool_result_ids(&ctx.store).await?.len(),
        7,
        "every executed round must be preserved"
    );
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert!(
        events.matches("\"type\":\"compact\"").count() >= 2,
        "each compaction must stay visible: {events}"
    );
    Ok(())
}

/// Regression: provider overflow recovery used to be refused whenever the
/// input had already compacted, turning a recoverable overflow into a failed
/// turn.
#[tokio::test]
async fn provider_overflow_recovers_after_compaction_in_the_same_input() -> anyhow::Result<()> {
    let backend = RepeatedCompactionBackend::new(vec![
        CompactionStep::Write("a.txt"),
        CompactionStep::Write("b.txt"),
        CompactionStep::Write("c.txt"),
        CompactionStep::Write("d.txt"),
        CompactionStep::ProviderError("maximum context length exceeded"),
        CompactionStep::Text("recovered"),
    ]);
    let ctx =
        repeated_compaction_context("turn-overflow-after-compaction", backend.clone()).await?;
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("keep going", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.agent_calls(), 6);
    assert!(
        backend.compactions() >= 2,
        "overflow recovery must be able to compact after an earlier compaction: {}",
        backend.compactions()
    );
    Ok(())
}

// ── 摘要不可用 / 候选预算 / 中断：应急 checkpoint ──

/// 摘要请求的可控结果。
enum SummaryResponse {
    /// 每次都返回不可用摘要（空正文 + stop）→ 触发纠错/恢复耗尽。
    Unusable,
    /// 返回一份合法摘要（用于动态输出 cap 等成功路径）。
    Ok,
    /// 摘要成功但正文巨大 → 触发候选发布前预算验收。
    Huge(usize),
    /// 摘要永不返回 → 用于中断优先级。
    Pending,
}

/// 目的感知的 backend：摘要请求按 [`SummaryResponse`] 出牌并计数，主请求按脚本出牌。
struct SummaryOutageBackend {
    summary: SummaryResponse,
    summary_calls: AtomicUsize,
    /// 每个摘要请求申请的输出 cap（断言动态预算）。
    summary_max_tokens: Mutex<Vec<i32>>,
    script: Mutex<std::vec::IntoIter<CompactionStep>>,
    agent_calls: AtomicUsize,
}

impl SummaryOutageBackend {
    fn new(summary: SummaryResponse, script: Vec<CompactionStep>) -> Arc<Self> {
        Arc::new(Self {
            summary,
            summary_calls: AtomicUsize::new(0),
            summary_max_tokens: Mutex::new(Vec::new()),
            script: Mutex::new(script.into_iter()),
            agent_calls: AtomicUsize::new(0),
        })
    }

    fn summary_calls(&self) -> usize {
        self.summary_calls.load(AtomicOrdering::SeqCst)
    }

    fn agent_calls(&self) -> usize {
        self.agent_calls.load(AtomicOrdering::SeqCst)
    }

    fn summary_max_tokens(&self) -> Vec<i32> {
        self.summary_max_tokens.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LlmBackend for SummaryOutageBackend {
    fn name(&self) -> &str {
        "summary-outage"
    }

    async fn stream(
        &self,
        request: crate::llm::client::LlmRequest,
    ) -> Result<crate::llm::client::LlmResponseStream> {
        if matches!(request.purpose, crate::runtime::LlmPurpose::Compaction) {
            self.summary_calls.fetch_add(1, AtomicOrdering::SeqCst);
            self.summary_max_tokens
                .lock()
                .unwrap()
                .push(request.max_tokens);
            return Ok(match self.summary {
                SummaryResponse::Unusable => stream_of(vec![Ok(Event::Stop(StopEvent {
                    reason: "stop".into(),
                }))]),
                SummaryResponse::Ok => stream_of(vec![
                    Ok(Event::Text(TextEvent {
                        content: COMPACTED_SUMMARY.into(),
                    })),
                    Ok(Event::Stop(StopEvent {
                        reason: "stop".into(),
                    })),
                ]),
                SummaryResponse::Huge(chars) => stream_of(vec![
                    Ok(Event::Text(TextEvent {
                        content: format!("OVERSIZED-SUMMARY-MARKER {}", "s".repeat(chars)),
                    })),
                    Ok(Event::Stop(StopEvent {
                        reason: "stop".into(),
                    })),
                ]),
                SummaryResponse::Pending => crate::llm::client::LlmResponseStream {
                    events: Box::pin(futures::stream::pending()),
                    attempt_count: 1,
                },
            });
        }
        self.agent_calls.fetch_add(1, AtomicOrdering::SeqCst);
        let step = self.script.lock().unwrap().next();
        Ok(stream_of(match step {
            Some(CompactionStep::Write(path)) => vec![
                Ok(Event::ToolCall(write_call(
                    &format!("write_{path}"),
                    path,
                    &"x".repeat(100_000),
                ))),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_calls".into(),
                })),
            ],
            Some(CompactionStep::ProviderError(message)) => {
                vec![Ok(Event::Error(crate::protocol::ErrorEvent {
                    message: message.into(),
                    provider_code: None,
                    status: None,
                }))]
            }
            Some(CompactionStep::Text(text)) => vec![
                Ok(Event::Text(TextEvent {
                    content: text.into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "stop".into(),
                })),
            ],
            _ => Vec::new(),
        }))
    }
}

/// 预置历史：`turns` 轮 user/assistant 对，每轮正文 `chars` 字符（估算 ≈ chars/3）。
async fn seed_history(
    ctx: &crate::context::AgentSharedContext,
    turns: usize,
    chars: usize,
) -> anyhow::Result<()> {
    for index in 0..turns {
        ctx.store
            .add_user(&format!("old task {index} {}", "x".repeat(chars)))
            .await?;
        ctx.store
            .add_assistant(&format!("progress {index} {}", "y".repeat(chars)), "", &[])
            .await?;
    }
    Ok(())
}

/// 摘要持续不可用而请求已经装不下 → 确定性应急 checkpoint 接手，turn 不再以压缩
/// 错误结束；当前请求原文仍出现在投影里。
#[tokio::test]
async fn summary_outage_falls_back_to_emergency_checkpoint() -> anyhow::Result<()> {
    let backend = SummaryOutageBackend::new(
        SummaryResponse::Unusable,
        vec![CompactionStep::Text("done")],
    );
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-emergency-fallback",
        |cfg| {
            cfg.max_context_tokens = 200_000;
            cfg.context_reserve_tokens = 8_000;
            cfg.context_compact_pct = 50;
            cfg.context_compact_tail_tokens = 1_000;
            cfg.llm_recovery.request_max_retries = 0;
        },
        backend.clone(),
    )
    .await?;
    // 8 × 24,000 est ≈ 192k：请求已超硬预算，但折叠段仍能装进摘要输入预算，
    // 因此会真的发出物理摘要请求（provider 侧失败）。
    seed_history(&ctx, 4, 72_000).await?;

    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor
        .execute("carry on with the remaining work", None)
        .await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.agent_calls(), 1);
    assert!(
        backend.summary_calls() >= 1,
        "摘要请求确实被尝试过：{}",
        backend.summary_calls()
    );
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert!(events.contains(r#""trigger":"emergency""#), "{events}");
    assert!(events.contains("_mode=emergency"), "{events}");
    let active = ctx.compaction.active_messages().await?;
    let projection = serde_json::to_string(&active)?;
    assert!(
        projection.contains("emergency-context-excerpt"),
        "{projection}"
    );
    assert!(
        projection.contains("carry on with the remaining work"),
        "当前请求原文必须留在投影里：{projection}"
    );
    assert!(
        !projection.contains("old task 0"),
        "被折叠的历史不得再进入投影：{projection}"
    );
    // 完整历史只追加不重写：被折叠的内容仍在权威会话记录里。
    let history = serde_json::to_string(&ctx.store.lines_from(0).await?)?;
    assert!(history.contains("old task 0"), "{history}");
    assert!(
        history.contains("carry on with the remaining work"),
        "{history}"
    );
    Ok(())
}

/// 摘要输入可证明装不下（小窗口）→ 不浪费物理摘要请求，直接转应急。
#[tokio::test]
async fn oversized_summary_input_skips_the_request_and_uses_emergency() -> anyhow::Result<()> {
    let backend = SummaryOutageBackend::new(
        SummaryResponse::Unusable,
        vec![CompactionStep::Text("done")],
    );
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-summary-input-over-budget",
        |cfg| {
            cfg.max_context_tokens = 40_000;
            cfg.context_reserve_tokens = 8_000;
            cfg.context_compact_pct = 50;
            cfg.context_compact_tail_tokens = 1_000;
            cfg.llm_recovery.request_max_retries = 0;
        },
        backend.clone(),
    )
    .await?;
    seed_history(&ctx, 4, 60_000).await?;

    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("keep going", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.agent_calls(), 1);
    assert_eq!(backend.summary_calls(), 0, "装不下的摘要请求不应发出");
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert!(events.contains("_mode=emergency"), "{events}");
    Ok(())
}

/// auto 摘要失败但原请求本来就能发送 → 不提交压缩、不写应急，请求照常发出。
#[tokio::test]
async fn auto_summary_failure_does_not_block_a_sendable_request() -> anyhow::Result<()> {
    let backend = SummaryOutageBackend::new(
        SummaryResponse::Unusable,
        vec![CompactionStep::Text("done")],
    );
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-auto-summary-outage",
        |cfg| {
            cfg.max_context_tokens = 400_000;
            cfg.context_reserve_tokens = 8_000;
            cfg.context_compact_pct = 1; // 低触发点：auto 会尝试，但请求远未超预算
            cfg.context_compact_tail_tokens = 1_000;
            cfg.llm_recovery.request_max_retries = 0;
        },
        backend.clone(),
    )
    .await?;
    ctx.store.add_user("small task").await?;

    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("keep going", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.agent_calls(), 1);
    assert!(ctx.compaction.read_summary().await.is_none());
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert!(!events.contains(r#""trigger":"emergency""#), "{events}");
    assert!(!events.contains(r#""type":"compact""#), "{events}");
    Ok(())
}

/// 摘要成功但完整投影装不下 → 该候选不提交（投影里不出现它的正文），转应急。
#[tokio::test]
async fn oversized_summary_candidate_is_rejected_and_falls_back_to_emergency() -> anyhow::Result<()>
{
    let backend = SummaryOutageBackend::new(
        // ≈200k est 的摘要正文：摘要本身成功，但「摘要 + 保留尾部」装不进硬预算。
        SummaryResponse::Huge(600_000),
        vec![CompactionStep::Text("done")],
    );
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-candidate-over-budget",
        |cfg| {
            cfg.max_context_tokens = 200_000;
            cfg.context_reserve_tokens = 8_000;
            cfg.context_compact_pct = 50;
            cfg.context_compact_tail_tokens = 1_000;
            cfg.llm_recovery.request_max_retries = 0;
        },
        backend.clone(),
    )
    .await?;
    seed_history(&ctx, 4, 72_000).await?;

    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("keep going", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.agent_calls(), 1);
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert!(events.contains("_mode=emergency"), "{events}");
    let projection = serde_json::to_string(&ctx.compaction.active_messages().await?)?;
    assert!(
        !projection.contains("OVERSIZED-SUMMARY-MARKER"),
        "超预算摘要不得进入投影：{projection}"
    );
    Ok(())
}

/// 压缩期间被中断必须报 Interrupted，而不是普通失败。
#[tokio::test]
async fn interrupt_during_compaction_reports_interrupted() -> anyhow::Result<()> {
    let backend = SummaryOutageBackend::new(
        SummaryResponse::Pending,
        vec![CompactionStep::Text("never reached")],
    );
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-compaction-interrupt",
        |cfg| {
            cfg.max_context_tokens = 200_000;
            cfg.context_reserve_tokens = 8_000;
            cfg.context_compact_pct = 50;
            cfg.context_compact_tail_tokens = 1_000;
            cfg.llm_recovery.request_max_retries = 0;
        },
        backend.clone(),
    )
    .await?;
    seed_history(&ctx, 4, 72_000).await?;

    let interrupt = ctx.interrupt.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        interrupt.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("keep going", None).await?;
    canceller.await?;

    assert_eq!(decision, TurnDecision::Interrupted);
    assert_eq!(backend.agent_calls(), 0, "中断不得发出主请求");
    Ok(())
}

/// 摘要输入已经吃掉大部分窗口 → 动态输出 cap 接手：请求照发（cap 小于配置值），
/// 摘要成功提交，而不是按配置上限过早放弃。
#[tokio::test]
async fn dynamic_summary_cap_shrinks_the_request_instead_of_giving_up() -> anyhow::Result<()> {
    let backend =
        SummaryOutageBackend::new(SummaryResponse::Ok, vec![CompactionStep::Text("done")]);
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-dynamic-summary-cap",
        |cfg| {
            cfg.max_context_tokens = 40_000;
            cfg.context_reserve_tokens = 8_000;
            cfg.context_compact_pct = 100;
            cfg.context_compact_tail_tokens = 1_000;
            cfg.context_compact_max_output_tokens = 4_096;
            cfg.llm_recovery.request_max_retries = 0;
        },
        backend.clone(),
    )
    .await?;
    // 折叠段 ≈ 37k：窗口 40k 下按 4,096 的固定上限已装不下（需要 ≤ 35,904 才放行），
    // 但按动态 cap 只需留一个最小实用输出，因此摘要请求应带 ~3k 的输出上限发出。
    ctx.store
        .add_user(&format!("old task {}", "x".repeat(111_000)))
        .await?;
    ctx.store.add_assistant("ack", "", &[]).await?;
    ctx.store.add_user("next").await?;
    ctx.store.add_assistant("ok", "", &[]).await?;

    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("keep going", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.agent_calls(), 1);
    let caps = backend.summary_max_tokens();
    assert!(!caps.is_empty(), "摘要请求必须被发出");
    assert!(
        caps.iter().all(|cap| *cap < 4_096),
        "输出 cap 必须按剩余空间收紧：{caps:?}"
    );
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert!(!events.contains("_mode=emergency"), "{events}");
    Ok(())
}

/// 剩余空间连最小实用输出都放不下 → 不发物理摘要请求，直接转应急。
#[tokio::test]
async fn summary_output_budget_floor_falls_back_to_emergency() -> anyhow::Result<()> {
    let backend =
        SummaryOutageBackend::new(SummaryResponse::Ok, vec![CompactionStep::Text("done")]);
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-summary-cap-floor",
        |cfg| {
            cfg.max_context_tokens = 36_000;
            cfg.context_reserve_tokens = 8_000;
            cfg.context_compact_pct = 100;
            cfg.context_compact_tail_tokens = 1_000;
            cfg.context_compact_max_output_tokens = 4_096;
            cfg.llm_recovery.request_max_retries = 0;
        },
        backend.clone(),
    )
    .await?;
    seed_history(&ctx, 2, 60_000).await?;

    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("keep going", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.agent_calls(), 1);
    assert_eq!(
        backend.summary_calls(),
        0,
        "连最小输出都放不下时不得发出摘要请求"
    );
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert!(events.contains("_mode=emergency"), "{events}");
    Ok(())
}

/// provider 报 overflow（本地估算却装得下）+ 摘要不可用 → 应急按折半目标收缩，
/// 请求严格变小后重试成功。
#[tokio::test]
async fn provider_overflow_shrinks_via_emergency_when_summary_is_unavailable() -> anyhow::Result<()>
{
    let backend = SummaryOutageBackend::new(
        SummaryResponse::Unusable,
        vec![
            CompactionStep::ProviderError("maximum context length exceeded"),
            CompactionStep::Text("recovered"),
        ],
    );
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-overflow-emergency-shrink",
        |cfg| {
            cfg.max_context_tokens = 200_000;
            cfg.context_reserve_tokens = 8_000;
            cfg.context_compact_pct = 90;
            cfg.context_compact_tail_tokens = 1_000;
            cfg.llm_recovery.request_max_retries = 0;
        },
        backend.clone(),
    )
    .await?;
    // 本地估算远低于硬预算（192k），因此只在 provider 侧暴露 overflow。
    seed_history(&ctx, 4, 24_000).await?;

    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("keep going", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    assert_eq!(backend.agent_calls(), 2);
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert!(events.contains("_mode=emergency"), "{events}");
    assert!(
        events.contains("_emergency_reason=provider_overflow"),
        "overflow 收缩的原因必须落盘：{events}"
    );
    Ok(())
}

/// 巨大 plan.md：模型可见的 plan checkpoint 必须有损截短，权威 plan 文件不变，
/// 且投影仍能装进预算（应急接手而不是 fail-closed）。
#[tokio::test]
async fn huge_plan_checkpoint_is_bounded_in_the_projection() -> anyhow::Result<()> {
    let backend = SummaryOutageBackend::new(
        SummaryResponse::Unusable,
        vec![CompactionStep::Text("done")],
    );
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-bounded-plan",
        |cfg| {
            cfg.max_context_tokens = 40_000;
            cfg.context_reserve_tokens = 8_000;
            cfg.context_compact_pct = 50;
            cfg.context_compact_tail_tokens = 1_000;
            cfg.llm_recovery.request_max_retries = 0;
        },
        backend.clone(),
    )
    .await?;
    // 权威 plan 正文：远超窗口（≈200k 字符）。路径与引擎读取的权威位置一致。
    let plan_path = ctx.summary_path.with_file_name("plan.md");
    let plan_body = format!(
        "# plan\n{}\nMIDDLE-MARKER\n{}\nTAIL-MARKER\n",
        "p".repeat(100_000),
        "p".repeat(100_000)
    );
    tokio::fs::write(&plan_path, &plan_body).await?;
    seed_history(&ctx, 4, 60_000).await?;

    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("keep going", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    let projection = serde_json::to_string(&ctx.compaction.active_messages().await?)?;
    assert!(
        projection.contains("[derived display truncated:"),
        "plan checkpoint 必须有损截短：{}",
        &projection[..projection.len().min(400)]
    );
    assert!(
        !projection.contains("MIDDLE-MARKER"),
        "被省略的中段内容不应出现在投影里"
    );
    assert!(
        projection.contains("TAIL-MARKER"),
        "长内容优先保留头部与尾部：{projection}"
    );
    // 展示上限 = 输入预算/8 = 4,000 token ≈ 12,000 字节（加包装与省略标记的余量）。
    let plan_block = projection
        .split("<active-plan-checkpoint>")
        .nth(1)
        .and_then(|rest| rest.split("</active-plan-checkpoint>").next())
        .expect("plan checkpoint present");
    assert!(
        plan_block.len() <= 12_000 + 512,
        "plan 展示必须受额度约束：{} bytes",
        plan_block.len()
    );
    // 权威 plan 文件不被改写。
    let on_disk = tokio::fs::read_to_string(&plan_path).await?;
    assert_eq!(on_disk, plan_body);
    let _ = tokio::fs::remove_dir_all(&ctx.home).await;
    Ok(())
}

/// 应急摘录只列出「历史上真实出现且仍在 artifact 索引里」的引用，不虚构 URL。
#[tokio::test]
async fn emergency_checkpoint_lists_only_existing_artifact_refs() -> anyhow::Result<()> {
    let backend = SummaryOutageBackend::new(
        SummaryResponse::Unusable,
        vec![CompactionStep::Text("done")],
    );
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-emergency-artifact-refs",
        |cfg| {
            cfg.max_context_tokens = 40_000;
            cfg.context_reserve_tokens = 8_000;
            cfg.context_compact_pct = 50;
            cfg.context_compact_tail_tokens = 1_000;
            cfg.llm_recovery.request_max_retries = 0;
        },
        backend.clone(),
    )
    .await?;
    // 真实存在的 artifact + 一条只在历史文本里出现的伪造引用。
    let record = ctx
        .artifacts
        .write_text("Bash", "long output", &"o".repeat(4_000))?;
    ctx.store
        .add_user(&format!(
            "old task artifact://{} and artifact://deadbeefdeadbeef",
            record.id
        ))
        .await?;
    ctx.store
        .add_assistant(&"y".repeat(60_000), "", &[])
        .await?;
    ctx.store
        .add_user(&format!("older task {}", "z".repeat(60_000)))
        .await?;
    ctx.store.add_assistant("ack", "", &[]).await?;

    let mut executor = TurnExecutor::new(ctx.clone());
    let (decision, _) = executor.execute("keep going", None).await?;

    assert_eq!(decision, TurnDecision::Stop);
    let projection = serde_json::to_string(&ctx.compaction.active_messages().await?)?;
    assert!(
        projection.contains("emergency-context-excerpt"),
        "{projection}"
    );
    assert!(
        projection.contains(&format!("artifact://{}", record.id)),
        "真实存在的 artifact 引用应保留：{projection}"
    );
    assert!(
        !projection.contains("artifact://deadbeefdeadbeef"),
        "索引里不存在的引用不得出现：{projection}"
    );
    let _ = tokio::fs::remove_dir_all(&ctx.home).await;
    Ok(())
}

/// 长跑：摘要永久失败 + 有限窗口，写 `rounds` 轮文件仍须正常结束。
async fn run_summary_outage_endurance(rounds: usize) -> anyhow::Result<()> {
    let mut script: Vec<CompactionStep> = Vec::with_capacity(rounds + 1);
    for index in 0..rounds {
        // 每轮写不同路径：工具调用 ID 唯一，便于统计“精确执行一次”。
        let path: &'static str = Box::leak(format!("grow-{index}.txt").into_boxed_str());
        script.push(CompactionStep::Write(path));
    }
    script.push(CompactionStep::Text("finished"));
    let backend = SummaryOutageBackend::new(SummaryResponse::Unusable, script);
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "turn-summary-outage-endurance",
        |cfg| {
            cfg.max_context_tokens = 120_000;
            cfg.context_reserve_tokens = 20_000;
            cfg.context_compact_pct = 100;
            cfg.context_compact_tail_tokens = 1_000;
            cfg.context_compact_max_output_tokens = 1_000;
            cfg.llm_recovery.request_max_retries = 0;
            // max_turns 不在本测试范围：调高以免长跑被轮次上限截断。
            cfg.max_turns = rounds as i32 + 10;
        },
        backend.clone(),
    )
    .await?;

    let mut executor = TurnExecutor::new(ctx.clone());
    let outcome = executor.execute("write every file", None).await;
    let (decision, _) = outcome?;
    assert_eq!(
        decision,
        TurnDecision::Stop,
        "摘要永久失败不得让 turn 以压缩错误结束"
    );
    assert_eq!(backend.agent_calls(), rounds + 1);
    assert!(
        backend.summary_calls() > 0,
        "摘要服务确实被尝试过（随后失败）"
    );
    assert_eq!(
        crate::regression::tool_result_ids(&ctx.store).await?.len(),
        rounds,
        "每个工具调用只能执行一次"
    );
    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert!(
        !events.contains("\"type\":\"turn_error\""),
        "长跑不得出现压缩类 turn_error：{events}"
    );
    let emergencies = events.matches("\"trigger\":\"emergency\"").count();
    assert!(
        emergencies >= 2,
        "多次超预算应触发多次应急 checkpoint：{emergencies}"
    );
    // 完整历史只追加：所有写调用的参数仍在权威会话记录里。
    let history = serde_json::to_string(&ctx.store.lines_from(0).await?)?;
    assert!(history.contains("grow-0.txt"), "首轮记录必须保留");
    assert!(
        history.contains(&format!("grow-{}.txt", rounds - 1)),
        "末轮记录必须保留"
    );
    Ok(())
}

/// 长跑回归：摘要永久失败 + 有限窗口 → 多轮写文件仍能正常结束；压缩类
/// `turn_error` 不得出现，工具精确执行一次，完整历史只追加。
#[tokio::test]
async fn summary_outage_endurance_keeps_the_turn_alive() -> anyhow::Result<()> {
    run_summary_outage_endurance(40).await
}

/// 同上的 300 轮版本（CI 以 `--features slow-tests -- --include-ignored` 运行）。
#[cfg_attr(not(feature = "slow-tests"), ignore)]
#[tokio::test]
async fn summary_outage_endurance_long_run_300_rounds() -> anyhow::Result<()> {
    run_summary_outage_endurance(300).await
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
