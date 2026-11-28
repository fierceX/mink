use super::*;
use mink::runtime::{
    AgentEvent, AgentOptions, AgentRuntime, AgentTool, EventSink, LlmBackend, LlmEvent, LlmRequest,
    LlmResponseStream, LlmStopEvent, LlmTextEvent, LlmToolCallEvent, RuntimeResult, ToolDefinition,
    ToolError, ToolExecutionContext, ToolExecutionMode, ToolOutput,
};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_DIR_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn unique_temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sequence = TEMP_DIR_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mink-goal-test-{name}-{}-{nanos}-{sequence}",
        std::process::id()
    ))
}

pub(super) fn tool_call_event(name: &str, id: &str, input: serde_json::Value) -> LlmToolCallEvent {
    let fields: std::collections::BTreeMap<String, String> = input
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(key, value)| (key.clone(), value.to_string()))
                .collect()
        })
        .unwrap_or_default();
    LlmToolCallEvent {
        name: name.into(),
        id: id.into(),
        input_json: input,
        fields,
        parse_error: None,
        raw_arguments_digest: None,
    }
}

pub(super) struct ScriptedMockLlmBackend {
    pub(super) model: String,
    pub(super) responses: Mutex<std::collections::VecDeque<Vec<anyhow::Result<LlmEvent>>>>,
}

impl ScriptedMockLlmBackend {
    pub(super) fn new(
        model: impl Into<String>,
        responses: Vec<Vec<anyhow::Result<LlmEvent>>>,
    ) -> Self {
        Self {
            model: model.into(),
            responses: Mutex::new(responses.into()),
        }
    }
}

#[async_trait]
impl LlmBackend for ScriptedMockLlmBackend {
    fn name(&self) -> &str {
        &self.model
    }

    async fn stream(&self, _request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        let events = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(natural_goal_end);
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

pub(super) struct PanicTool(pub(super) tokio::sync::Notify);

#[async_trait]
impl AgentTool for PanicTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = ToolDefinition::new(
            "PanicTool",
            "Always panics (test fixture)",
            serde_json::json!({
                "type": "object",
                "properties": {"release":{"type":"boolean"},"mark":{"type":"boolean"},"stall":{"type":"boolean"}},
                "additionalProperties": false
            }),
        );
        definition.execution = ToolExecutionMode::ParallelReadOnly;
        definition
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        if input["release"] == true {
            self.0.notify_one();
            return Ok(ToolOutput::text("later sibling finished"));
        }
        if input["mark"] == true {
            std::fs::write(ctx.cwd().join("attempted"), "once").unwrap();
            if input["stall"] == true {
                return futures::future::pending().await;
            }
        } else {
            self.0.notified().await;
        }
        panic!("panic tool fixture");
    }
}

#[derive(Default)]
pub(super) struct PendingEstablishBackend {
    pub(super) calls: Mutex<u32>,
    pub(super) entered: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl LlmBackend for PendingEstablishBackend {
    fn name(&self) -> &str {
        "pending-establish"
    }

    async fn stream(&self, _request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        self.entered.notify_one();
        let call = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        if call == 1 {
            futures::future::pending::<()>().await;
        }
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(LlmEvent::Text(LlmTextEvent {
                    content: "after establishment interrupt".into(),
                })),
                Ok(LlmEvent::Stop(LlmStopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

pub(super) struct GoalReportBackend {
    pub(super) calls: std::sync::atomic::AtomicU64,
    pub(super) continue_first: bool,
}

#[async_trait]
impl LlmBackend for GoalReportBackend {
    fn name(&self) -> &str {
        "goal-reporter"
    }
    async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let verdict = if self.continue_first
            && mock_review_parts(&request).is_some_and(|(c, _)| c.iteration == 1)
        {
            "needs_work"
        } else {
            "satisfied"
        };
        let events = mock_goal_review(&request, verdict, "");
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

pub(super) async fn start_with_goal(
    mut options: AgentOptions,
    plugin: GoalPlugin,
) -> RuntimeResult<(AgentRuntime, Arc<GoalEngine>)> {
    plugin.install_into(&mut options);
    let runtime = AgentRuntime::start(options).await?;
    let goals = GoalEngine::attach(runtime.handle(), plugin)?;
    Ok((runtime, goals))
}

pub(super) async fn goal_runtime(
    name: &str,
    continue_first: bool,
    verifier: Option<Arc<dyn GoalVerifier>>,
) -> (
    AgentRuntime,
    Arc<GoalEngine>,
    PathBuf,
    PathBuf,
    Arc<GoalReportBackend>,
) {
    let home = unique_temp_dir(&format!("{name}-home"));
    let cwd = unique_temp_dir(&format!("{name}-cwd"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first,
    });
    let mut plugin = GoalPlugin::new();
    if let Some(verifier) = verifier {
        plugin = plugin.with_verifier(verifier);
    }
    let options = AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone());
    let (runtime, goals) = start_with_goal(options, plugin).await.unwrap();
    (runtime, goals, home, cwd, backend)
}

pub(super) struct AlwaysBlockedBackend {
    pub(super) calls: std::sync::atomic::AtomicU64,
}

#[async_trait]
impl LlmBackend for AlwaysBlockedBackend {
    fn name(&self) -> &str {
        "always-blocked"
    }
    async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let events = mock_goal_review(&request, "blocked", "waiting on human prerequisite");
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

pub(super) struct ScriptedReportBackend {
    pub(super) calls: std::sync::atomic::AtomicU64,
    pub(super) script: Vec<(&'static str, String, String)>,
}

#[async_trait]
impl LlmBackend for ScriptedReportBackend {
    fn name(&self) -> &str {
        "scripted-goal-reports"
    }
    async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let index = mock_review_parts(&request)
            .map_or(0, |(c, _)| c.iteration.saturating_sub(1) as usize)
            .min(self.script.len() - 1);
        let (kind, _summary, dependency) = &self.script[index];
        let verdict = match *kind {
            "complete" => "satisfied",
            "blocked" => "blocked",
            _ => "needs_work",
        };
        let events = mock_goal_review(&request, verdict, dependency);
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

pub(super) struct RetryableReviewBackend {
    pub(super) review_calls: std::sync::atomic::AtomicU64,
    pub(super) execution_calls: std::sync::atomic::AtomicU64,
}

#[async_trait]
impl LlmBackend for RetryableReviewBackend {
    fn name(&self) -> &str {
        "retryable-goal-review"
    }
    async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        let independent = request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .next_back()
            .is_some_and(|s| s.starts_with("<independent-goal-review>"));
        let events = if independent {
            if self
                .review_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                anyhow::bail!("independent review backend unavailable")
            }
            natural_goal_end()
        } else if mock_review_parts(&request).is_some() {
            mock_goal_review(&request, "satisfied", "")
        } else {
            self.execution_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            natural_goal_end()
        };
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

pub(super) struct HostSubmissionRecoveryBackend {
    pub(super) review_calls: std::sync::atomic::AtomicU64,
    pub(super) execution_calls: std::sync::atomic::AtomicU64,
    pub(super) recover_after_rejections: bool,
    pub(super) end_without_conclusion: bool,
}

#[async_trait]
impl LlmBackend for HostSubmissionRecoveryBackend {
    fn name(&self) -> &str {
        "format-recovering-goal-review"
    }
    async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        let events = if mock_review_parts(&request).is_some() {
            let call = self
                .review_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let continued = request
                .messages
                .iter()
                .filter_map(|m| m["content"].as_str())
                .any(|s| s.starts_with("<goal-conclusion-required>"))
                || call >= 4;
            if continued && self.recover_after_rejections {
                mock_goal_review(&request, "satisfied", "")
            } else if call == 0 && self.end_without_conclusion {
                vec![
                    Ok(LlmEvent::Text(LlmTextEvent {
                        content: "Some evidence paths were only found with Glob; I will inspect them with Read.".into(),
                    })),
                    Ok(LlmEvent::Stop(LlmStopEvent {
                        reason: "end_turn".into(),
                    })),
                ]
            } else {
                let mut events = mock_goal_review(&request, "satisfied", "");
                events.retain(|event| {
                    !matches!(event, Ok(LlmEvent::ToolCall(tool_call)) if tool_call.name == "Read")
                });
                for event in &mut events {
                    if let Ok(LlmEvent::ToolCall(tool_call)) = event
                        && tool_call.name == "GoalReview"
                    {
                        tool_call.id = format!("invalid-review-{call}");
                        if !self.recover_after_rejections {
                            tool_call.parse_error = Some("invalid host tool-call JSON".into());
                        }
                    }
                }
                events
            }
        } else if request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .next_back()
            .is_some_and(|s| s.starts_with("<independent-goal-review>"))
        {
            natural_goal_end()
        } else {
            self.execution_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            natural_goal_end()
        };
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

pub(super) struct TestGoalVerifier {
    pub(super) status: GoalVerificationStatus,
    pub(super) mutate: bool,
}

#[async_trait]
impl GoalVerifier for TestGoalVerifier {
    fn id(&self) -> &str {
        "test-check-v1"
    }
    async fn verify(&self, context: GoalVerificationContext) -> anyhow::Result<GoalVerification> {
        if self.mutate {
            std::fs::write(
                context.cwd.join("proof.txt"),
                format!("changed {}", context.goal.revision),
            )?;
        }
        Ok(GoalVerification {
            status: self.status.clone(),
            reason: "check result".into(),
            evidence: vec!["proof.txt".into()],
        })
    }
}

pub(super) struct CountingLease(pub(super) Arc<std::sync::atomic::AtomicUsize>);
impl CountingLease {
    pub(super) fn new(alive: &Arc<std::sync::atomic::AtomicUsize>) -> Self {
        alive.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(alive.clone())
    }
}
impl GoalRunLease for CountingLease {}
impl Drop for CountingLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(super) struct FailingSink;

#[async_trait]
impl EventSink for FailingSink {
    async fn on_event(&self, _event: AgentEvent) -> Result<(), String> {
        Err("injected sink failure".into())
    }
}

pub(super) struct HeldGoalVerifier {
    pub(super) entered: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}

#[async_trait]
impl GoalVerifier for HeldGoalVerifier {
    fn id(&self) -> &str {
        "held-check-v1"
    }
    async fn verify(&self, _context: GoalVerificationContext) -> anyhow::Result<GoalVerification> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(GoalVerification {
            status: GoalVerificationStatus::Pass,
            reason: "released".into(),
            evidence: vec![],
        })
    }
}

pub(super) struct Audit3FailingSink;

#[async_trait]
impl EventSink for Audit3FailingSink {
    async fn on_event(&self, _: AgentEvent) -> Result<(), String> {
        Err("injected sink error".into())
    }
}

pub(super) struct Audit3HoldFinalSink {
    pub(super) entered: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
    pub(super) held: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl EventSink for Audit3HoldFinalSink {
    async fn on_event(&self, event: AgentEvent) -> Result<(), String> {
        if serde_json::to_value(event).unwrap()["type"] == "final"
            && !self.held.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}

pub(super) struct Audit4HoldFinalErrorSink {
    pub(super) entered: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
    pub(super) held: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl EventSink for Audit4HoldFinalErrorSink {
    async fn on_event(&self, event: AgentEvent) -> Result<(), String> {
        if serde_json::to_value(event).unwrap()["type"] == "final"
            && !self.held.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.entered.notify_one();
            self.release.notified().await;
            return Err("old sink failed".into());
        }
        Ok(())
    }
}

pub(super) fn confirmed_test_goal(
    goals: &GoalEngine,
    mut spec: GoalSpec,
) -> RuntimeResult<GoalSnapshot> {
    spec.criteria = spec
        .acceptance_criteria
        .iter()
        .enumerate()
        .map(|(i, outcome)| GoalCriterion {
            id: format!("criterion-{}", i + 1),
            subgoal_id: "subgoal-1".into(),
            outcome: outcome.clone(),
            required_evidence: "Concrete proof".into(),
        })
        .collect();
    let duration = spec
        .limits
        .deadline_unix_secs
        .saturating_sub(goal_now())
        .max(1);
    let draft = goals.draft(spec, duration)?;
    goals.confirm(&draft.id, draft.revision)
}

pub(super) fn confirmed_test_goal_with_lease(
    goals: &GoalEngine,
    mut spec: GoalSpec,
    lease: Box<dyn GoalRunLease>,
) -> RuntimeResult<GoalSnapshot> {
    spec.criteria = spec
        .acceptance_criteria
        .iter()
        .enumerate()
        .map(|(i, outcome)| GoalCriterion {
            id: format!("criterion-{}", i + 1),
            subgoal_id: "subgoal-1".into(),
            outcome: outcome.clone(),
            required_evidence: "Concrete proof".into(),
        })
        .collect();
    let duration = spec
        .limits
        .deadline_unix_secs
        .saturating_sub(goal_now())
        .max(1);
    let draft = goals.draft(spec, duration)?;
    goals.confirm_and_start(&draft.id, draft.revision, lease)
}

pub(super) fn natural_goal_end() -> Vec<anyhow::Result<LlmEvent>> {
    vec![
        Ok(LlmEvent::Text(LlmTextEvent {
            content: "execution round ended".into(),
        })),
        Ok(LlmEvent::Stop(LlmStopEvent {
            reason: "end_turn".into(),
        })),
    ]
}

pub(super) fn mock_review_parts(request: &LlmRequest) -> Option<(GoalReviewCandidate, GoalSpec)> {
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

pub(super) fn mock_goal_review(
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
            blocks.iter().any(|block| {
                block["type"] == "tool_result"
                    && block["_mink"]["tool_name"] == "GoalReview"
                    && block["content"].as_str() == Some("Host goal conclusion recorded.")
            })
        });
    if has_result {
        return natural_goal_end();
    }
    let pass = verdict == "satisfied";
    let review = serde_json::json!({
        "candidate":candidate,"verdict":verdict,"original_request":{"status":if pass {"pass"} else {"unknown"},"reason":"checked the whole original request","evidence":if pass {vec!["proof.txt"]} else {vec![]} },"summary":"independent criterion review","next_step":if pass { "" } else { "continue with the unmet criterion" },
        "criteria":spec.criteria.iter().map(|c| serde_json::json!({"criterion_id":c.id,"status":if pass {"pass"} else {"unknown"},"reason":"checked available evidence","evidence":if pass {vec!["proof.txt"]} else {vec![]} })).collect::<Vec<_>>(),
        "constraints":spec.constraints.iter().map(|c| serde_json::json!({"constraint":c,"status":if pass {"pass"} else {"unknown"},"reason":"checked constraints","evidence":if pass {vec!["proof.txt"]} else {vec![]} })).collect::<Vec<_>>(),
        "blocker":if verdict == "blocked" {serde_json::json!({"category":"external-dependency","criterion_ids":spec.criteria.iter().map(|c|c.id.clone()).collect::<Vec<_>>(),"dependency":dependency,"evidence":["proof.txt"]})} else {serde_json::Value::Null}
    });
    let mut events = Vec::new();
    if pass || verdict == "blocked" {
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
