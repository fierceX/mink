//! Goal rounds must never rewrite the parts of a request that are already on
//! the wire.
//!
//! The goal state used to be mounted as the first request message, so every
//! round boundary changed the whole provider prefix and the entire active
//! window was re-read without a cache hit. These tests pin the append-only
//! contract instead: every request extends the previous one, the immutable
//! prefix is built once per session, and the per-round goal facts travel with
//! the appended round prompt.

use anyhow::{Result, anyhow};
use mink::runtime::{
    AgentOptions, AgentRuntime, GoalSpec, GoalStatus, LlmBackend, LlmEvent, LlmRequest,
    LlmResponseStream, LlmStopEvent, LlmTextEvent, LlmToolCallEvent,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Captures every request and plays a deterministic two-round goal script.
struct GoalScriptBackend {
    calls: Mutex<Vec<CapturedRequest>>,
}

struct CapturedRequest {
    system_prompt: String,
    tools: Vec<Value>,
    messages: Vec<Value>,
}

impl GoalScriptBackend {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }

    fn captured(&self) -> Vec<CapturedRequest> {
        let mut guard = self.calls.lock().unwrap();
        std::mem::take(&mut *guard)
    }
}

/// The wire shape of one message: transport strips the engine metadata, so
/// prefix reuse is decided by exactly this projection.
fn wire(message: &Value) -> Value {
    let mut message = message.clone();
    if let Some(object) = message.as_object_mut() {
        object.remove("_mink");
        object.remove("internal");
    }
    message
}

fn wire_messages(messages: &[Value]) -> Vec<Value> {
    messages.iter().map(wire).collect()
}

#[async_trait::async_trait]
impl LlmBackend for GoalScriptBackend {
    fn name(&self) -> &str {
        "goal-script"
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        let independent = request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .next_back()
            .is_some_and(|s| s.starts_with("<independent-goal-review>"));
        let adjudication = mock_review_parts(&request).is_some();
        if !independent && !adjudication {
            self.calls.lock().unwrap().push(CapturedRequest {
                system_prompt: request.system_prompt.clone(),
                tools: request.tools.clone(),
                messages: request.messages.clone(),
            });
        }
        let events = if adjudication {
            let iteration = mock_review_parts(&request).map_or(1, |(c, _)| c.iteration);
            mock_goal_review(
                &request,
                if iteration < 2 {
                    "needs_work"
                } else {
                    "satisfied"
                },
                "",
            )
        } else if request
            .messages
            .last()
            .is_some_and(|m| m["_mink"]["source"] == "goal_continuation")
        {
            vec![
                Ok(LlmEvent::ToolCall(LlmToolCallEvent {
                    name: "GoalRead".into(),
                    id: "goal-read".into(),
                    input_json: serde_json::json!({}),
                    fields: Default::default(),
                    parse_error: None,
                    raw_arguments_digest: None,
                })),
                Ok(LlmEvent::Stop(LlmStopEvent {
                    reason: "tool_use".into(),
                })),
            ]
        } else {
            natural_goal_end()
        };
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

fn prefix_snapshots(events_path: &Path) -> Result<usize> {
    let text = std::fs::read_to_string(events_path)?;
    Ok(text
        .lines()
        .filter(|line| line.contains("\"prefix_snapshot\""))
        .count())
}

fn conversation_rows(path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path)?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(|error| anyhow!("bad row: {error}")))
        .collect()
}

fn temp_pair(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("mink-goal-prefix-{}-{name}", std::process::id()));
    let home = root.join("home");
    let cwd = root.join("cwd");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    (home, cwd)
}

/// Every round extends the request instead of rewriting it.
#[tokio::test]
async fn goal_rounds_extend_the_request_prefix_instead_of_rewriting_it() -> Result<()> {
    let (home, cwd) = temp_pair("extend");
    std::fs::write(cwd.join("proof.txt"), "objective evidence")?;
    let backend = Arc::new(GoalScriptBackend::new());
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await?;
    let handle = runtime.handle();

    // A turn before the goal proves that starting a goal appends rather than
    // inserting: the goal state may not appear in front of existing history.
    handle.run_turn("warm up").await?;
    handle.start_goal_driver_and_run()?;
    let mut spec = GoalSpec::new("ship the parser");
    spec.limits.max_turns = 2;
    let draft = handle.draft_goal(spec, 3600)?;
    let created = handle.confirm_goal(&draft.id, draft.revision)?;
    let mut limits = created.spec.limits.clone();
    limits.max_turns = 2;
    let _ = handle.update_goal_limits(&created.id, created.revision, limits);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .map_err(|_| anyhow!("goal run timed out"))??;
    assert_eq!(outcome.status, GoalStatus::Completed, "{outcome:?}");
    let events_path = runtime.session_info().events_path.clone();
    let conversation_path = runtime.session_info().conversation_path.clone();
    runtime.shutdown().await?;

    let requests = backend.captured();
    assert!(
        requests.len() >= 4,
        "two rounds must produce at least four requests: {}",
        requests.len()
    );

    // 1. The immutable prefix is built once and never drifts.
    let tools = serde_json::to_string(&requests[0].tools)?;
    assert!(
        tools.contains("\"GoalRead\""),
        "the goal tool is part of the static surface: {tools}"
    );
    for request in &requests {
        assert_eq!(
            request.system_prompt, requests[0].system_prompt,
            "system prompt changed mid-session"
        );
        assert_eq!(request.tools, requests[0].tools, "tool surface changed");
    }
    assert_eq!(
        prefix_snapshots(&events_path)?,
        1,
        "one prefix build for the whole session"
    );

    // 2. Every request is a strict extension of the previous one — across
    //    round boundaries included.
    let mut previous: Vec<Value> = Vec::new();
    for (index, request) in requests.iter().enumerate() {
        let current = wire_messages(&request.messages);
        assert!(
            current.len() >= previous.len(),
            "request {index} lost {} message(s)",
            previous.len() - current.len()
        );
        assert_eq!(
            current[..previous.len()],
            previous[..],
            "request {index} rewrote the prefix it already sent"
        );
        previous = current;
    }

    // 3. The per-round facts travel with the appended round prompt.
    let rows = conversation_rows(&conversation_path)?;
    let prompts: Vec<&Value> = rows
        .iter()
        .filter(|row| row["_mink"]["source"] == "goal_continuation")
        .collect();
    assert_eq!(prompts.len(), 2, "one round prompt per admitted round");
    for (round, prompt) in prompts.iter().enumerate() {
        let text = prompt["content"].as_str().unwrap_or_default();
        let expected = round + 1;
        assert!(
            text.contains(&format!("Round: {expected}/2")),
            "round {expected} prompt must state its position: {text}"
        );
        assert!(
            text.contains(&format!("Remaining turns: {}", 2 - expected)),
            "round {expected} prompt must state the remaining turns: {text}"
        );
        assert!(
            !text.contains("GoalReport"),
            "execution does not report completion: {text}"
        );
    }

    // 4. The goal state is never mounted in front of the history, and the
    //    terminal facts are appended once.
    assert!(
        rows.iter()
            .all(|row| row["_mink"]["goal_state_kind"] != "contract"),
        "no contract checkpoint may be mounted without a compaction"
    );
    let ended: Vec<&Value> = rows
        .iter()
        .filter(|row| row["_mink"]["goal_state_kind"] == "ended")
        .collect();
    assert_eq!(ended.len(), 1, "terminal facts are published exactly once");
    assert!(
        ended[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("Status: completed"),
        "{:?}",
        ended[0]["content"]
    );
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
    Ok(())
}

use mink::runtime::GoalReviewCandidate;

fn natural_goal_end() -> Vec<anyhow::Result<LlmEvent>> {
    vec![
        Ok(LlmEvent::Text(LlmTextEvent {
            content: "execution round ended".into(),
        })),
        Ok(LlmEvent::Stop(LlmStopEvent {
            reason: "end_turn".into(),
        })),
    ]
}

fn mock_review_parts(request: &LlmRequest) -> Option<(GoalReviewCandidate, GoalSpec)> {
    if !request.tools.iter().any(|t| t["name"] == "GoalReview") {
        return None;
    }
    let text = request
        .messages
        .iter()
        .rev()
        .filter_map(|m| m["content"].as_str())
        .find(|s| !s.starts_with("<goal-conclusion-required>"))?;
    if !text.starts_with("<goal-review>") {
        return None;
    }
    let candidate =
        serde_json::from_str(text.lines().find_map(|l| l.strip_prefix("Candidate: "))?).unwrap();
    let contract =
        serde_json::from_str(text.lines().find_map(|l| l.strip_prefix("Contract: "))?).unwrap();
    Some((candidate, contract))
}

fn mock_goal_review(
    request: &LlmRequest,
    verdict: &str,
    dependency: &str,
) -> Vec<anyhow::Result<LlmEvent>> {
    let Some((candidate, spec)) = mock_review_parts(request) else {
        return natural_goal_end();
    };
    let has_result = request
        .messages
        .last()
        .and_then(|m| m["content"].as_array())
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|b| b["type"] == "tool_result" && b["_mink"]["tool_name"] == "GoalReview")
        });
    if has_result {
        return natural_goal_end();
    }
    let pass = verdict == "satisfied";
    let review = serde_json::json!({
        "candidate":candidate,"verdict":verdict,"original_request":{"status":if pass {"pass"} else {"unknown"},"reason":"checked the whole original request","evidence":if pass {vec!["proof.txt"]} else {vec![]} },"summary":"independent criterion review","next_step":if pass { "" } else { "continue with the unmet criterion" },
        "criteria":spec.criteria.iter().map(|c| serde_json::json!({"criterion_id":c.id,"status":if pass {"pass"} else {"unknown"},"reason":"checked available evidence","evidence":if pass {vec!["proof.txt"]} else {vec![]} })).collect::<Vec<_>>(),
        "constraints":spec.constraints.iter().map(|c| serde_json::json!({"constraint":c,"status":if pass {"pass"} else {"unknown"},"reason":"checked constraints","evidence":[]})).collect::<Vec<_>>(),
        "blocker":if verdict == "blocked" {serde_json::json!({"category":"external-dependency","criterion_ids":spec.criteria.iter().map(|c|c.id.clone()).collect::<Vec<_>>(),"dependency":dependency,"evidence":[]})} else {serde_json::Value::Null}
    });
    let mut events = Vec::new();
    if pass {
        events.push(Ok(LlmEvent::ToolCall(LlmToolCallEvent {
            id: "review-read".into(),
            name: "Read".into(),
            input_json: serde_json::json!({"path":"proof.txt"}),
            fields: Default::default(),
            parse_error: None,
            raw_arguments_digest: None,
        })));
    }
    events.push(Ok(LlmEvent::ToolCall(LlmToolCallEvent {
        id: "review-submit".into(),
        name: "GoalReview".into(),
        input_json: review,
        fields: Default::default(),
        parse_error: None,
        raw_arguments_digest: None,
    })));
    events.push(Ok(LlmEvent::Stop(LlmStopEvent {
        reason: "tool_use".into(),
    })));
    events
}
