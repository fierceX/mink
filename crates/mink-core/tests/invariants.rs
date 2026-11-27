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
    AgentOptions, AgentRuntime, GoalSpec, GoalStatus, LlmBackend, LlmEvent, LlmRequest,
    LlmResponseStream, LlmStopEvent, LlmTextEvent, LlmToolCallEvent, SessionPolicy,
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

/// Goal 契约是持久快照上的 compare-and-set：陈旧 revision 不能确认契约，重开
/// 会话后恢复的是同一契约身份与 revision，而不是一份新草案。
#[tokio::test]
async fn goal_contract_revision_is_a_cas_and_survives_a_reopen() -> Result<()> {
    let (home, cwd) = temp_pair("goal-recovery");
    let options = || {
        AgentOptions::new(home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(ScriptedBackend::new(vec![])))
    };
    let runtime = AgentRuntime::start(options())
        .await
        .map_err(|e| anyhow!("start: {e}"))?;
    let draft = runtime
        .goals()
        .draft(GoalSpec::new("ship the goal"), 3_600)
        .map_err(|e| anyhow!("draft: {e}"))?;
    assert!(
        runtime
            .goals()
            .confirm(&draft.id, draft.revision + 1)
            .is_err(),
        "a stale revision must not confirm the contract"
    );
    let confirmed = runtime
        .goals()
        .confirm(&draft.id, draft.revision)
        .map_err(|e| anyhow!("confirm: {e}"))?;
    assert_eq!(confirmed.status, GoalStatus::Active);
    let sid = runtime.session_info().session_id.clone();
    runtime
        .shutdown()
        .await
        .map_err(|e| anyhow!("shutdown: {e}"))?;

    let runtime = AgentRuntime::start(options().with_session(SessionPolicy::Resume(sid)))
        .await
        .map_err(|e| anyhow!("reopen: {e}"))?;
    let restored = runtime
        .goals()
        .snapshot()
        .ok_or_else(|| anyhow!("goal missing after reopen"))?;
    assert_eq!(restored.id, confirmed.id);
    assert_eq!(restored.contract_revision, confirmed.contract_revision);
    assert_eq!(restored.spec, confirmed.spec);
    assert_eq!(restored.status, GoalStatus::Active);
    // The recovery pass republishes the snapshot, so the CAS token advances: a
    // caller still holding the pre-reopen revision is refused instead of acting
    // on a state it no longer describes, and the restored revision works.
    assert!(
        runtime
            .goals()
            .update_limits(
                &restored.id,
                confirmed.revision,
                restored.spec.limits.clone()
            )
            .is_err(),
        "a revision from before the reopen must be stale"
    );
    let adjusted = runtime
        .goals()
        .update_limits(
            &restored.id,
            restored.revision,
            restored.spec.limits.clone(),
        )
        .map_err(|e| anyhow!("limits: {e}"))?;
    assert_eq!(adjusted.status, GoalStatus::Active);
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

    // 2. Exclusive mode: wire exposes RunCode + planning/control tools (Todo*, Plan*, Goal*, SubAgent),
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
    for required_wire in [
        "RunCode",
        "TodoRead",
        "TodoWrite",
        "TodoAdvance",
        "GoalRead",
    ] {
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
