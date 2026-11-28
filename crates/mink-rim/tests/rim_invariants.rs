use anyhow::{Result, anyhow};
use mink::runtime::{
    AgentOptions, CancellationToken, LlmBackend, LlmEvent, LlmRequest, LlmResponseStream,
    LlmStopEvent, LlmTextEvent, LlmToolCallEvent, SessionEvent, SessionEventSink,
    ToolExecutionGate, ToolGateDecision, ToolGateRequest, ToolOutputFilter,
    ToolOutputFilterContext, ToolOutputMutation, TurnStatus,
};
use mink_rim::{
    BundleActivation, Disposer, RimBundle, RimInstallContext, RimProfile, RuntimeAssembler, Scope,
    ScopeGuard, SeatError, SeatRegistry, scopes_related,
};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn unique_pair(name: &str) -> (PathBuf, PathBuf) {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("mink-rim-{}-{name}-{n}", std::process::id()));
    let home = root.join("home");
    let cwd = root.join("cwd");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    (home, cwd)
}

#[test]
fn scope_lifo_pruning_and_late_registration() {
    let root = Scope::root();
    let order = Arc::new(Mutex::new(Vec::new()));
    let o1 = Arc::clone(&order);
    let o2 = Arc::clone(&order);
    root.push_labeled(
        "first",
        Disposer::new(move || o1.lock().unwrap().push("first")),
    );

    let child = root.fork("child");
    let oc = Arc::clone(&order);
    child.push_labeled(
        "child-item",
        Disposer::new(move || oc.lock().unwrap().push("child-item")),
    );
    child.dispose();
    assert!(child.is_disposed());
    assert_eq!(order.lock().unwrap().as_slice(), &["child-item"]);

    // Pushing into parent prunes spent child entry automatically.
    root.push_labeled(
        "second",
        Disposer::new(move || o2.lock().unwrap().push("second")),
    );
    assert_eq!(root.live_labels(), vec!["first", "second"]);

    // Idempotent LIFO dispose.
    root.dispose();
    root.dispose();
    assert_eq!(
        order.lock().unwrap().as_slice(),
        &["child-item", "second", "first"]
    );
    assert!(root.live_labels().is_empty());

    // Late registration runs immediately.
    let late = Arc::new(AtomicUsize::new(0));
    let late_cb = Arc::clone(&late);
    root.push_labeled(
        "late",
        Disposer::new(move || {
            late_cb.fetch_add(1, Ordering::SeqCst);
        }),
    );
    assert_eq!(late.load(Ordering::SeqCst), 1);

    // ScopeGuard disposes on drop.
    let guarded = Scope::root();
    let dropped = Arc::new(AtomicUsize::new(0));
    let dropped_cb = Arc::clone(&dropped);
    guarded.push_labeled(
        "guard-item",
        Disposer::new(move || {
            dropped_cb.fetch_add(1, Ordering::SeqCst);
        }),
    );
    {
        let _guard = ScopeGuard::new(Arc::clone(&guarded));
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(guarded.is_disposed());
}

#[test]
fn scopes_related_isolates_siblings_and_partial_prefixes() {
    assert!(scopes_related("", "session/s1/ext/a"));
    assert!(scopes_related("session/s1/ext/a", ""));
    assert!(scopes_related("session/s1", "session/s1/ext/a"));
    assert!(scopes_related("session/s1/ext/a", "session/s1/ext/a"));
    assert!(!scopes_related("session/s1/ext/a", "session/s1/ext/b"));
    assert!(!scopes_related("session/s1/ext/a", "session/s1/ext/a2"));
}

trait DummyProvider: Send + Sync {
    fn tag(&self) -> &'static str;
    fn is_ready(&self) -> bool;
}

struct ProviderImpl(&'static str, bool);

impl DummyProvider for ProviderImpl {
    fn tag(&self) -> &'static str {
        self.0
    }

    fn is_ready(&self) -> bool {
        self.1
    }
}

#[test]
fn seat_registry_three_state_arbitration_and_stale_token_safety() {
    let reg: SeatRegistry<dyn DummyProvider> = SeatRegistry::new("memory");
    assert_eq!(
        reg.resolve(None).err(),
        Some(SeatError::Unavailable { seat: "memory" })
    );

    let scope = Scope::root();
    let d1 = reg.register("p1", Arc::new(ProviderImpl("p1", true)), |p| {
        matches!(p.tag(), "p1")
    });
    assert_eq!(reg.resolve(None).unwrap().tag(), "p1");
    assert_eq!(reg.resolve(Some("p1")).unwrap().tag(), "p1");
    assert!(matches!(
        reg.resolve(Some("missing")),
        Err(SeatError::UnknownProvider { .. })
    ));

    // Overwrite p1 with a newer instance; disposing old d1 must NOT evict new p1.
    reg.register_in(&scope, "p1", Arc::new(ProviderImpl("p1", true)), |_| true);
    d1.dispose();
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.resolve(Some("p1")).unwrap().tag(), "p1");

    // Multiple available -> Ambiguous (never guesses).
    reg.register_in(&scope, "p2", Arc::new(ProviderImpl("p2", true)), |_| true);
    assert!(matches!(
        reg.resolve(None),
        Err(SeatError::Ambiguous { .. })
    ));
    assert_eq!(reg.resolve(Some("p2")).unwrap().tag(), "p2");

    // Unavailable explicit provider -> Unavailable (never falls back to p1).
    reg.register_in(&scope, "p3", Arc::new(ProviderImpl("p3", false)), |p| {
        p.is_ready()
    });
    assert_eq!(
        reg.resolve(Some("p3")).err(),
        Some(SeatError::Unavailable { seat: "memory" })
    );

    scope.dispose();
    assert!(reg.is_empty());
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
            .ok_or_else(|| anyhow!("out of scripts"))?;
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

struct DenyCommandGate {
    pattern: String,
}

#[async_trait::async_trait]
impl ToolExecutionGate for DenyCommandGate {
    fn id(&self) -> &str {
        "rim-deny-gate"
    }

    async fn check(
        &self,
        req: &ToolGateRequest<'_>,
        _cancel: &CancellationToken,
    ) -> Result<ToolGateDecision> {
        if req.tool_name == "Bash"
            && req
                .input
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|cmd| cmd.contains(&self.pattern))
        {
            return Ok(ToolGateDecision::Deny {
                code: "RIM_BLOCKED_CMD".into(),
                reason: format!("blocked command matching {}", self.pattern),
            });
        }
        Ok(ToolGateDecision::Allow)
    }
}

struct RedactFilter(String);

impl ToolOutputFilter for RedactFilter {
    fn id(&self) -> &str {
        "rim-redact-filter"
    }

    fn filter(
        &self,
        _ctx: &ToolOutputFilterContext<'_>,
        output: &mut ToolOutputMutation,
    ) -> Result<()> {
        if output.content.contains(&self.0) {
            output.content = output.content.replace(&self.0, "***");
            output.conv_content = output.conv_content.replace(&self.0, "***");
            output.mark_mutated("RIM_REDACTED", None);
        }
        Ok(())
    }
}

struct RecordingSink(Arc<AtomicUsize>);

impl SessionEventSink for RecordingSink {
    fn on_session_event(&self, _session_id: &str, _event: &SessionEvent) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct GuardBundle {
    teardowns: Arc<Mutex<Vec<String>>>,
}

impl RimBundle for GuardBundle {
    fn id(&self) -> &'static str {
        "guard-bundle"
    }

    fn install(&self, ctx: &mut RimInstallContext<'_>, patch: &Value) -> Result<()> {
        let pattern = patch["deny_pattern"]
            .as_str()
            .unwrap_or("rm -rf")
            .to_string();
        ctx.options()
            .add_tool_execution_gate(Arc::new(DenyCommandGate { pattern }));
        let teardowns = Arc::clone(&self.teardowns);
        ctx.push_disposer(
            "guard-cleanup",
            Disposer::new(move || teardowns.lock().unwrap().push("guard".into())),
        );
        Ok(())
    }
}

struct FailingBundle;

impl RimBundle for FailingBundle {
    fn id(&self) -> &'static str {
        "failing-bundle"
    }

    fn install(&self, _ctx: &mut RimInstallContext<'_>, _patch: &Value) -> Result<()> {
        anyhow::bail!("bad bundle patch")
    }
}

#[tokio::test]
async fn assembler_rollback_and_scoped_rim_session_zero_residual() -> Result<()> {
    let teardowns = Arc::new(Mutex::new(Vec::new()));
    let assembler = RuntimeAssembler::new()
        .register_bundle(Arc::new(GuardBundle {
            teardowns: Arc::clone(&teardowns),
        }))
        .register_bundle(Arc::new(FailingBundle));

    let (home_fail, cwd_fail) = unique_pair("rim-rollback");
    let bad_profile = RimProfile {
        name: "bad".into(),
        bundles: vec![
            BundleActivation {
                id: "guard-bundle".into(),
                enabled: true,
                patch: serde_json::json!({"deny_pattern": "forbidden"}),
            },
            BundleActivation {
                id: "failing-bundle".into(),
                enabled: true,
                patch: Value::Null,
            },
        ],
        seat_overrides: BTreeMap::new(),
    };
    let mut bad_opts = AgentOptions::new(&home_fail, &cwd_fail)
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1")
        .with_model("flash");
    assert!(
        assembler
            .apply_profile(&bad_profile, &mut bad_opts)
            .is_err()
    );
    assert_eq!(
        teardowns.lock().unwrap().as_slice(),
        &["guard"],
        "bundle #2 failure must atomically roll back bundle #1"
    );

    // Full end-to-end RimSession lifecycle + zero residual on scope dispose.
    teardowns.lock().unwrap().clear();
    let (home, cwd) = unique_pair("rim-e2e");
    let profile = RimProfile {
        name: "e2e".into(),
        bundles: vec![BundleActivation {
            id: "guard-bundle".into(),
            enabled: true,
            patch: serde_json::json!({"deny_pattern": "forbidden"}),
        }],
        seat_overrides: BTreeMap::new(),
    };
    let backend = Arc::new(ScriptedBackend::new(vec![
        // Turn 1: blocked by bundle gate + Read redacted by scoped filter
        vec![
            tool_call(
                "Bash",
                "c1",
                serde_json::json!({"command": "echo forbidden"}),
            ),
            tool_call(
                "Bash",
                "c2",
                serde_json::json!({"command": "printf TOKEN_999"}),
            ),
            LlmEvent::Stop(LlmStopEvent {
                reason: "tool_use".into(),
            }),
        ],
        vec![
            LlmEvent::Text(LlmTextEvent {
                content: "turn1".into(),
            }),
            LlmEvent::Stop(LlmStopEvent {
                reason: "end_turn".into(),
            }),
        ],
        // Turn 2 (after scoped filter & scoped gate disposed): TOKEN_999 is not redacted
        vec![
            tool_call(
                "Bash",
                "c3",
                serde_json::json!({"command": "printf TOKEN_999"}),
            ),
            LlmEvent::Stop(LlmStopEvent {
                reason: "tool_use".into(),
            }),
        ],
        vec![
            LlmEvent::Text(LlmTextEvent {
                content: "turn2".into(),
            }),
            LlmEvent::Stop(LlmStopEvent {
                reason: "end_turn".into(),
            }),
        ],
    ]));
    let opts = AgentOptions::new(&home, &cwd)
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1")
        .with_model("flash")
        .with_max_context_tokens(1_000_000)
        .with_llm_backend(backend);

    let (runtime, rim) = assembler.start_runtime(&profile, opts).await?;
    assert_eq!(rim.handle().ext().tool_hook_stats().gates, 1);

    let turn_scope = rim.fork_scope("turn-1");
    let sink_hits = Arc::new(AtomicUsize::new(0));
    rim.attach_session_event_sink(
        &turn_scope,
        "turn-sink",
        Arc::new(RecordingSink(Arc::clone(&sink_hits))),
    );
    rim.attach_tool_output_filter(
        &turn_scope,
        "turn-filter",
        Arc::new(RedactFilter("TOKEN_999".into())),
    );
    rim.attach_tool_execution_gate(
        &turn_scope,
        "turn-gate",
        Arc::new(DenyCommandGate {
            pattern: "extra_bad".into(),
        }),
    );
    assert_eq!(rim.handle().ext().tool_hook_stats().gates, 2);
    assert_eq!(rim.handle().ext().tool_hook_stats().filters, 1);
    assert_eq!(rim.handle().ext().session_event_sink_count(), 1);

    assert_eq!(runtime.run_turn("first").await?.status, TurnStatus::Ok);
    assert!(sink_hits.load(Ordering::SeqCst) > 0);

    // Dispose child scope -> zero residual of scoped hooks!
    turn_scope.dispose();
    assert!(turn_scope.live_labels().is_empty());
    assert_eq!(rim.handle().ext().tool_hook_stats().gates, 1);
    assert_eq!(rim.handle().ext().tool_hook_stats().filters, 0);
    assert_eq!(rim.handle().ext().session_event_sink_count(), 0);

    assert_eq!(runtime.run_turn("second").await?.status, TurnStatus::Ok);
    let info = runtime.session_info().clone();
    runtime.shutdown().await?;

    // Shutdown disposed root scope -> bundle disposer ran.
    assert_eq!(teardowns.lock().unwrap().as_slice(), &["guard"]);
    assert!(rim.root_scope().is_disposed());

    let conv = std::fs::read_to_string(&info.conversation_path)?;
    assert!(conv.contains("RIM_BLOCKED_CMD"));
    assert!(conv.contains("***"));
    assert!(conv.contains("TOKEN_999"));

    let _ = std::fs::remove_dir_all(home_fail.parent().unwrap());
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
    Ok(())
}
