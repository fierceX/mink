use anyhow::{Result, anyhow};
use mink::runtime::{
    AgentOptions, CancellationToken, LlmBackend, LlmEvent, LlmRequest, LlmResponseStream,
    LlmStopEvent, LlmTextEvent, LlmToolCallEvent, ModelRequestHook, ModelRequestHookContext,
    ModelRequestPatch, SessionEvent, SessionEventSink, ToolExecutionGate, ToolGateDecision,
    ToolGateRequest, ToolOutputFilter, ToolOutputFilterContext, ToolOutputMutation, TurnStatus,
};
use mink_rim::{
    BundleActivation, Disposer, RimBundle, RimInstallContext, RimProfile, RuntimeAssembler, Scope,
    ScopeGuard, ScopeSegmentError, SeatError, SeatRegistry, scopes_related,
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
fn seat_registry_three_state_arbitration_and_fail_closed_duplicates() {
    let reg: SeatRegistry<dyn DummyProvider> = SeatRegistry::new("memory");
    assert_eq!(
        reg.resolve(None).err(),
        Some(SeatError::Unavailable { seat: "memory" })
    );

    let scope = Scope::root();
    let d1 = reg
        .register("p1", Arc::new(ProviderImpl("p1", true)), |p| {
            matches!(p.tag(), "p1")
        })
        .expect("initial registration succeeds");
    assert_eq!(reg.resolve(None).unwrap().tag(), "p1");
    assert_eq!(reg.resolve(Some("p1")).unwrap().tag(), "p1");
    assert!(matches!(
        reg.resolve(Some("missing")),
        Err(SeatError::UnknownProvider { .. })
    ));

    // Duplicate ID registration fails closed instead of silently overwriting.
    assert_eq!(
        reg.register_in(&scope, "p1", Arc::new(ProviderImpl("p1", true)), |_| true)
            .err(),
        Some(SeatError::DuplicateProvider {
            seat: "memory",
            id: "p1".into(),
        })
    );

    // Once the old disposer runs, re-registering the same ID succeeds.
    d1.dispose();
    assert!(reg.is_empty());
    reg.register_in(&scope, "p1", Arc::new(ProviderImpl("p1", true)), |_| true)
        .expect("re-registration after dispose succeeds");
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.resolve(Some("p1")).unwrap().tag(), "p1");

    // Multiple available -> Ambiguous (never guesses).
    reg.register_in(&scope, "p2", Arc::new(ProviderImpl("p2", true)), |_| true)
        .unwrap();
    assert!(matches!(
        reg.resolve(None),
        Err(SeatError::Ambiguous { .. })
    ));
    assert_eq!(reg.resolve(Some("p2")).unwrap().tag(), "p2");

    // Unavailable explicit provider -> Unavailable (never falls back to p1).
    reg.register_in(&scope, "p3", Arc::new(ProviderImpl("p3", false)), |p| {
        p.is_ready()
    })
    .unwrap();
    assert_eq!(
        reg.resolve(Some("p3")).err(),
        Some(SeatError::Unavailable { seat: "memory" })
    );

    // Closing or disposed scope rejects new registrations fail-closed.
    let closing_scope = Scope::root();
    closing_scope.close();
    assert_eq!(
        reg.register_in(
            &closing_scope,
            "p4",
            Arc::new(ProviderImpl("p4", true)),
            |_| true
        )
        .err(),
        Some(SeatError::ScopeClosed {
            seat: "memory",
            scope: "".into(),
        })
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
    assert!(rim.attach_session_event_sink(
        &turn_scope,
        "turn-sink",
        Arc::new(RecordingSink(Arc::clone(&sink_hits))),
    ));
    assert!(rim.attach_tool_output_filter(
        &turn_scope,
        "turn-filter",
        Arc::new(RedactFilter("TOKEN_999".into())),
    ));
    assert!(rim.attach_tool_execution_gate(
        &turn_scope,
        "turn-gate",
        Arc::new(DenyCommandGate {
            pattern: "extra_bad".into(),
        }),
    ));
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

    // Attaching onto a closed/disposed scope fails closed (returns false, zero registration).
    assert!(!rim.attach_tool_execution_gate(
        &turn_scope,
        "late-gate",
        Arc::new(DenyCommandGate {
            pattern: "late".into(),
        }),
    ));
    assert_eq!(rim.handle().ext().tool_hook_stats().gates, 1);

    assert_eq!(runtime.run_turn("second").await?.status, TurnStatus::Ok);
    let info = runtime.session_info().clone();
    let root_handle = Arc::clone(rim.root_scope());
    runtime.shutdown().await?;

    // Shutdown closed and disposed root scope -> bundle disposer ran, late attaches rejected.
    assert_eq!(teardowns.lock().unwrap().as_slice(), &["guard"]);
    assert!(root_handle.is_closing());
    assert!(root_handle.is_disposed());
    assert!(!rim.attach_tool_output_filter(
        &root_handle,
        "after-shutdown",
        Arc::new(RedactFilter("X".into())),
    ));

    let conv = std::fs::read_to_string(&info.conversation_path)?;
    assert!(conv.contains("RIM_BLOCKED_CMD"));
    assert!(conv.contains("***"));
    assert!(conv.contains("TOKEN_999"));

    let _ = std::fs::remove_dir_all(home_fail.parent().unwrap());
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
    Ok(())
}

struct InvalidIdBundle(&'static str);

impl RimBundle for InvalidIdBundle {
    fn id(&self) -> &'static str {
        self.0
    }

    fn install(&self, _ctx: &mut RimInstallContext<'_>, _patch: &Value) -> Result<()> {
        Ok(())
    }
}

struct SeatProviderBundle(&'static str, &'static str);

impl RimBundle for SeatProviderBundle {
    fn id(&self) -> &'static str {
        self.0
    }

    fn install(&self, ctx: &mut RimInstallContext<'_>, _patch: &Value) -> Result<()> {
        ctx.register_seat_provider::<dyn DummyProvider>(
            "memory",
            self.1,
            Arc::new(ProviderImpl(self.1, true)),
            |p| p.is_ready(),
        )?;
        Ok(())
    }
}

struct EphemeralNoteHook(String);

#[async_trait::async_trait]
impl ModelRequestHook for EphemeralNoteHook {
    fn id(&self) -> &str {
        "rim-ephemeral-note"
    }

    async fn before_request(
        &self,
        ctx: &ModelRequestHookContext<'_>,
        patch: &mut ModelRequestPatch,
        _cancel: &CancellationToken,
    ) -> Result<()> {
        patch.push_ephemeral_block(
            "rim-scope-note",
            format!("scope={} note={}", ctx.scope_path, self.0),
        );
        Ok(())
    }
}

struct CapturingBackend {
    scripts: Mutex<VecDeque<Vec<LlmEvent>>>,
    captured_messages: Arc<Mutex<Vec<Vec<Value>>>>,
}

#[async_trait::async_trait]
impl LlmBackend for CapturingBackend {
    fn name(&self) -> &str {
        "capturing"
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        self.captured_messages
            .lock()
            .unwrap()
            .push(request.messages.as_ref().clone());
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

#[tokio::test]
async fn rim_invalid_bundle_ids_seat_hub_and_scope_isolation_invariants() -> Result<()> {
    // 1. Invalid bundle.id() containing '/' returns Err (no panic!) and rolls back earlier bundle.
    let teardowns = Arc::new(Mutex::new(Vec::new()));
    let assembler = RuntimeAssembler::new()
        .register_bundle(Arc::new(GuardBundle {
            teardowns: Arc::clone(&teardowns),
        }))
        .register_bundle(Arc::new(InvalidIdBundle("bad/bundle")));
    let (h1, c1) = unique_pair("invalid-id");
    let bad_profile = RimProfile {
        name: "bad-id".into(),
        bundles: vec![
            BundleActivation {
                id: "guard-bundle".into(),
                enabled: true,
                patch: Value::Null,
            },
            BundleActivation {
                id: "bad/bundle".into(),
                enabled: true,
                patch: Value::Null,
            },
        ],
        seat_overrides: BTreeMap::new(),
    };
    let mut opts1 = AgentOptions::new(&h1, &c1)
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1")
        .with_model("flash");
    let err = match assembler.apply_profile(&bad_profile, &mut opts1) {
        Ok(_) => panic!("invalid bundle id must fail closed"),
        Err(err) => err,
    };
    assert!(
        err.downcast_ref::<ScopeSegmentError>().is_some()
            || err.to_string().contains("SCOPE_SEGMENT_INVALID")
    );
    assert_eq!(teardowns.lock().unwrap().as_slice(), &["guard"]);

    // 2. Duplicate enabled bundle ID fails closed.
    let dup_profile = RimProfile {
        name: "dup".into(),
        bundles: vec![
            BundleActivation {
                id: "guard-bundle".into(),
                enabled: true,
                patch: Value::Null,
            },
            BundleActivation {
                id: "guard-bundle".into(),
                enabled: true,
                patch: Value::Null,
            },
        ],
        seat_overrides: BTreeMap::new(),
    };
    let mut opts_dup = AgentOptions::new(&h1, &c1)
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1")
        .with_model("flash");
    assert!(
        assembler
            .apply_profile(&dup_profile, &mut opts_dup)
            .is_err()
    );

    // 3. Scope::try_fork + close keeps child closed.
    let p_scope = Scope::root();
    p_scope.close();
    let c_scope = p_scope.try_fork("late").unwrap();
    assert!(c_scope.is_closing());

    // 4. SeatHub + seat_overrides + scoped primary vs subagent RimSession isolation.
    let seat_assembler = RuntimeAssembler::new()
        .register_bundle(Arc::new(SeatProviderBundle("mem-a", "provider-a")))
        .register_bundle(Arc::new(SeatProviderBundle("mem-b", "provider-b")));
    let (h2, c2) = unique_pair("rim-seats-scopes");
    let captured_messages = Arc::new(Mutex::new(Vec::new()));
    let backend = Arc::new(CapturingBackend {
        scripts: Mutex::new(
            vec![
                // Primary round 1 -> SubAgent
                vec![
                    tool_call(
                        "SubAgent",
                        "sub-1",
                        serde_json::json!({
                            "prompt": "run blocked bash",
                            "description": "child task"
                        }),
                    ),
                    LlmEvent::Stop(LlmStopEvent {
                        reason: "tool_use".into(),
                    }),
                ],
                // SubAgent round 1 -> Bash
                vec![
                    tool_call(
                        "Bash",
                        "sub-bash",
                        serde_json::json!({"command": "echo primary_only_cmd"}),
                    ),
                    LlmEvent::Stop(LlmStopEvent {
                        reason: "tool_use".into(),
                    }),
                ],
                // SubAgent final
                vec![
                    LlmEvent::Text(LlmTextEvent {
                        content: "child done".into(),
                    }),
                    LlmEvent::Stop(LlmStopEvent {
                        reason: "end_turn".into(),
                    }),
                ],
                // Primary round 2 -> final
                vec![
                    LlmEvent::Text(LlmTextEvent {
                        content: "parent done".into(),
                    }),
                    LlmEvent::Stop(LlmStopEvent {
                        reason: "end_turn".into(),
                    }),
                ],
            ]
            .into_iter()
            .collect(),
        ),
        captured_messages: Arc::clone(&captured_messages),
    });
    let profile2 = RimProfile {
        name: "seats-and-scopes".into(),
        bundles: vec![
            BundleActivation {
                id: "mem-a".into(),
                enabled: true,
                patch: Value::Null,
            },
            BundleActivation {
                id: "mem-b".into(),
                enabled: true,
                patch: Value::Null,
            },
        ],
        seat_overrides: BTreeMap::from([("memory".into(), "provider-b".into())]),
    };
    let opts2 = AgentOptions::new(&h2, &c2)
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1")
        .with_model("flash")
        .with_max_context_tokens(1_000_000)
        .with_llm_backend(backend);

    let (runtime, rim) = seat_assembler.start_runtime(&profile2, opts2).await?;
    let resolved = rim.resolve_seat::<dyn DummyProvider>("memory")?;
    assert_eq!(resolved.tag(), "provider-b");

    // Primary-scoped gate & ModelRequestHook must NOT fire in SubAgent!
    let primary_task = rim.fork_scope("task-1");
    assert_eq!(primary_task.path(), "primary/task-1");
    assert!(rim.attach_tool_execution_gate(
        &primary_task,
        "primary-gate",
        Arc::new(DenyCommandGate {
            pattern: "primary_only_cmd".into(),
        }),
    ));
    assert!(rim.attach_model_request_hook(
        &primary_task,
        "primary-note",
        Arc::new(EphemeralNoteHook("only-parent".into())),
    ));
    // Unrelated subagent scope must NOT fire on either primary or the actual child.
    let foreign_sub = rim.try_fork_subagent_scope("nonexistent-subagent")?;
    assert!(rim.attach_model_request_hook(
        &foreign_sub,
        "foreign-note",
        Arc::new(EphemeralNoteHook("never-seen".into())),
    ));

    let outcome = runtime.run_turn("delegate").await?;
    assert_eq!(outcome.status, TurnStatus::Ok);
    let info2 = runtime.session_info().clone();
    runtime.shutdown().await?;

    // Check captured backend requests:
    // Request 0 (primary r1) and 3 (primary r2) see `only-parent`, NOT `never-seen`.
    // Request 1 & 2 (subagent) see neither, and Request 2 proves `sub-bash` succeeded
    // with `primary_only_cmd` (was NOT blocked by primary_task's gate)!
    let reqs = captured_messages.lock().unwrap();
    assert_eq!(reqs.len(), 4);
    let has_text = |msgs: &[Value], pat: &str| msgs.iter().any(|m| m.to_string().contains(pat));
    assert!(has_text(&reqs[0], "only-parent"));
    assert!(!has_text(&reqs[1], "only-parent"));
    assert!(!has_text(&reqs[2], "only-parent"));
    assert!(has_text(&reqs[2], "primary_only_cmd"));
    assert!(!has_text(&reqs[2], "RIM_BLOCKED_CMD"));
    assert!(has_text(&reqs[3], "only-parent"));
    for r in reqs.iter() {
        assert!(!has_text(r, "never-seen"));
    }
    drop(reqs);

    let conv2 = std::fs::read_to_string(&info2.conversation_path)?;
    assert!(!conv2.contains("RIM_BLOCKED_CMD"));
    // Ephemeral hook blocks must NEVER persist into conversation.jsonl.
    assert!(!conv2.contains("only-parent"));

    let _ = std::fs::remove_dir_all(h1.parent().unwrap());
    let _ = std::fs::remove_dir_all(h2.parent().unwrap());
    Ok(())
}
