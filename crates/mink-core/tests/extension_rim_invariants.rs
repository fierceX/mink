use anyhow::{Result, anyhow};
use mink::runtime::ext::{
    ReadEvidenceContext, ReadObservation, ToolCallObserver, ToolEffectJournal, ToolExecutionMask,
    TurnPolicyProvider,
};
use mink::runtime::ptc::{PtcBackendKind, PtcMode};
use mink::runtime::{
    AgentOptions, AgentRuntime, AgentRuntimeHandle, CancellationToken, LlmBackend, LlmEvent,
    LlmRequest, LlmResponseStream, LlmStopEvent, LlmTextEvent, LlmToolCallEvent, PostInitContext,
    PostInitHook, RuntimeError, SessionEvent, SessionEventSink, ToolExecutionGate,
    ToolGateDecision, ToolGateRequest, ToolOutputFilter, ToolOutputFilterContext,
    ToolOutputMutation, TurnStatus,
};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn unique_pair(name: &str) -> (PathBuf, PathBuf) {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("mink-ext-rim-{}-{name}-{n}", std::process::id()));
    let home = root.join("home");
    let cwd = root.join("cwd");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    (home, cwd)
}

struct ScriptedBackend {
    scripts: Mutex<VecDeque<Vec<LlmEvent>>>,
}

impl ScriptedBackend {
    fn new(scripts: Vec<Vec<LlmEvent>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
        }
    }
}

#[async_trait::async_trait]
impl LlmBackend for ScriptedBackend {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn stream(&self, _request: LlmRequest) -> Result<LlmResponseStream> {
        let events = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow!("scripted backend ran out of scripts"))?;
        let items: Vec<Result<LlmEvent>> = events.into_iter().map(Ok).collect();
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(items)),
            attempt_count: 1,
        })
    }
}

fn tool_call(name: &str, id: &str, input: Value) -> LlmEvent {
    let fields: BTreeMap<String, String> = input
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    LlmEvent::ToolCall(LlmToolCallEvent {
        name: name.into(),
        id: id.into(),
        input_json: input,
        fields,
        parse_error: None,
        raw_arguments_digest: None,
    })
}

fn text_stop(text: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::Text(LlmTextEvent {
            content: text.into(),
        }),
        LlmEvent::Stop(LlmStopEvent {
            reason: "end_turn".into(),
        }),
    ]
}

fn tool_stop(events: Vec<LlmEvent>) -> Vec<LlmEvent> {
    let mut out = events;
    out.push(LlmEvent::Stop(LlmStopEvent {
        reason: "tool_use".into(),
    }));
    out
}

struct PolicyMaskAndObserver {
    allowed: Vec<String>,
    observed: Arc<Mutex<Vec<String>>>,
    reads_finished: Arc<AtomicUsize>,
    with_journal: bool,
    journal_calls: Arc<AtomicUsize>,
}

struct RecordingObservation {
    reads_finished: Arc<AtomicUsize>,
}

impl ReadObservation for RecordingObservation {
    fn on_succeeded(self: Box<Self>) -> Result<()> {
        self.reads_finished.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct ObserverImpl {
    observed: Arc<Mutex<Vec<String>>>,
    reads_finished: Arc<AtomicUsize>,
}

impl ToolCallObserver for ObserverImpl {
    fn before_prepare_execute(&self, tool_name: &str) -> Result<()> {
        self.observed.lock().unwrap().push(tool_name.to_string());
        Ok(())
    }

    fn observe_read(
        &self,
        tool_name: &str,
        _call_id: &str,
        _input: &Value,
        _ctx: &ReadEvidenceContext<'_>,
    ) -> Result<Option<Box<dyn ReadObservation>>> {
        if tool_name == "Read" {
            return Ok(Some(Box::new(RecordingObservation {
                reads_finished: Arc::clone(&self.reads_finished),
            })));
        }
        Ok(None)
    }
}

struct CountingJournal(Arc<AtomicUsize>);

impl ToolEffectJournal for CountingJournal {
    fn tracks_durable_effects(&self) -> bool {
        true
    }

    fn before_side_effect(&self, _call_id: &str, _tool_name: &str) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl TurnPolicyProvider for PolicyMaskAndObserver {
    fn ordinary_turn_mask(&self) -> Option<ToolExecutionMask> {
        Some(ToolExecutionMask::allow_only(self.allowed.iter().cloned()))
    }

    fn ordinary_effect_journal(&self) -> Option<Arc<dyn ToolEffectJournal>> {
        self.with_journal
            .then(|| Arc::new(CountingJournal(Arc::clone(&self.journal_calls))) as _)
    }

    fn ordinary_tool_observer(&self) -> Option<Arc<dyn ToolCallObserver>> {
        Some(Arc::new(ObserverImpl {
            observed: Arc::clone(&self.observed),
            reads_finished: Arc::clone(&self.reads_finished),
        }))
    }
}

#[tokio::test]
async fn multi_turn_policies_compose_masks_and_observers_and_reject_duplicate_journals()
-> Result<()> {
    let (home, cwd) = unique_pair("multi-policy");
    std::fs::write(cwd.join("note.txt"), "hello world\n")?;

    let obs_a = Arc::new(Mutex::new(Vec::new()));
    let obs_b = Arc::new(Mutex::new(Vec::new()));
    let reads_a = Arc::new(AtomicUsize::new(0));
    let reads_b = Arc::new(AtomicUsize::new(0));
    let journal_calls = Arc::new(AtomicUsize::new(0));

    let backend = Arc::new(ScriptedBackend::new(vec![
        tool_stop(vec![
            tool_call("Read", "c1", serde_json::json!({"path": "note.txt"})),
            tool_call("Glob", "c2", serde_json::json!({"pattern": "*.txt"})),
        ]),
        text_stop("done"),
    ]));

    let options = AgentOptions::new(&home, &cwd)
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1")
        .with_model("flash")
        .with_max_context_tokens(1_000_000)
        .with_llm_backend(backend)
        .with_turn_policy(Arc::new(PolicyMaskAndObserver {
            allowed: vec!["Read".into(), "Glob".into()],
            observed: Arc::clone(&obs_a),
            reads_finished: Arc::clone(&reads_a),
            with_journal: true,
            journal_calls: Arc::clone(&journal_calls),
        }))
        .with_turn_policy(Arc::new(PolicyMaskAndObserver {
            allowed: vec!["Read".into(), "Grep".into()],
            observed: Arc::clone(&obs_b),
            reads_finished: Arc::clone(&reads_b),
            with_journal: false,
            journal_calls: Arc::clone(&journal_calls),
        }));

    let runtime = AgentRuntime::start(options).await?;
    let outcome = runtime.run_turn("read note").await?;
    assert_eq!(outcome.status, TurnStatus::Ok);
    assert_eq!(obs_a.lock().unwrap().as_slice(), &["Read"]);
    assert_eq!(obs_b.lock().unwrap().as_slice(), &["Read"]);
    assert_eq!(reads_a.load(Ordering::SeqCst), 1);
    assert_eq!(reads_b.load(Ordering::SeqCst), 1);

    let conv = std::fs::read_to_string(runtime.session_info().conversation_path.clone())?;
    assert!(
        conv.contains("\"reason\":\"tool_surface\""),
        "Glob must be blocked by mask intersection: {conv}"
    );
    runtime.shutdown().await?;

    // Conflicting ToolEffectJournals must fail closed at startup.
    let (home2, cwd2) = unique_pair("dup-journal");
    let dup = AgentOptions::new(&home2, &cwd2)
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1")
        .with_model("flash")
        .with_llm_backend(Arc::new(ScriptedBackend::new(vec![])))
        .with_turn_policy(Arc::new(PolicyMaskAndObserver {
            allowed: vec!["Read".into()],
            observed: Arc::new(Mutex::new(Vec::new())),
            reads_finished: Arc::new(AtomicUsize::new(0)),
            with_journal: true,
            journal_calls: Arc::new(AtomicUsize::new(0)),
        }))
        .with_turn_policy(Arc::new(PolicyMaskAndObserver {
            allowed: vec!["Read".into()],
            observed: Arc::new(Mutex::new(Vec::new())),
            reads_finished: Arc::new(AtomicUsize::new(0)),
            with_journal: true,
            journal_calls: Arc::new(AtomicUsize::new(0)),
        }));
    let err = match AgentRuntime::start(dup).await {
        Ok(_) => panic!("duplicate ToolEffectJournal must fail closed"),
        Err(err) => err,
    };
    assert!(
        matches!(err, RuntimeError::Configuration(_)),
        "expected configuration error, got {err:?}"
    );
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
    let _ = std::fs::remove_dir_all(home2.parent().unwrap());
    Ok(())
}

struct ReentrantSink {
    seen: Arc<Mutex<Vec<String>>>,
    handle: Mutex<Option<AgentRuntimeHandle>>,
    extra_seen: Arc<AtomicUsize>,
}

struct CountingSink(Arc<AtomicUsize>);

impl SessionEventSink for CountingSink {
    fn on_session_event(&self, _session_id: &str, _event: &SessionEvent) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl SessionEventSink for ReentrantSink {
    fn on_session_event(&self, _session_id: &str, event: &SessionEvent) {
        self.seen.lock().unwrap().push(format!("{event:?}"));
        if let Some(handle) = self.handle.lock().unwrap().take() {
            // Registering another sink from inside a sink callback must never
            // deadlock (lock-free snapshot dispatch invariant).
            let guard = handle
                .ext()
                .register_session_event_sink(Arc::new(CountingSink(Arc::clone(&self.extra_seen))));
            std::mem::forget(guard);
        }
    }
}

struct FailingPostInit;

impl PostInitHook for FailingPostInit {
    fn run(&self, _ctx: &PostInitContext<'_>) -> Result<()> {
        anyhow::bail!("injected post-init failure")
    }
}

#[tokio::test]
async fn multi_sinks_dispatch_lock_free_and_lifecycle_tears_down_lifo() -> Result<()> {
    let (home, cwd) = unique_pair("sinks-lifo");

    // 1. Startup PostInitHook failure triggers LIFO teardown rollback.
    let order = Arc::new(Mutex::new(Vec::new()));
    let o1 = Arc::clone(&order);
    let o2 = Arc::clone(&order);
    let failing_opts = AgentOptions::new(&home, &cwd)
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1")
        .with_model("flash")
        .with_llm_backend(Arc::new(ScriptedBackend::new(vec![])))
        .with_teardown("first", move || o1.lock().unwrap().push("first"))
        .with_teardown("second", move || o2.lock().unwrap().push("second"))
        .with_post_init_hook(Arc::new(FailingPostInit));
    assert!(AgentRuntime::start(failing_opts).await.is_err());
    assert_eq!(order.lock().unwrap().as_slice(), &["second", "first"]);

    // 2. Live multi-sink + lock-free reentrant registration + RAII guard + LIFO shutdown.
    let (home2, cwd2) = unique_pair("sinks-live");
    let seen1 = Arc::new(Mutex::new(Vec::new()));
    let extra_seen = Arc::new(AtomicUsize::new(0));
    let sink1 = Arc::new(ReentrantSink {
        seen: Arc::clone(&seen1),
        handle: Mutex::new(None),
        extra_seen: Arc::clone(&extra_seen),
    });
    let seen2 = Arc::new(AtomicUsize::new(0));
    let sink2 = Arc::new(CountingSink(Arc::clone(&seen2)));

    let shutdown_order = Arc::new(Mutex::new(Vec::new()));
    let s1 = Arc::clone(&shutdown_order);
    let s2 = Arc::clone(&shutdown_order);
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home2, &cwd2)
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_model("flash")
            .with_max_context_tokens(1_000_000)
            .with_llm_backend(Arc::new(ScriptedBackend::new(vec![text_stop("hi")])))
            .with_session_event_sink(sink1.clone())
            .with_teardown("t1", move || s1.lock().unwrap().push("t1"))
            .with_teardown("t2", move || s2.lock().unwrap().push("t2")),
    )
    .await?;

    *sink1.handle.lock().unwrap() = Some(runtime.handle());
    let guard2 = runtime.handle().ext().register_session_event_sink(sink2);
    assert_eq!(runtime.handle().ext().session_event_sink_count(), 2);

    let outcome = runtime.run_turn("hello").await?;
    assert_eq!(outcome.status, TurnStatus::Ok);
    assert!(!seen1.lock().unwrap().is_empty());
    assert!(seen2.load(Ordering::SeqCst) > 0);
    assert!(extra_seen.load(Ordering::SeqCst) > 0);

    drop(guard2);
    assert_eq!(runtime.handle().ext().session_event_sink_count(), 2);
    let late_ran = Arc::new(AtomicUsize::new(0));
    let late_ran_cb = Arc::clone(&late_ran);
    let handle = runtime.handle();
    runtime.shutdown().await?;
    assert_eq!(shutdown_order.lock().unwrap().as_slice(), &["t2", "t1"]);

    // Late teardown registration after dispose must execute immediately.
    handle.ext().register_teardown("late", move || {
        late_ran_cb.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(late_ran.load(Ordering::SeqCst), 1);

    let _ = std::fs::remove_dir_all(home.parent().unwrap());
    let _ = std::fs::remove_dir_all(home2.parent().unwrap());
    Ok(())
}

struct BlockingBashGate {
    id: &'static str,
    seen_ptc_flags: Arc<Mutex<Vec<(String, bool, bool)>>>,
}

#[async_trait::async_trait]
impl ToolExecutionGate for BlockingBashGate {
    fn id(&self) -> &str {
        self.id
    }

    async fn check(
        &self,
        req: &ToolGateRequest<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolGateDecision> {
        self.seen_ptc_flags.lock().unwrap().push((
            req.tool_name.to_string(),
            req.is_ptc_nested,
            req.is_sub_agent,
        ));
        if req.tool_name == "Bash" {
            return Ok(ToolGateDecision::Deny {
                code: "POLICY_NO_BASH".into(),
                reason: "bash disabled by enterprise policy".into(),
            });
        }
        Ok(ToolGateDecision::Allow)
    }
}

struct FailingGate;

#[async_trait::async_trait]
impl ToolExecutionGate for FailingGate {
    fn id(&self) -> &str {
        "failing-gate"
    }

    async fn check(
        &self,
        _req: &ToolGateRequest<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolGateDecision> {
        anyhow::bail!("remote policy service down")
    }
}

struct HangingGate {
    timeout: Duration,
}

#[async_trait::async_trait]
impl ToolExecutionGate for HangingGate {
    fn id(&self) -> &str {
        "hanging-gate"
    }

    fn timeout(&self) -> Duration {
        self.timeout
    }

    async fn check(
        &self,
        _req: &ToolGateRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<ToolGateDecision> {
        cancel.cancelled().await;
        Ok(ToolGateDecision::Allow)
    }
}

#[tokio::test]
async fn gate_blocks_direct_ptc_and_subagent_and_fails_closed_with_critical_audit() -> Result<()> {
    // 1. Direct Bash blocked + ToolEffectJournal NOT invoked + PolicyEffect persisted.
    let (home, cwd) = unique_pair("gate-direct");
    let seen_flags = Arc::new(Mutex::new(Vec::new()));
    let journal_calls = Arc::new(AtomicUsize::new(0));
    let backend = Arc::new(ScriptedBackend::new(vec![
        tool_stop(vec![tool_call(
            "Bash",
            "call-bash-1",
            serde_json::json!({"command": "echo should_not_run"}),
        )]),
        text_stop("blocked"),
    ]));
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_model("flash")
            .with_max_context_tokens(1_000_000)
            .with_llm_backend(backend)
            .with_turn_policy(Arc::new(PolicyMaskAndObserver {
                allowed: vec!["Bash".into(), "Read".into()],
                observed: Arc::new(Mutex::new(Vec::new())),
                reads_finished: Arc::new(AtomicUsize::new(0)),
                with_journal: true,
                journal_calls: Arc::clone(&journal_calls),
            }))
            .with_tool_execution_gate(Arc::new(BlockingBashGate {
                id: "enterprise-guard",
                seen_ptc_flags: Arc::clone(&seen_flags),
            })),
    )
    .await?;
    let info = runtime.session_info().clone();
    let outcome = runtime.run_turn("run bash").await?;
    assert_eq!(outcome.status, TurnStatus::Ok);
    assert_eq!(
        journal_calls.load(Ordering::SeqCst),
        0,
        "denied tool must never enter ToolEffectJournal::before_side_effect"
    );
    assert!(
        seen_flags
            .lock()
            .unwrap()
            .contains(&("Bash".into(), false, false))
    );
    runtime.shutdown().await?;

    let events_text = std::fs::read_to_string(&info.events_path)?;
    assert!(
        events_text.contains("\"type\":\"policy_effect\"")
            && events_text.contains("\"code\":\"POLICY_NO_BASH\""),
        "events.jsonl must record critical PolicyEffect: {events_text}"
    );
    let conv_text = std::fs::read_to_string(&info.conversation_path)?;
    assert!(
        conv_text.contains("\"policy_effect\"") && conv_text.contains("\"reason\":\"policy\""),
        "conversation.jsonl must record _mink.policy_effect and ToolBlocker::Policy: {conv_text}"
    );

    // 2. SubAgent inherits parent gates.
    let (home_sub, cwd_sub) = unique_pair("gate-subagent");
    let sub_flags = Arc::new(Mutex::new(Vec::new()));
    let sub_backend = Arc::new(ScriptedBackend::new(vec![
        // Parent calls SubAgent
        tool_stop(vec![tool_call(
            "SubAgent",
            "call-sub-1",
            serde_json::json!({"prompt": "try bash", "description": "child"}),
        )]),
        // Child tries Bash -> denied by inherited gate -> child finishes
        tool_stop(vec![tool_call(
            "Bash",
            "child-bash-1",
            serde_json::json!({"command": "echo nope"}),
        )]),
        text_stop("child blocked"),
        // Parent finishes
        text_stop("parent done"),
    ]));
    let sub_rt = AgentRuntime::start(
        AgentOptions::new(&home_sub, &cwd_sub)
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_model("flash")
            .with_max_context_tokens(1_000_000)
            .with_llm_backend(sub_backend)
            .with_tool_execution_gate(Arc::new(BlockingBashGate {
                id: "enterprise-guard",
                seen_ptc_flags: Arc::clone(&sub_flags),
            })),
    )
    .await?;
    let sub_outcome = sub_rt.run_turn("delegate").await?;
    assert_eq!(sub_outcome.status, TurnStatus::Ok);
    assert!(
        sub_flags
            .lock()
            .unwrap()
            .iter()
            .any(|(tool, nested, is_sub)| tool == "Bash" && !nested && *is_sub),
        "sub-agent must inherit parent ToolExecutionGate: {:?}",
        sub_flags.lock().unwrap()
    );
    sub_rt.shutdown().await?;

    // 3. PTC RunCode nested call is gated when python3 is available.
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let (home_ptc, cwd_ptc) = unique_pair("gate-ptc");
        let ptc_flags = Arc::new(Mutex::new(Vec::new()));
        let ptc_backend = Arc::new(ScriptedBackend::new(vec![
            tool_stop(vec![tool_call(
                "RunCode",
                "call-ptc-1",
                serde_json::json!({
                    "description": "nested bash",
                    "code": "res = await tools.Bash(command='echo ptc')\nprint(res)"
                }),
            )]),
            text_stop("ptc finished"),
        ]));
        let ptc_rt = AgentRuntime::start(
            AgentOptions::new(&home_ptc, &cwd_ptc)
                .with_api_key("test-key")
                .with_base_url("https://example.invalid/v1")
                .with_model("flash")
                .with_max_context_tokens(1_000_000)
                .with_ptc_mode(PtcMode::Tool)
                .with_ptc_backend(PtcBackendKind::SubprocessPython)
                .with_llm_backend(ptc_backend)
                .with_tool_execution_gate(Arc::new(BlockingBashGate {
                    id: "enterprise-guard",
                    seen_ptc_flags: Arc::clone(&ptc_flags),
                })),
        )
        .await?;
        let ptc_outcome = ptc_rt.run_turn("run ptc").await?;
        assert_eq!(ptc_outcome.status, TurnStatus::Ok);
        assert!(
            ptc_flags
                .lock()
                .unwrap()
                .iter()
                .any(|(tool, nested, _)| tool == "Bash" && *nested),
            "nested PTC Bash call must pass through ToolExecutionGate: {:?}",
            ptc_flags.lock().unwrap()
        );
        ptc_rt.shutdown().await?;
        let _ = std::fs::remove_dir_all(home_ptc.parent().unwrap());
    }

    // 4. Failing gate and timeout gate fail-closed; user interrupt aborts hanging gate immediately.
    let (home_err, cwd_err) = unique_pair("gate-err-timeout");
    let err_backend = Arc::new(ScriptedBackend::new(vec![
        tool_stop(vec![tool_call(
            "Bash",
            "c-err",
            serde_json::json!({"command": "echo x"}),
        )]),
        text_stop("after err"),
    ]));
    let err_rt = AgentRuntime::start(
        AgentOptions::new(&home_err, &cwd_err)
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_model("flash")
            .with_max_context_tokens(1_000_000)
            .with_llm_backend(err_backend)
            .with_tool_execution_gate(Arc::new(FailingGate)),
    )
    .await?;
    let err_info = err_rt.session_info().clone();
    assert_eq!(err_rt.run_turn("go").await?.status, TurnStatus::Ok);
    err_rt.shutdown().await?;
    assert!(std::fs::read_to_string(&err_info.events_path)?.contains("POLICY_GATE_ERROR"));

    let (home_int, cwd_int) = unique_pair("gate-interrupt");
    let int_backend = Arc::new(ScriptedBackend::new(vec![tool_stop(vec![tool_call(
        "Bash",
        "c-hang",
        serde_json::json!({"command": "echo x"}),
    )])]));
    let int_rt = AgentRuntime::start(
        AgentOptions::new(&home_int, &cwd_int)
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_model("flash")
            .with_max_context_tokens(1_000_000)
            .with_llm_backend(int_backend)
            .with_tool_execution_gate(Arc::new(HangingGate {
                timeout: Duration::from_secs(30),
            })),
    )
    .await?;
    let handle = int_rt.handle();
    let interrupt_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        handle.interrupt_current_turn();
    });
    let outcome = tokio::time::timeout(Duration::from_secs(3), int_rt.run_turn("hang"))
        .await
        .expect("interrupt must unblock hanging gate promptly")?;
    interrupt_task.await?;
    assert_eq!(outcome.status, TurnStatus::Interrupted);
    int_rt.shutdown().await?;

    let _ = std::fs::remove_dir_all(home.parent().unwrap());
    let _ = std::fs::remove_dir_all(home_sub.parent().unwrap());
    let _ = std::fs::remove_dir_all(home_err.parent().unwrap());
    let _ = std::fs::remove_dir_all(home_int.parent().unwrap());
    Ok(())
}

struct SecretRedactingFilter;

impl ToolOutputFilter for SecretRedactingFilter {
    fn id(&self) -> &str {
        "secret-redactor"
    }

    fn filter(
        &self,
        _ctx: &ToolOutputFilterContext<'_>,
        output: &mut ToolOutputMutation,
    ) -> Result<()> {
        if output.content.contains("SECRET_TOKEN_XYZ") {
            output.content = output.content.replace("SECRET_TOKEN_XYZ", "[REDACTED]");
            output.conv_content = output
                .conv_content
                .replace("SECRET_TOKEN_XYZ", "[REDACTED]");
            output.mark_mutated("SECRET_REDACTED", Some("redacted secret token".into()));
        }
        Ok(())
    }
}

struct FailingFilter;

impl ToolOutputFilter for FailingFilter {
    fn id(&self) -> &str {
        "failing-filter"
    }

    fn filter(
        &self,
        _ctx: &ToolOutputFilterContext<'_>,
        _output: &mut ToolOutputMutation,
    ) -> Result<()> {
        anyhow::bail!("DLP engine crashed")
    }
}

#[tokio::test]
async fn output_filter_redacts_before_artifact_spill_and_invalidates_read_snapshots() -> Result<()>
{
    let (home, cwd) = unique_pair("filter-artifact");
    let secret_file = cwd.join("secret.txt");
    let payload = format!("{}\nTAIL SECRET_TOKEN_XYZ\n", "A".repeat(800));
    std::fs::write(&secret_file, &payload)?;

    // Turn 1: Read large file with small tool_result_max_bytes so it spills to artifact://.
    // Then attempt Edit with a fabricated/stale tag — must fail because mutated Read
    // invalidated the FileSnapshotStore.
    let backend = Arc::new(ScriptedBackend::new(vec![
        tool_stop(vec![tool_call(
            "Read",
            "read-secret-1",
            serde_json::json!({"path": "secret.txt"}),
        )]),
        tool_stop(vec![tool_call(
            "Edit",
            "edit-after-redact",
            serde_json::json!({
                "input": "[secret.txt#AAAA]\nPUT 1.=1:\n+tampered"
            }),
        )]),
        text_stop("done"),
    ]));

    let opts = AgentOptions::new(&home, &cwd)
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1")
        .with_model("flash")
        .with_max_context_tokens(1_000_000)
        .with_llm_backend(backend)
        .with_tool_output_filter(Arc::new(SecretRedactingFilter))
        .with_tool_options(mink::runtime::ToolOptions {
            result_max_bytes: 256,
            ..Default::default()
        });

    let runtime = AgentRuntime::start(opts).await?;
    let info = runtime.session_info().clone();
    let outcome = runtime.run_turn("read and edit").await?;
    assert_eq!(outcome.status, TurnStatus::Ok);
    assert_eq!(runtime.handle().ext().tool_hook_stats().filters, 1);

    // Drop a runtime-attached filter guard and verify zero residual.
    let guard = runtime
        .handle()
        .ext()
        .register_tool_output_filter(Arc::new(FailingFilter));
    assert_eq!(runtime.handle().ext().tool_hook_stats().filters, 2);
    drop(guard);
    assert_eq!(runtime.handle().ext().tool_hook_stats().filters, 1);

    runtime.shutdown().await?;

    // Verify zero plaintext secret on disk in artifacts/ and conversation.jsonl.
    let conv = std::fs::read_to_string(&info.conversation_path)?;
    assert!(
        !conv.contains("SECRET_TOKEN_XYZ"),
        "conversation must not contain unredacted secret: {conv}"
    );
    assert!(
        conv.contains("artifact://read-0001"),
        "expected spilled artifact reference: {conv}"
    );
    assert!(
        conv.contains("\"state\":\"failed\"") && conv.contains("edit-after-redact"),
        "Edit after invalidated Read snapshot must fail: {conv}"
    );
    for entry in std::fs::read_dir(&info.artifacts_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("txt") {
            let body = std::fs::read_to_string(&path)?;
            assert!(
                !body.contains("SECRET_TOKEN_XYZ"),
                "artifact {} must not leak unredacted secret",
                path.display()
            );
            assert!(body.contains("[REDACTED]"));
        }
    }
    let events = std::fs::read_to_string(&info.events_path)?;
    assert!(
        events.contains("\"type\":\"policy_effect\"") && events.contains("SECRET_REDACTED"),
        "events.jsonl must record PolicyEffect for redaction: {events}"
    );

    // File must remain untouched because Read snapshot was invalidated.
    assert_eq!(std::fs::read_to_string(&secret_file)?, payload);

    // Filter error must fail closed without leaking plaintext.
    let (home_fail, cwd_fail) = unique_pair("filter-fail-closed");
    std::fs::write(cwd_fail.join("plain.txt"), "TOP_SECRET_PLAIN\n")?;
    let fail_backend = Arc::new(ScriptedBackend::new(vec![
        tool_stop(vec![tool_call(
            "Read",
            "read-fail",
            serde_json::json!({"path": "plain.txt"}),
        )]),
        text_stop("ok"),
    ]));
    let fail_rt = AgentRuntime::start(
        AgentOptions::new(&home_fail, &cwd_fail)
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_model("flash")
            .with_max_context_tokens(1_000_000)
            .with_llm_backend(fail_backend)
            .with_tool_output_filter(Arc::new(FailingFilter)),
    )
    .await?;
    let fail_info = fail_rt.session_info().clone();
    fail_rt.run_turn("read").await?;
    fail_rt.shutdown().await?;
    let fail_conv = std::fs::read_to_string(&fail_info.conversation_path)?;
    assert!(!fail_conv.contains("TOP_SECRET_PLAIN"));
    assert!(fail_conv.contains("fail-closed"));

    let _ = std::fs::remove_dir_all(home.parent().unwrap());
    let _ = std::fs::remove_dir_all(home_fail.parent().unwrap());
    Ok(())
}
