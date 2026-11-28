//!
//! ScriptedBackend 捕获每一个 LlmRequest，断言任意请求的模型可见输入都能从
//! 会话耐久产物离线重建：
//! 1. system_prompt == events.jsonl 最后一个 prefix_snapshot 的 system_prompt；
//! 2. tools == 同一快照的 tools_json（即构建时 tool_surface.schemas() 的序列化）；
//! 3. messages == conversation.jsonl 行前缀 + 计划尾置投影（语义等值）。
//!
//! 场景覆盖：确认计划 + 工具循环 + 失败工具（可能触发轨迹证据注入，注入行
//! 同样落在 conversation.jsonl，重建不受影响）。

use anyhow::{Result, anyhow};
use mink::runtime::{
    AgentOptions, AgentRuntime, LlmBackend, LlmEvent, LlmRequest, LlmResponseStream, LlmStopEvent,
    LlmTextEvent, LlmToolCallEvent,
};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct CapturedRequest {
    system_prompt: String,
    messages: Vec<Value>,
    tools: Vec<Value>,
}

struct ScriptedBackend {
    scripts: Mutex<VecDeque<Vec<LlmEvent>>>,
    captured: Mutex<Vec<CapturedRequest>>,
}

impl ScriptedBackend {
    fn new(scripts: Vec<Vec<LlmEvent>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter().collect()),
            captured: Mutex::new(Vec::new()),
        }
    }

    fn captured(&self) -> Vec<CapturedRequest> {
        let mut guard = self.captured.lock().unwrap();
        let mut out = Vec::new();
        out.append(&mut guard);
        out
    }
}

#[async_trait::async_trait]
impl LlmBackend for ScriptedBackend {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        let record = CapturedRequest {
            system_prompt: request.system_prompt.to_string(),
            messages: (*request.messages).clone(),
            tools: (*request.tools).clone(),
        };
        self.captured.lock().unwrap().push(record);
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

fn read_tool_call(id: &str, path: &str) -> LlmEvent {
    tool_call("Read", id, serde_json::json!({"path": path}))
}

fn tool_call(name: &str, id: &str, input: Value) -> LlmEvent {
    let fields: BTreeMap<String, String> = input
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    LlmEvent::ToolCall(LlmToolCallEvent {
        name: name.into(),
        id: id.to_string(),
        input_json: input,
        fields,
        parse_error: None,
        raw_arguments_digest: None,
    })
}

fn text_stop(text: &str, reason: &str) -> Vec<LlmEvent> {
    vec![
        LlmEvent::Text(LlmTextEvent {
            content: text.to_string(),
        }),
        LlmEvent::Stop(LlmStopEvent {
            reason: reason.to_string(),
        }),
    ]
}

fn prefix_snapshots(events_path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(events_path)?;
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|evt| evt.get("type").and_then(Value::as_str) == Some("prefix_snapshot"))
        .collect())
}

fn conversation_rows(path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path)?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line).map_err(|e| anyhow!("bad conversation row: {e}"))
        })
        .collect()
}

fn temp_pair(name: &str) -> (PathBuf, PathBuf) {
    let root =
        std::env::temp_dir().join(format!("mink-invariants-{}-{}", std::process::id(), name));
    let home = root.join("home");
    let cwd = root.join("cwd");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    (home, cwd)
}

#[tokio::test]
async fn requests_rebuild_from_durable_session_logs() -> Result<()> {
    let (home, cwd) = temp_pair("rebuild");
    std::fs::write(cwd.join("fixture.txt"), "alpha beta")?;

    let backend = Arc::new(ScriptedBackend::new(vec![
        vec![
            read_tool_call("call_read", "fixture.txt"),
            LlmEvent::Stop(LlmStopEvent {
                reason: "tool_use".to_string(),
            }),
        ],
        text_stop("done", "end_turn"),
        text_stop("ok", "end_turn"),
        vec![
            read_tool_call("call_missing", "missing.txt"),
            LlmEvent::Stop(LlmStopEvent {
                reason: "tool_use".to_string(),
            }),
        ],
        text_stop("recovered", "end_turn"),
    ]));

    let options = AgentOptions::new(home.clone(), cwd.clone()).with_llm_backend(backend.clone());
    let runtime = AgentRuntime::start(options)
        .await
        .map_err(|e| anyhow!("start: {e}"))?;

    // 确认计划：写 runtime 读取的同一个 plan 文件。
    let plan_path = runtime.session_info().plan_path.clone();
    std::fs::write(&plan_path, "# Verified plan")?;

    for prompt in ["read fixture", "confirm", "read missing"] {
        runtime
            .run_turn(prompt)
            .await
            .map_err(|e| anyhow!("turn: {e}"))?;
    }

    let captured = backend.captured();
    assert_eq!(
        captured.len(),
        5,
        "three turns consume exactly five requests"
    );

    let snapshots = prefix_snapshots(&runtime.session_info().events_path)?;
    assert_eq!(snapshots.len(), 1, "one prefix build for the whole session");
    let snapshot = &snapshots[0];
    let snapshot_prompt = snapshot["system_prompt"]
        .as_str()
        .ok_or_else(|| anyhow!("snapshot missing system_prompt"))?;
    let snapshot_tools = snapshot["tools_json"]
        .as_array()
        .ok_or_else(|| anyhow!("snapshot missing tools_json"))?;

    let rows = conversation_rows(&runtime.session_info().conversation_path)?;

    // 场景自证：失败工具必须触发轨迹证据注入（默认 signal 配置），
    // 注入行同样落在 conversation.jsonl，被下面的前缀重建覆盖。
    let has_trajectory = rows.iter().any(|row| {
        row.get("content")
            .and_then(Value::as_str)
            .is_some_and(|c| c.contains("[trajectory]"))
    });
    assert!(
        has_trajectory,
        "failed tool must trigger trajectory evidence injection when signal policy is enabled"
    );

    assert!(plan_path.exists());

    // 每个捕获请求必须能从耐久产物重建。
    let mut previous_k = 0usize;
    for request in &captured {
        assert_eq!(request.system_prompt, snapshot_prompt);
        assert_eq!(request.tools.as_slice(), snapshot_tools.as_slice());
        let matched_k = (previous_k..=rows.len())
            .find(|k| request.messages == rows[..*k])
            .ok_or_else(|| anyhow!("no durable row prefix rebuilds request messages"))?;
        previous_k = matched_k;
    }

    runtime
        .shutdown()
        .await
        .map_err(|e| anyhow!("shutdown: {e}"))?;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
    Ok(())
}

#[tokio::test]
async fn warning_preserves_external_save_after_accepted_edit() -> Result<()> {
    use mink::runtime::EditMode;
    struct ExternalEditor {
        backend: ScriptedBackend,
        file: PathBuf,
    }
    #[async_trait::async_trait]
    impl LlmBackend for ExternalEditor {
        fn name(&self) -> &str {
            "external-editor"
        }

        async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
            // Third request is strictly after the real Edit result was committed.
            if self.backend.captured.lock().unwrap().len() == 2 {
                assert_eq!(std::fs::read_to_string(&self.file)?, "lineX\nline2\n");
                let replacement = self.file.with_extension("new");
                std::fs::write(&replacement, "lineX \r\nexternal\r\n")?;
                std::fs::rename(replacement, &self.file)?;
            }
            self.backend.stream(request).await
        }
    }
    let original = "line1\nline2\n";
    for mode in [EditMode::Hashline, EditMode::Replace] {
        let (home, cwd) = temp_pair(&format!("concurrent-save-{mode:?}"));
        let file = cwd.join("a.rs");
        std::fs::write(&file, original)?;
        let input = match mode {
            EditMode::Hashline => {
                let tag = xxhash_rust::xxh32::xxh32(original.as_bytes(), 0) & 0xffff;
                serde_json::json!({"input":format!("[a.rs#{tag:04X}]\nPUT 1.=1:\n+lineX\n")})
            }
            EditMode::Replace => {
                serde_json::json!({"path":"a.rs","edits":[{"old_text":"line1","new_text":"lineX"}]})
            }
        };
        let stop = || {
            LlmEvent::Stop(LlmStopEvent {
                reason: "tool_use".into(),
            })
        };
        let mut failures: Vec<_> = (0..5)
            .map(|i| {
                tool_call(
                    "Bash",
                    &format!("f{i}"),
                    serde_json::json!({"command":format!("false check {i}")}),
                )
            })
            .collect();
        failures.push(stop());
        let backend = Arc::new(ExternalEditor {
            file: file.clone(),
            backend: ScriptedBackend::new(vec![
                vec![read_tool_call("read", "a.rs:1-10"), stop()],
                vec![tool_call("Edit", "edit", input), stop()],
                failures,
                text_stop("done", "end_turn"),
            ]),
        });
        let runtime = AgentRuntime::start(
            AgentOptions::new(&home, &cwd)
                .with_edit_mode(mode)
                .with_llm_backend(backend.clone()),
        )
        .await?;
        runtime.run_turn("edit then check").await?;
        assert_eq!(std::fs::read(&file)?, b"lineX \r\nexternal\r\n");
        let requests = backend.backend.captured();
        assert_eq!(requests.len(), 4);
        assert!(requests[3].messages.iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|v| v.contains("[edit-recovery]") && v.contains("a.rs"))
        }));
        runtime.shutdown().await?;
        std::fs::remove_dir_all(home.parent().unwrap())?;
    }
    Ok(())
}

#[tokio::test]
async fn plain_examples_remain_one_request_on_the_public_runtime() -> Result<()> {
    let (home, cwd) = temp_pair("example-only");
    let example = r#"{"name":"Read","arguments":{"path":"sample.txt"}}"#;
    std::fs::write(cwd.join("sample.txt"), "external marker")?;
    for body in [
        format!("Example: {example}"),
        format!("<tool_call>{example}</tool_call>"),
    ] {
        let backend = Arc::new(ScriptedBackend::new(vec![text_stop(&body, "end_turn")]));
        let runtime =
            AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
                .await?;
        runtime
            .run_turn("Show the call as an example only.")
            .await?;
        assert_eq!(backend.captured().len(), 1);
        let rows = conversation_rows(&runtime.session_info().conversation_path)?;
        assert!(
            !rows
                .iter()
                .filter_map(|row| row["content"].as_array())
                .flatten()
                .any(|b| b["type"] == "tool_use" || b["type"] == "tool_result")
        );
        runtime.shutdown().await?;
    }
    std::fs::remove_dir_all(home.parent().unwrap())?;
    Ok(())
}

#[tokio::test]
async fn ptc_exclusive_end_to_end_pipeline_and_off_mode_invariants() -> Result<()> {
    // 1. Off mode leaves wire schemas and system prompt completely untouched.
    let (home_off, cwd_off) = temp_pair("ptc-off");
    let backend_off = Arc::new(ScriptedBackend::new(vec![text_stop("ok", "end_turn")]));
    let runtime_off = AgentRuntime::start(
        AgentOptions::new(&home_off, &cwd_off).with_llm_backend(backend_off.clone()),
    )
    .await?;
    runtime_off.run_turn("hello").await?;
    let cap_off = backend_off.captured();
    assert_eq!(cap_off.len(), 1);
    assert!(!cap_off[0].system_prompt.contains("<ptc-sdk>"));
    assert!(
        !cap_off[0]
            .tools
            .iter()
            .any(|t| t["name"].as_str() == Some("RunCode"))
    );
    assert!(
        cap_off[0]
            .tools
            .iter()
            .any(|t| t["name"].as_str() == Some("Read"))
    );
    runtime_off.shutdown().await?;
    std::fs::remove_dir_all(home_off.parent().unwrap())?;

    // 2. Exclusive mode: wire exposes RunCode + planning/control tools (Todo*, Plan*, SubAgent),
    //    prompt has <ptc-mode> + <ptc-sdk>, direct Read fails with PTC hint, direct TodoWrite +
    //    RunCode (Glob -> gather(Read) -> Edit + large inspect spill) execute cleanly.
    let (home, cwd) = temp_pair("ptc-exclusive");
    std::fs::write(cwd.join("one.txt"), "alpha one\n")?;
    std::fs::write(cwd.join("two.txt"), "alpha two\n")?;

    let py_program = r#"
files = (await tools.Glob(pattern="*.txt"))["files"]
reads = await gather(*(tools.Read(path=p) for p in sorted(files)))
target = reads[0]
patch = f"[{target['path']}#{target['tag']}]\nPUT 1.=1:\n+beta one\n"
ed = await tools.Edit(input=patch)
inspect({"count": len(reads), "applied": ed["applied"], "pad": "x" * 16000})
"#;

    let stop = || {
        LlmEvent::Stop(LlmStopEvent {
            reason: "tool_use".into(),
        })
    };
    let backend = Arc::new(ScriptedBackend::new(vec![
        // Round 1: model mistakenly calls Read directly -> rejected with PTC hint
        vec![read_tool_call("direct_read", "one.txt"), stop()],
        // Round 2: model calls direct TodoWrite on wire alongside RunCode orchestration
        vec![
            tool_call(
                "TodoWrite",
                "todo_1",
                serde_json::json!({"base_revision": 0, "add": [{"content": "Patch one.txt"}]}),
            ),
            tool_call(
                "RunCode",
                "ptc_1",
                serde_json::json!({"code": py_program, "description": "Read files and edit one.txt"}),
            ),
            stop(),
        ],
        // Round 3: finish
        text_stop("completed via RunCode", "end_turn"),
    ]));

    let ptc_cfg = mink::runtime::PtcConfig {
        mode: mink::runtime::PtcMode::Exclusive,
        ..Default::default()
    };
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_ptc(ptc_cfg)
            .with_tool_options(mink::runtime::ToolOptions {
                result_max_bytes: 2_048,
                ..Default::default()
            })
            .with_llm_backend(backend.clone()),
    )
    .await?;

    let turn_outcome = runtime.run_turn("orchestrate edits").await?;
    assert!(
        turn_outcome.error.is_none(),
        "turn failed: {:?}",
        turn_outcome.error
    );
    let requests = backend.captured();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].system_prompt.contains("<ptc-mode>"));
    assert!(requests[0].system_prompt.contains("<ptc-sdk>"));
    for required_wire in ["RunCode", "TodoRead", "TodoWrite", "TodoAdvance"] {
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|t| t["name"].as_str() == Some(required_wire)),
            "missing wire tool {required_wire}"
        );
    }
    assert!(!requests[0].tools.iter().any(|t| matches!(
        t["name"].as_str(),
        Some("Read" | "Edit" | "Bash" | "Python")
    )));

    // Direct Read rejection explains await tools.Read(...)
    let direct_rejection = serde_json::to_string(&requests[1].messages)?;
    assert!(
        direct_rejection.contains("only callable inside RunCode"),
        "{direct_rejection}"
    );

    // RunCode updated file on disk, updated todo snapshot, and spilled large inspect output to artifact://
    let ptc_reply = serde_json::to_string(&requests[2].messages)?;
    assert!(!ptc_reply.contains("RunCode failed"), "{ptc_reply}");
    assert_eq!(std::fs::read_to_string(cwd.join("one.txt"))?, "beta one\n");
    assert!(ptc_reply.contains("artifact://"), "{ptc_reply}");
    assert!(ptc_reply.contains("<current-todos"), "{ptc_reply}");

    runtime.shutdown().await?;
    std::fs::remove_dir_all(home.parent().unwrap())?;
    Ok(())
}

#[tokio::test]
async fn ptc_library_embedding_parameters_custom_tools_and_runtime_seams() -> Result<()> {
    use mink::runtime::ptc::{
        CancellationToken, InterruptSignal, PtcBackendKind, PtcBindingDispatcher, PtcConfig,
        PtcMode, PtcOutputItem, PtcRunRequest, PtcRunResult, PtcRunSpec, PtcRuntime,
        ToolPtcExposure,
    };
    use mink::runtime::{
        AgentEvent, AgentEventKind, AgentTool, EventSink, SessionEvent, SessionEventSink,
        ToolDefinition, ToolError, ToolExecutionContext, ToolOutput, ui::ToolPresentation,
    };

    struct LookupRecordTool;
    #[async_trait::async_trait]
    impl AgentTool for LookupRecordTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(
                "LookupRecord",
                "Fetch a structured domain record by id.",
                serde_json::json!({
                    "type": "object",
                    "properties": {"id": {"type": "string"}},
                    "required": ["id"],
                    "additionalProperties": false
                }),
            )
            .with_ptc_exposure(ToolPtcExposure::ProgrammaticOnly)
        }

        async fn execute(
            &self,
            input: Value,
            _ctx: ToolExecutionContext,
        ) -> std::result::Result<ToolOutput, ToolError> {
            let id = input
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ToolError::argument("id is required"))?;
            Ok(ToolOutput::text(format!("record:{id}"))
                .with_structured_value(serde_json::json!({"id": id, "score": 42})))
        }
    }

    struct DirectNoticeTool;
    #[async_trait::async_trait]
    impl AgentTool for DirectNoticeTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(
                "DirectNotice",
                "Emit a direct-only host notice.",
                serde_json::json!({
                    "type": "object",
                    "properties": {"msg": {"type": "string"}},
                    "required": ["msg"],
                    "additionalProperties": false
                }),
            )
            .with_ptc_exposure(ToolPtcExposure::DirectOnly)
        }

        async fn execute(
            &self,
            input: Value,
            _ctx: ToolExecutionContext,
        ) -> std::result::Result<ToolOutput, ToolError> {
            let msg = input.get("msg").and_then(Value::as_str).unwrap_or("");
            Ok(ToolOutput::text(format!("noticed:{msg}")))
        }
    }

    #[derive(Default)]
    struct CaptureTurnEvents(Mutex<Vec<AgentEvent>>);
    #[async_trait::async_trait]
    impl EventSink for CaptureTurnEvents {
        async fn on_event(&self, event: AgentEvent) -> std::result::Result<(), String> {
            self.0.lock().unwrap().push(event);
            Ok(())
        }
    }

    #[derive(Default)]
    struct CaptureSessionEvents(Mutex<Vec<SessionEvent>>);
    impl SessionEventSink for CaptureSessionEvents {
        fn on_session_event(&self, _session_id: &str, event: &SessionEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    // 1. Embedded Hybrid PTC with individual AgentOptions::with_ptc_* parameters, custom-only
    //    enabled_tools, ProgrammaticOnly + DirectOnly custom tools, EventSink & SessionEventSink.
    let (home, cwd) = temp_pair("ptc-embed-params");
    let turn_sink = Arc::new(CaptureTurnEvents::default());
    let session_sink = Arc::new(CaptureSessionEvents::default());
    let backend = Arc::new(ScriptedBackend::new(vec![
        vec![
            tool_call(
                "DirectNotice",
                "direct_1",
                serde_json::json!({"msg": "starting"}),
            ),
            tool_call(
                "RunCode",
                "ptc_embed_1",
                serde_json::json!({
                    "code": "rec = await tools.LookupRecord(id='abc')\ninspect(rec['id'], rec['score'])",
                    "description": "Query custom LookupRecord via PTC"
                }),
            ),
            LlmEvent::Stop(LlmStopEvent {
                reason: "tool_use".into(),
            }),
        ],
        text_stop("embedded ptc done", "end_turn"),
    ]));

    let options = AgentOptions::new(&home, &cwd)
        .with_ptc_mode(PtcMode::Tool)
        .with_ptc_backend(PtcBackendKind::SubprocessPython)
        .with_ptc_timeout_secs(30)
        .with_ptc_max_timeout_secs(120)
        .with_ptc_max_parallel_sub_calls(4)
        .with_ptc_max_sub_calls_per_minute(120)
        .with_ptc_max_output_bytes(32_768)
        .with_ptc_max_message_bytes(65_536)
        .with_ptc_max_pending_calls(16)
        .with_ptc_stop_grace_ms(500)
        .with_tool(Arc::new(LookupRecordTool))
        .with_tool(Arc::new(DirectNoticeTool))
        .with_enabled_tools(["LookupRecord".to_string(), "DirectNotice".to_string()])
        .with_event_sink(turn_sink.clone())
        .with_session_event_sink(session_sink.clone())
        .with_log_events(false)
        .with_llm_backend(backend.clone());

    options.validate_runtime_config()?;
    let runtime = AgentRuntime::start(options).await?;
    let outcome = runtime.run_turn("run embedded ptc").await?;
    assert!(outcome.error.is_none(), "turn failed: {:?}", outcome.error);
    assert_eq!(outcome.text, "embedded ptc done");

    let requests = backend.captured();
    assert_eq!(requests.len(), 2);
    // Wire has RunCode + DirectNotice, NOT LookupRecord (ProgrammaticOnly).
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|t| t["name"].as_str() == Some("RunCode"))
    );
    assert!(
        requests[0]
            .tools
            .iter()
            .any(|t| t["name"].as_str() == Some("DirectNotice"))
    );
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|t| t["name"].as_str() == Some("LookupRecord"))
    );
    // <ptc-sdk> documents LookupRecord and excludes DirectOnly DirectNotice.
    assert!(
        requests[0]
            .system_prompt
            .contains("async def LookupRecord(*, id: str)")
    );
    assert!(!requests[0].system_prompt.contains("async def DirectNotice"));

    let msg_json = serde_json::to_string(&requests[1].messages)?;
    assert!(msg_json.contains("noticed:starting"), "{msg_json}");
    assert!(msg_json.contains("abc 42"), "{msg_json}");

    runtime.shutdown().await?;

    {
        let captured_turn_events = turn_sink.0.lock().unwrap();
        assert!(captured_turn_events.iter().any(|ev| matches!(
            &ev.kind,
            AgentEventKind::ToolResult {
                tool_name,
                presentation: Some(ToolPresentation::PtcRun(run)),
                ..
            } if tool_name == "RunCode"
                && run.sub_calls.len() == 1
                && run.sub_calls[0].tool_name == "LookupRecord"
        )));
    }

    {
        let captured_session_events = session_sink.0.lock().unwrap();
        assert!(captured_session_events.iter().any(|ev| matches!(
            ev,
            SessionEvent::PtcDispatchStart { tool_name, .. } if tool_name == "LookupRecord"
        )));
        assert!(captured_session_events.iter().any(|ev| matches!(
            ev,
            SessionEvent::PtcDispatchEnd { tool_name, .. } if tool_name == "LookupRecord"
        )));
        assert!(captured_session_events.iter().any(|ev| matches!(
            ev,
            SessionEvent::PtcRunSummary {
                sub_call_count: 1,
                outcome,
                ..
            } if outcome == "ok"
        )));
    }
    std::fs::remove_dir_all(home.parent().unwrap())?;

    // 2. Custom PtcRuntime implemented purely from external public mink::runtime::ptc exports,
    //    including CancellationToken, InterruptSignal, and PtcConfig::resolve_run_spec.
    struct CustomEchoPtcRuntime {
        cfg: PtcConfig,
    }
    #[async_trait::async_trait]
    impl PtcRuntime for CustomEchoPtcRuntime {
        fn kind(&self) -> PtcBackendKind {
            PtcBackendKind::WasiPython
        }
        fn resolve(&self, request: PtcRunRequest) -> Result<PtcRunSpec> {
            let _cancel: &CancellationToken = &request.cancel;
            let _interrupt: &InterruptSignal = &request.interrupt;
            self.cfg.resolve_run_spec(request)
        }
        async fn run(
            &self,
            _spec: PtcRunSpec,
            dispatcher: Arc<dyn PtcBindingDispatcher>,
        ) -> Result<PtcRunResult> {
            let res = dispatcher
                .invoke("LookupRecord", serde_json::json!({"id": "custom-rt"}))
                .await
                .map_err(|e| anyhow!("dispatch error: {}", e.message))?;
            Ok(PtcRunResult {
                output: vec![PtcOutputItem::Text(format!(
                    "custom:{}",
                    res.value["id"].as_str().unwrap_or("")
                ))],
                output_truncated: false,
                error: None,
            })
        }
    }

    let (home2, cwd2) = temp_pair("ptc-custom-runtime");
    let custom_cfg = PtcConfig::exclusive()
        .with_backend(PtcBackendKind::WasiPython)
        .with_timeout_secs(20);
    let backend2 = Arc::new(ScriptedBackend::new(vec![
        vec![
            tool_call(
                "RunCode",
                "ptc_custom_1",
                serde_json::json!({"code": "pass", "description": "custom runtime test"}),
            ),
            LlmEvent::Stop(LlmStopEvent {
                reason: "tool_use".into(),
            }),
        ],
        text_stop("custom runtime ok", "end_turn"),
    ]));
    let opts2 = AgentOptions::new(&home2, &cwd2)
        .with_ptc(custom_cfg.clone())
        .with_ptc_runtime(Arc::new(CustomEchoPtcRuntime { cfg: custom_cfg }))
        .with_tool(Arc::new(LookupRecordTool))
        .with_enabled_tools(["LookupRecord".to_string()])
        .with_llm_backend(backend2.clone());
    opts2.validate_runtime_config()?;
    let rt2 = AgentRuntime::start(opts2).await?;
    let outcome2 = rt2.run_turn("invoke custom runtime").await?;
    assert!(
        outcome2.error.is_none(),
        "custom runtime failed: {:?}",
        outcome2.error
    );
    let reqs2 = backend2.captured();
    let msg2 = serde_json::to_string(&reqs2[1].messages)?;
    assert!(msg2.contains("custom:custom-rt"), "{msg2}");
    rt2.shutdown().await?;
    std::fs::remove_dir_all(home2.parent().unwrap())?;
    Ok(())
}
