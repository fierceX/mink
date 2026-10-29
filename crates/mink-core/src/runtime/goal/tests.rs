use super::*;
use crate::runtime::{AgentOptions, AgentRuntime, LlmBackend, LlmRequest, LlmResponseStream};
use std::sync::atomic::AtomicUsize;

struct NoRequest;

/// A prose-only independent reviewer, followed by a host that corrects four
/// rejected conclusions. The rejections must not exhaust the format window.
struct HostConclusionBackend {
    reviewer_requests: AtomicUsize,
    host_requests: AtomicUsize,
}

#[async_trait]
impl LlmBackend for HostConclusionBackend {
    fn name(&self) -> &str {
        "host-goal-conclusion-regression"
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        use crate::runtime::{LlmEvent, LlmStopEvent, LlmTextEvent};
        let prompt = request
            .messages
            .iter()
            .rev()
            .filter_map(|m| m["content"].as_str())
            .find(|s| !s.starts_with("<goal-conclusion-required>"))
            .unwrap_or_default();
        let tool = |id: &str, name: &str, input_json| {
            Ok(LlmEvent::ToolCall(crate::protocol::ToolCallEvent {
                id: id.into(),
                name: name.into(),
                input_json,
                fields: Default::default(),
                parse_error: None,
                raw_arguments_digest: None,
            }))
        };
        let events = if prompt.starts_with("<independent-goal-review>") {
            assert!(request.tools.iter().all(|t| matches!(
                t["name"].as_str(),
                Some("Read" | "Glob" | "Grep" | "GoalRead")
            )));
            assert!(!request.tools.iter().any(|t| t["name"] == "GoalReview"));
            if self.reviewer_requests.fetch_add(1, Ordering::SeqCst) == 0 {
                vec![
                    tool(
                        "independent-read",
                        "Read",
                        serde_json::json!({"path":"proof.txt"}),
                    ),
                    Ok(LlmEvent::Stop(LlmStopEvent {
                        reason: "tool_use".into(),
                    })),
                ]
            } else {
                vec![Ok(LlmEvent::Text(LlmTextEvent { content: "I inspected proof.txt with Read. The original request and criterion-1 are satisfied by the delivered proof. No unmet requirements or external blockers were observed.".into() })),
                    Ok(LlmEvent::Stop(LlmStopEvent { reason: "end_turn".into() }))]
            }
        } else if prompt.starts_with("<goal-review>") {
            assert!(request.tools.iter().any(|t| t["name"] == "GoalReview"));
            let candidate: GoalReviewCandidate = serde_json::from_str(
                prompt
                    .lines()
                    .find_map(|l| l.strip_prefix("Candidate: "))
                    .unwrap(),
            )?;
            let attempt = self.host_requests.fetch_add(1, Ordering::SeqCst);
            if attempt > 0 {
                assert!(
                    request
                        .messages
                        .last()
                        .and_then(|m| m["content"].as_array())
                        .is_some_and(|blocks| blocks.iter().any(|b| b["content"]
                            .as_str()
                            .is_some_and(|s| s.contains(
                                "GoalReview rejected: review must cover each criterion exactly once"
                            ))))
                );
            }
            vec![
                tool(
                    &format!("host-submit-{attempt}"),
                    "GoalReview",
                    serde_json::json!({
                        "candidate":candidate, "verdict":"satisfied", "summary":"confirmed from independent observations",
                        "original_request":{"status":"pass","reason":"independent report confirms the original request","evidence":["proof.txt"]},
                        "criteria":if attempt < 4 { vec![] } else { vec![serde_json::json!({"criterion_id":"criterion-1","status":"pass","reason":"independently inspected proof","evidence":["proof.txt"]})] },
                        "constraints":[], "next_step":"", "blocker":null
                    }),
                ),
                Ok(LlmEvent::Stop(LlmStopEvent {
                    reason: "tool_use".into(),
                })),
            ]
        } else {
            vec![
                Ok(LlmEvent::Text(LlmTextEvent {
                    content: "execution ended naturally".into(),
                })),
                Ok(LlmEvent::Stop(LlmStopEvent {
                    reason: "end_turn".into(),
                })),
            ]
        };
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn host_agent_retries_rejected_conclusions_after_prose_only_independent_review() {
    let root = std::env::temp_dir().join(format!(
        "mink-host-conclusion-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "the requested deliverable").unwrap();
    let backend = Arc::new(HostConclusionBackend {
        reviewer_requests: AtomicUsize::new(0),
        host_requests: AtomicUsize::new(0),
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(root.join("home"), &cwd).with_llm_backend(backend.clone()),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let draft = handle
        .draft_goal(GoalSpec::new("deliver proof"), 3600)
        .unwrap();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    let history = handle.goal_ctx.store.lines().await.unwrap();
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(completed.status, GoalStatus::Completed, "{completed:?}");
    assert_eq!(completed.iterations.len(), 1);
    assert_eq!(backend.reviewer_requests.load(Ordering::SeqCst), 2);
    assert_eq!(backend.host_requests.load(Ordering::SeqCst), 5);
    assert_eq!(
        completed.usage.request_count, 8,
        "execution, reviewer and all host retries share the goal bill"
    );
    assert_eq!(
        history
            .iter()
            .filter(|m| m["_mink"]["source"] == "goal_adjudication")
            .count(),
        1
    );
    assert_eq!(
        history
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .filter(|b| b["type"] == "tool_use" && b["name"] == "GoalReview")
            .count(),
        5
    );
    assert!(completed.review_failure_reason.is_none());
}
#[async_trait]
impl LlmBackend for NoRequest {
    fn name(&self) -> &str {
        "goal-cached-review-regression"
    }
    async fn stream(&self, _: LlmRequest) -> Result<LlmResponseStream> {
        anyhow::bail!("a settled review must not request the provider again")
    }
}

async fn blocked_review_fixture() -> (AgentRuntime, AgentRuntimeHandle, PathBuf, GoalSnapshot) {
    let root = std::env::temp_dir().join(format!(
        "mink-review-replay-{}-{:?}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
        std::thread::current().id()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "external input has not arrived\n").unwrap();
    let runtime = AgentRuntime::start(
        AgentOptions::new(root.join("home"), &cwd).with_llm_backend(Arc::new(NoRequest)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let draft = handle
        .draft_goal(GoalSpec::new("obtain external input"), 3600)
        .unwrap();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    let executing = handle
        .goal_ctx
        .goal_store
        .begin_iteration("one-execution", 0)
        .unwrap();
    let candidate = GoalReviewCandidate {
        generation: 0,
        goal_id: executing.id.clone(),
        contract_revision: executing.contract_revision,
        iteration: 1,
        conversation_seq: 0,
        reviewer_id: format!(
            "goal-review:{}:{}",
            handle.goal_ctx.active_model.lock().unwrap().actual,
            handle.goal_ctx.tool_surface.fingerprint()
        ),
    };
    let review: GoalReview = serde_json::from_value(serde_json::json!({
        "candidate":candidate,"verdict":"blocked",
        "original_request":{"status":"unknown","reason":"waiting","evidence":[]},
        "criteria":[{"criterion_id":"criterion-1","status":"unknown","reason":"waiting","evidence":[]}],
        "constraints":[],"summary":"waiting","next_step":"obtain external input",
        "blocker":{"category":"external","criterion_ids":["criterion-1"],"dependency":"missing input","evidence":["proof.txt"]}
    })).unwrap();
    let hash = handle.evidence_version("proof.txt").unwrap();
    let fingerprint = hash_bytes(
        &serde_json::to_vec(&(
            "external",
            vec!["criterion-1"],
            "missing input",
            vec![("proof.txt", &hash)],
        ))
        .unwrap(),
    );
    let goal = handle
        .goal_ctx
        .goal_store
        .change(&executing.id, executing.revision, |g| {
            g.iterations[0].finished = true;
            g.iterations[0].outcome_status = Some(crate::runtime::TurnStatus::Ok);
            g.phase = GoalPhase::Idle;
            g.review_candidate = Some(candidate);
            g.reviews.push(review);
            g.evidence_versions = vec![(
                cwd.canonicalize()
                    .unwrap()
                    .join("proof.txt")
                    .to_string_lossy()
                    .into_owned(),
                hash,
            )];
            g.blocked_rounds = 1;
            g.blocker_rounds = 1;
            g.blocker_fingerprint = Some(fingerprint);
            Ok(())
        })
        .unwrap();
    {
        let mut control = handle.goal_driver.lock_control();
        control.enabled = true;
        handle.goal_driver.set_driver_locked(
            &mut control,
            DriverState::Running {
                run_id: 1,
                goal_id: goal.id.clone(),
            },
        );
    }
    (runtime, handle, root, goal)
}

async fn close_fixture(runtime: AgentRuntime, handle: &AgentRuntimeHandle, root: PathBuf) {
    {
        let mut control = handle.goal_driver.lock_control();
        handle
            .goal_driver
            .set_driver_locked(&mut control, DriverState::Idle);
    }
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn settled_review_recovery_preserves_next_execution_phase() {
    let (runtime, handle, root, goal) = blocked_review_fixture().await;
    handle
        .goal_ctx
        .goal_store
        .recover(&goal.environment, &[], 0)
        .unwrap();
    let recovered = handle.goal_snapshot().unwrap();
    close_fixture(runtime, &handle, root).await;
    assert_eq!(
        recovered.phase,
        GoalPhase::Idle,
        "the review and next-step decision were already durable"
    );
    assert_eq!(recovered.blocker_rounds, 1);
}

#[tokio::test]
async fn external_blocker_requires_inspected_evidence() {
    let (runtime, handle, root, goal) = blocked_review_fixture().await;
    let session = super::review::GoalReviewSession::new(
        goal.clone(),
        handle.goal_ctx.goal_store.clone(),
        handle.goal_ctx.usage.clone(),
    );
    let ctx = crate::context::ToolContext::from(handle.goal_ctx.as_ref());
    let evidence_ctx = ctx.goal_review_tool_context().unwrap();
    let call = crate::protocol::ToolCallEvent {
        id: "proof-read".into(),
        name: "Read".into(),
        input_json: serde_json::json!({"path":"proof.txt"}),
        fields: Default::default(),
        parse_error: None,
        raw_arguments_digest: None,
    };
    session
        .finish_read(session.capture_read(&call, &evidence_ctx).unwrap().unwrap())
        .unwrap();
    let mut claimed = goal.reviews[0].clone();
    claimed.blocker.as_mut().unwrap().evidence.clear();
    let accepted = session.submit(claimed, &evidence_ctx);
    close_fixture(runtime, &handle, root).await;
    assert!(
        accepted.is_err(),
        "an unsupported external dependency was accepted"
    );
}

#[tokio::test]
async fn host_conclusion_tool_is_scoped_and_rejections_preserve_goal_state() {
    use crate::tools::runner::ToolRunner;
    let (runtime, handle, root, goal) = blocked_review_fixture().await;
    let session = Arc::new(super::review::GoalReviewSession::new(
        goal.clone(),
        handle.goal_ctx.goal_store.clone(),
        handle.goal_ctx.usage.clone(),
    ));
    let mut ctx = crate::context::ToolContext::from(handle.goal_ctx.as_ref());
    let call = |id: &str, name: &str, input_json| crate::protocol::ToolCallEvent {
        id: id.into(),
        name: name.into(),
        input_json,
        fields: Default::default(),
        parse_error: None,
        raw_arguments_digest: None,
    };
    let conclusion = serde_json::to_value(&goal.reviews[0]).unwrap();
    let ordinary = ToolRunner::new(Arc::new(ctx.clone()))
        .execute_all(vec![call(
            "ordinary-submit",
            "GoalReview",
            conclusion.clone(),
        )])
        .await
        .unwrap();
    assert!(!ordinary[0].succeeded());
    assert!(
        ordinary[0]
            .content
            .contains("host agent's adjudication turn")
    );
    ctx.goal_adjudication = Some(session.clone());
    let runner = ToolRunner::new(Arc::new(ctx));
    let forbidden = runner
        .execute_all(vec![call(
            "forbidden-write",
            "Write",
            serde_json::json!({"path":"forbidden.txt","content":"must not execute"}),
        )])
        .await
        .unwrap();
    assert!(!forbidden[0].succeeded());
    assert!(!handle.goal_ctx.cwd.join("forbidden.txt").exists());
    let mut invalid = conclusion;
    invalid["criteria"] = serde_json::json!([]);
    let rejected = runner
        .execute_all(vec![call("host-invalid", "GoalReview", invalid)])
        .await
        .unwrap();
    assert!(!rejected[0].succeeded());
    assert!(
        rejected[0]
            .content
            .contains("review must cover each criterion exactly once")
    );
    assert_eq!(
        rejected[0].failure_source, None,
        "a conclusion rejection is not a damaged model format"
    );
    assert!(session.submission.lock().unwrap().is_none());
    assert_eq!(handle.goal_snapshot().unwrap(), goal);
    close_fixture(runtime, &handle, root).await;
}

#[tokio::test]
async fn goal_confirmation_rejects_a_missing_host_submission_tool_before_activation() {
    let root = std::env::temp_dir().join(format!(
        "mink-host-tool-required-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let options = AgentOptions::new(root.join("home"), &root)
        .with_llm_backend(Arc::new(NoRequest))
        .with_enabled_tools(vec!["GoalRead".into()]);
    let runtime = AgentRuntime::start(options).await.unwrap();
    let handle = runtime.handle();
    let draft = handle
        .draft_goal(GoalSpec::new("requires a host conclusion"), 3600)
        .unwrap();
    let refused = handle.confirm_goal(&draft.id, draft.revision);
    assert!(
        matches!(refused,Err(RuntimeError::Configuration(ref error)) if error.contains("GoalReview"))
    );
    assert_eq!(handle.goal_snapshot().unwrap(), draft);
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn host_conclusion_verdict_must_match_requirement_findings() {
    let (runtime, handle, root, goal) = blocked_review_fixture().await;
    let mut feedback = goal.reviews[0].feedback();

    let blocked = feedback.clone().into_review(&goal).unwrap();
    assert_eq!(blocked.verdict, GoalReviewVerdict::Blocked);

    feedback.blocker = None;
    feedback.original_request.status = GoalCriterionStatus::Unknown;
    feedback.original_request.evidence.clear();
    feedback.criteria[0].status = GoalCriterionStatus::Fail;
    feedback.criteria[0].evidence.clear();
    feedback.next_step = "obtain the missing external input".into();
    let needs_work = feedback.clone().into_review(&goal).unwrap();
    assert_eq!(needs_work.verdict, GoalReviewVerdict::NeedsWork);

    feedback.original_request.status = GoalCriterionStatus::Pass;
    feedback.original_request.evidence = vec!["proof.txt".into()];
    feedback.criteria[0].status = GoalCriterionStatus::Pass;
    feedback.criteria[0].evidence = vec!["proof.txt".into()];
    feedback.next_step.clear();
    let satisfied = feedback.clone().into_review(&goal).unwrap();
    assert_eq!(satisfied.verdict, GoalReviewVerdict::Satisfied);

    let mut unsupported_host_conclusion = satisfied;
    unsupported_host_conclusion.criteria[0].status = GoalCriterionStatus::Fail;
    assert!(unsupported_host_conclusion.validate(&goal).is_err());

    close_fixture(runtime, &handle, root).await;
}

#[tokio::test]
async fn review_token_limit_stops_new_provider_requests() {
    let (runtime, handle, root, goal) = blocked_review_fixture().await;
    let verifying = handle
        .goal_ctx
        .goal_store
        .change(&goal.id, goal.revision, |g| {
            g.reviews.clear();
            g.phase = GoalPhase::Verifying;
            g.spec.limits.soft_token_limit = Some(10);
            g.usage.tokens.input_tokens = 10;
            Ok(())
        })
        .unwrap();
    handle.accept_goal(1, verifying).await.unwrap();
    let after = handle.goal_snapshot().unwrap();
    let requests = handle.goal_ctx.usage.all_records().unwrap().len();
    close_fixture(runtime, &handle, root).await;
    assert_eq!(after.status, GoalStatus::BudgetExhausted);
    assert_eq!(requests, 0, "review requests share the goal token budget");
}

struct ReviewBudgetBackend {
    review_requests: Arc<AtomicUsize>,
}

#[async_trait]
impl LlmBackend for ReviewBudgetBackend {
    fn name(&self) -> &str {
        "goal-review-budget-admission"
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        let is_review = request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .next_back()
            .is_some_and(|s| s.starts_with("<independent-goal-review>"));
        let input_tokens = if is_review {
            self.review_requests.fetch_add(1, Ordering::SeqCst);
            20
        } else {
            1
        };
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(crate::runtime::LlmEvent::Text(
                    crate::runtime::LlmTextEvent {
                        content: "natural end without structured review feedback".into(),
                    },
                )),
                Ok(crate::runtime::LlmEvent::Usage(
                    crate::runtime::LlmUsageEvent {
                        input_tokens,
                        output_tokens: 0,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                    },
                )),
                Ok(crate::runtime::LlmEvent::Stop(
                    crate::runtime::LlmStopEvent {
                        reason: "end_turn".into(),
                    },
                )),
            ])),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn host_adjudication_is_not_admitted_after_review_spends_soft_token_limit() {
    let root = std::env::temp_dir().join(format!(
        "mink-review-budget-admission-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let review_requests = Arc::new(AtomicUsize::new(0));
    let runtime = AgentRuntime::start(AgentOptions::new(root.join("home"), &cwd).with_llm_backend(
        Arc::new(ReviewBudgetBackend {
            review_requests: review_requests.clone(),
        }),
    ))
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let mut spec = GoalSpec::new("stop reviewer continuations at the token soft limit");
    spec.limits.soft_token_limit = Some(10);
    let draft = handle.draft_goal(spec, 3600).unwrap();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    let after = handle.goal_snapshot().unwrap();
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();

    assert_eq!(review_requests.load(Ordering::SeqCst), 1);
    assert_eq!(after.status, GoalStatus::BudgetExhausted);
    assert!(
        after.review_candidate.is_some(),
        "the candidate stays available for explicit recovery"
    );
    assert!(
        after
            .review_failure_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("soft limit")),
        "unexpected review failure reason: {:?}",
        after.review_failure_reason
    );
    assert_eq!(
        after.iterations.len(),
        1,
        "audit exhaustion does not admit another execution turn"
    );
}

#[tokio::test]
async fn cached_review_replay_does_not_count_another_execution_round() {
    let (runtime, handle, root, _) = blocked_review_fixture().await;
    let mut counts = vec![1];
    for _ in 0..2 {
        let current = handle.goal_snapshot().unwrap();
        let verifying = handle
            .goal_ctx
            .goal_store
            .change(&current.id, current.revision, |g| {
                g.phase = GoalPhase::Verifying;
                Ok(())
            })
            .unwrap();
        handle.accept_goal(1, verifying).await.unwrap();
        counts.push(handle.goal_snapshot().unwrap().blocker_rounds);
    }
    let after = handle.goal_snapshot().unwrap();
    close_fixture(runtime, &handle, root).await;
    assert_eq!(counts, vec![1, 1, 1]);
    assert_eq!(after.iterations.len(), 1);
    assert_eq!(after.reviews.len(), 1);
    assert_eq!(after.status, GoalStatus::Active);
}

struct LightweightReviewBackend;
#[async_trait]
impl LlmBackend for LightweightReviewBackend {
    fn name(&self) -> &str {
        "lightweight-review"
    }
    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        use crate::runtime::{LlmEvent, LlmStopEvent, LlmTextEvent};
        let prompt = request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .rfind(|s| !s.starts_with("<goal-conclusion-required>"))
            .unwrap_or("");
        let mut events = Vec::new();
        if prompt.starts_with("<goal-review>") {
            let candidate: serde_json::Value = serde_json::from_str(
                prompt
                    .lines()
                    .find_map(|l| l.strip_prefix("Candidate: "))
                    .unwrap(),
            )?;
            events.push(Ok(LlmEvent::ToolCall(crate::protocol::ToolCallEvent {
                id: "continue-submission".into(), name: "GoalReview".into(),
                input_json: serde_json::json!({"mode":"continue","candidate":candidate,"summary":"The required deliverable is still incomplete.","remaining_requirements":["criterion-1"],"next_step":"Implement the remaining deliverable"}),
                fields: Default::default(), parse_error: None, raw_arguments_digest: None,
            })));
            events.push(Ok(LlmEvent::Stop(LlmStopEvent {
                reason: "tool_use".into(),
            })));
        } else {
            events.push(Ok(LlmEvent::Text(LlmTextEvent {content: if prompt.starts_with("<independent-goal-review>") {
                "The host explicitly leaves criterion-1 unfinished and gives a concrete next step. No complete verification was performed.".into()
            } else { "Only part of criterion-1 is implemented. Next implement the remaining deliverable.".into() }})));
            events.push(Ok(LlmEvent::Stop(LlmStopEvent {
                reason: "end_turn".into(),
            })));
        }
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn lightweight_continue_on_last_round_preserves_decision_without_full_review() {
    let root = std::env::temp_dir().join(format!(
        "mink-lightweight-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let runtime = AgentRuntime::start(
        AgentOptions::new(root.join("home"), &cwd)
            .with_llm_backend(Arc::new(LightweightReviewBackend)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let mut spec = GoalSpec::new("Implement a deliverable");
    spec.limits.max_turns = 1;
    let draft = handle.draft_goal(spec, 3600).unwrap();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(result.status, GoalStatus::BudgetExhausted, "{result:?}");
    assert_eq!(result.iterations.len(), 1);
    assert!(result.reviews.is_empty());
    assert_eq!(
        serde_json::to_value(&result).unwrap()["continuations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(result.completion_basis.is_none());
}

struct RepeatedContinueBackend;

struct SimpleConclusionBackend;

#[tokio::test]
async fn goal_state_control_matrix_preserves_terminal_and_retry_boundaries() {
    use crate::runtime::TurnStatus;
    let (runtime, handle, root, _) = blocked_review_fixture().await;
    for status in [
        GoalStatus::Draft,
        GoalStatus::Active,
        GoalStatus::Paused,
        GoalStatus::Blocked,
        GoalStatus::BudgetExhausted,
        GoalStatus::Completed,
        GoalStatus::Cancelled,
        GoalStatus::Failed,
    ] {
        for action in ["resume", "budget", "pause", "cancel", "clear"] {
            let goal = handle
                .goal_ctx
                .goal_store
                .update_for_test(|g| {
                    g.status = status.clone();
                    g.phase = GoalPhase::Idle;
                    g.hidden = status == GoalStatus::Failed && action == "resume";
                    g.reason = Some("request failed: error decoding response body".into());
                    g.review_candidate = None;
                    g.review_failure_reason = None;
                    g.pending_effects.clear();
                    g.iterations[0].finished = true;
                    g.iterations[0].outcome_status = Some(TurnStatus::Failed);
                    g.iterations[0].outcome_error = g.reason.clone();
                    Ok(())
                })
                .unwrap();
            let (actual, expected) = match action {
                "resume" => (
                    handle.resume_goal(&goal.id, goal.revision),
                    matches!(
                        status,
                        GoalStatus::Active
                            | GoalStatus::Paused
                            | GoalStatus::Blocked
                            | GoalStatus::BudgetExhausted
                            | GoalStatus::Failed
                    ),
                ),
                "budget" => (
                    handle.update_goal_limits(&goal.id, goal.revision, goal.spec.limits.clone()),
                    !matches!(status, GoalStatus::Completed | GoalStatus::Cancelled),
                ),
                "pause" => (
                    handle.pause_goal(&goal.id, goal.revision),
                    !status.is_terminal() && status != GoalStatus::Draft,
                ),
                "cancel" => (
                    handle.cancel_goal(&goal.id, goal.revision),
                    !status.is_terminal(),
                ),
                "clear" => (
                    handle.clear_goal(&goal.id, goal.revision),
                    status.is_terminal(),
                ),
                _ => unreachable!(),
            };
            assert_eq!(actual.is_ok(), expected, "{status:?}/{action}: {actual:?}");
            let current = handle.goal_snapshot().unwrap();
            assert_eq!(current.id, goal.id);
            assert_eq!(current.spec, goal.spec);
            assert_eq!(current.iterations, goal.iterations);
            assert_eq!(current.usage, goal.usage);
            if expected {
                let target = match action {
                    "resume" => GoalStatus::Active,
                    "pause" => GoalStatus::Paused,
                    "cancel" => GoalStatus::Cancelled,
                    _ => status.clone(),
                };
                assert_eq!(current.status, target, "{status:?}/{action}");
                if action == "resume" {
                    assert!(!current.hidden);
                }
                if action == "clear" {
                    assert!(current.hidden);
                }
            }
            if !expected {
                assert_eq!(current.revision, goal.revision);
            }
        }
    }
    close_fixture(runtime, &handle, root).await;
}

#[tokio::test]
async fn failed_goal_retries_after_restart_and_explicit_budget_adjustment() {
    use crate::runtime::SessionPolicy;
    let root = std::env::temp_dir().join(format!(
        "mink-failed-retry-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(ExecutionRequestFailureBackend {
        execution_calls: AtomicUsize::new(0),
        kind: None,
    });
    let options = || {
        AgentOptions::new(root.join("home"), &cwd)
            .with_llm_backend(backend.clone())
            .with_llm_recovery(crate::config::LlmRecoveryPolicy {
                request_max_retries: 0,
                ..Default::default()
            })
    };
    let runtime = AgentRuntime::start(options()).await.unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let mut spec = GoalSpec::new("Implement deliverable");
    spec.limits.max_turns = 2;
    let draft = handle.draft_goal(spec, 3600).unwrap();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    let failed = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.status, GoalStatus::Failed);
    handle
        .goal_ctx
        .goal_store
        .update_for_test(|g| {
            g.spec.limits.deadline_unix_secs = goal_now() - 1;
            Ok(())
        })
        .unwrap();
    let sid = handle.session_info().session_id.clone();
    runtime.shutdown().await.unwrap();
    drop(handle);
    let runtime = AgentRuntime::start(options().with_session(SessionPolicy::Resume(sid)))
        .await
        .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let old = handle.goal_snapshot().unwrap();
    assert_eq!(
        old.status,
        GoalStatus::Failed,
        "opening a session must not automatically retry old failures"
    );
    assert_eq!(backend.execution_calls.load(Ordering::SeqCst), 2);
    let pending = handle
        .goal_ctx
        .goal_store
        .change(&old.id, old.revision, |g| {
            g.pending_effects.push("unknown-effect".into());
            g.failure_fingerprint = Some("previous-failure".into());
            g.repeated_failures = 2;
            Ok(())
        })
        .unwrap();
    assert!(
        handle
            .resume_goal(&pending.id, pending.revision)
            .unwrap_err()
            .to_string()
            .contains("reconciliation")
    );
    assert!(
        handle
            .reconcile_goal_effects(&pending.id, pending.revision, String::new())
            .is_err()
    );
    let old = handle
        .reconcile_goal_effects(
            &pending.id,
            pending.revision,
            "Investigated: no unknown remote effects remain".into(),
        )
        .unwrap();
    assert_eq!(old.status, GoalStatus::Failed);
    let rejected = handle
        .resume_goal(&old.id, old.revision)
        .unwrap_err()
        .to_string();
    assert!(
        rejected.contains("adjust exhausted goal limits"),
        "{rejected}"
    );
    let adjusted = handle
        .update_goal_limits(&old.id, old.revision, failed.spec.limits.clone())
        .unwrap();
    assert_eq!(
        adjusted.status,
        GoalStatus::Failed,
        "budget adjustment must not activate"
    );
    assert_eq!(adjusted.iterations, failed.iterations);
    assert_eq!(adjusted.usage_records, failed.usage_records);
    assert!(
        handle.resume_goal(&adjusted.id, old.revision).is_err(),
        "stale retries must be rejected"
    );
    handle.resume_goal(&adjusted.id, adjusted.revision).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, GoalStatus::BudgetExhausted, "{result:?}");
    assert_eq!(result.id, old.id);
    assert_eq!(result.contract_revision, old.contract_revision);
    assert_eq!(result.iterations.len(), 2);
    assert_eq!(result.iterations[0], failed.iterations[0]);
    assert_eq!(result.spec, failed.spec);
    assert_eq!(result.failure_fingerprint, old.failure_fingerprint);
    assert_eq!(result.repeated_failures, old.repeated_failures);
    assert_eq!(backend.execution_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        std::fs::read_to_string(cwd.join("progress.txt")).unwrap(),
        "committed once"
    );
    for record in &old.usage_records {
        assert_eq!(
            result
                .usage_records
                .iter()
                .filter(|r| r.request_id == record.request_id)
                .count(),
            1
        );
    }
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn resume_phase_matrix_allows_last_round_review_without_an_extra_execution_round() {
    let (runtime, handle, root, _) = blocked_review_fixture().await;
    for status in [
        GoalStatus::Active,
        GoalStatus::Paused,
        GoalStatus::Blocked,
        GoalStatus::BudgetExhausted,
        GoalStatus::Failed,
    ] {
        for phase in [GoalPhase::Idle, GoalPhase::Executing, GoalPhase::Verifying] {
            let goal = handle
                .goal_ctx
                .goal_store
                .update_for_test(|g| {
                    g.status = status.clone();
                    g.phase = phase.clone();
                    g.spec.limits.max_turns = 1;
                    g.review_failure_reason = None;
                    Ok(())
                })
                .unwrap();
            let resumed = handle.resume_goal(&goal.id, goal.revision);
            assert_eq!(
                resumed.is_ok(),
                phase == GoalPhase::Verifying,
                "{status:?}/{phase:?}: {resumed:?}"
            );
            let current = handle.goal_snapshot().unwrap();
            assert_eq!(current.iterations, goal.iterations);
            assert_eq!(current.spec.limits, goal.spec.limits);
            if let Ok(resumed) = resumed {
                assert_eq!(resumed.phase, GoalPhase::Verifying);
            }
        }
    }
    close_fixture(runtime, &handle, root).await;
}

struct ExecutionRequestFailureBackend {
    execution_calls: AtomicUsize,
    kind: Option<crate::llm::recovery::UpstreamFailureKind>,
}

#[async_trait]
impl LlmBackend for ExecutionRequestFailureBackend {
    fn name(&self) -> &str {
        "execution-request-failure"
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        use crate::runtime::{LlmEvent, LlmStopEvent};
        let prompt = request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .rfind(|s| !s.starts_with("<goal-conclusion-required>"))
            .unwrap_or("");
        if prompt.starts_with("<independent-goal-review>") || prompt.starts_with("<goal-review>") {
            return SimpleConclusionBackend.stream(request).await;
        }
        match self.execution_calls.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(LlmResponseStream {
                events: Box::pin(futures::stream::iter(vec![
                    Ok(LlmEvent::ToolCall(crate::protocol::ToolCallEvent {
                        id: "write-before-request-failure".into(),
                        name: "Write".into(),
                        input_json: serde_json::json!({"path":"progress.txt","content":"committed once"}),
                        fields: Default::default(),
                        parse_error: None,
                        raw_arguments_digest: None,
                    })),
                    Ok(LlmEvent::Stop(LlmStopEvent {
                        reason: "tool_use".into(),
                    })),
                ])),
                attempt_count: 1,
            }),
            1 => match self.kind {
                Some(kind) => Err(crate::llm::recovery::LlmUpstreamError::new(
                    kind,
                    "response decoding failed",
                )
                .into()),
                None => anyhow::bail!("untyped request failure"),
            },
            _ => {
                assert!(
                    serde_json::to_string(&*request.messages)?
                        .contains("write-before-request-failure"),
                    "recovery must retain the committed tool exchange"
                );
                SimpleConclusionBackend.stream(request).await
            }
        }
    }
}

#[tokio::test]
async fn execution_failure_classes_stop_automatically_but_allow_explicit_retry() {
    use crate::llm::recovery::UpstreamFailureKind;
    for kind in [
        Some(UpstreamFailureKind::Recoverable),
        Some(UpstreamFailureKind::Permanent),
        Some(UpstreamFailureKind::ProtocolDamaged),
        None,
    ] {
        let root = std::env::temp_dir().join(format!(
            "mink-terminal-execution-{}",
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let backend = Arc::new(ExecutionRequestFailureBackend {
            execution_calls: AtomicUsize::new(0),
            kind,
        });
        let runtime = AgentRuntime::start(
            AgentOptions::new(root.join("home"), &cwd)
                .with_llm_backend(backend.clone())
                .with_llm_recovery(crate::config::LlmRecoveryPolicy {
                    request_max_retries: 0,
                    ..Default::default()
                }),
        )
        .await
        .unwrap();
        let handle = runtime.handle();
        handle.start_goal_driver().unwrap();
        let mut spec = GoalSpec::new("Implement deliverable");
        spec.limits.max_turns = 2;
        let draft = handle.draft_goal(spec, 3600).unwrap();
        handle.confirm_goal(&draft.id, draft.revision).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status, GoalStatus::Failed, "{kind:?}: {result:?}");
        assert_eq!(
            backend.execution_calls.load(Ordering::SeqCst),
            2,
            "failure must stop automatic execution"
        );
        handle.resume_goal(&result.id, result.revision).unwrap();
        let retried = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retried.status, GoalStatus::BudgetExhausted, "{retried:?}");
        assert_eq!(retried.iterations.len(), 2);
        assert_eq!(retried.iterations[0], result.iterations[0]);
        assert_eq!(retried.spec, result.spec);
        assert_eq!(backend.execution_calls.load(Ordering::SeqCst), 3);
        runtime.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn simple_conclusion_missing_decision_does_not_request_a_legacy_candidate() {
    let error = serde_json::from_value::<GoalReviewSubmission>(
        serde_json::json!({"summary":"work remains"}),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("decision"), "{error}");
    assert!(!error.contains("candidate"), "{error}");
}

struct SimpleBlockerBackend {
    host_calls: AtomicUsize,
}
#[async_trait]
impl LlmBackend for SimpleBlockerBackend {
    fn name(&self) -> &str {
        "simple-blocker-identity"
    }
    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        use crate::runtime::{LlmEvent, LlmStopEvent, LlmTextEvent};
        let prompt = request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .rfind(|s| !s.starts_with("<goal-conclusion-required>"))
            .unwrap_or("");
        let call = |id: &str, name: &str, input_json| {
            Ok(LlmEvent::ToolCall(crate::protocol::ToolCallEvent {
                id: format!("{id}-{}", request.messages.len()),
                name: name.into(),
                input_json,
                fields: Default::default(),
                parse_error: None,
                raw_arguments_digest: None,
            }))
        };
        let stop = |reason: &str| {
            Ok(LlmEvent::Stop(LlmStopEvent {
                reason: reason.into(),
            }))
        };
        let events = if prompt.starts_with("<goal-review>") {
            let n = self.host_calls.fetch_add(1, Ordering::SeqCst);
            vec![
                call(
                    "blocked",
                    "GoalReview",
                    serde_json::json!({"decision":"blocked","summary":"Waiting for independent operator","blocker":format!("Operator approval is missing, observation {n}"),"next_step":"Obtain operator approval"}),
                ),
                stop("tool_use"),
            ]
        } else if prompt.starts_with("<independent-goal-review>") {
            if request
                .messages
                .last()
                .is_some_and(|m| m["content"].is_array())
            {
                vec![Ok(LlmEvent::Text(LlmTextEvent {content:"I inspected the dependency status and conclusion. The external operator approval is absent; all requirements remain blocked.".into()})),stop("end_turn")]
            } else {
                let artifact = prompt
                    .lines()
                    .find_map(|s| s.strip_prefix("Full conclusion: "))
                    .unwrap();
                vec![
                    call(
                        "dependency",
                        "Read",
                        serde_json::json!({"path":"proof.txt"}),
                    ),
                    call("conclusion", "Read", serde_json::json!({"path":artifact})),
                    stop("tool_use"),
                ]
            }
        } else {
            vec![
                Ok(LlmEvent::Text(LlmTextEvent {
                    content: format!(
                        "Operator dependency still missing, round {}",
                        self.host_calls.load(Ordering::SeqCst)
                    ),
                })),
                stop("end_turn"),
            ]
        };
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn descriptive_blocker_identity_ignores_wording_and_round_conclusion_artifacts() {
    let root = std::env::temp_dir().join(format!(
        "mink-simple-blocker-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "operator approval pending").unwrap();
    let runtime = AgentRuntime::start(AgentOptions::new(root.join("home"), &cwd).with_llm_backend(
        Arc::new(SimpleBlockerBackend {
            host_calls: AtomicUsize::new(0),
        }),
    ))
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let mut spec = GoalSpec::new("Obtain operator approval");
    spec.limits.max_turns = 3;
    let draft = handle.draft_goal(spec, 3600).unwrap();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(result.status, GoalStatus::Blocked, "{result:?}");
    assert_eq!(result.blocker_rounds, 3);
    assert_eq!(result.reviews.len(), 3);
    assert!(result.review_failure_reason.is_none());
}

#[tokio::test]
async fn simple_conclusion_cannot_complete_without_independent_evidence() {
    use crate::tools::runner::ToolExec;
    let (runtime, handle, root, goal) = blocked_review_fixture().await;
    let artifact = handle
        .goal_ctx
        .artifacts
        .write_text(
            "IndependentGoalReview",
            "report",
            "Necessary contract work remains; implement it next",
        )
        .unwrap();
    let current = handle
        .goal_ctx
        .goal_store
        .change(&goal.id, goal.revision, |g| {
            g.reviews.clear();
            g.review_requires_full = true;
            g.review_report = Some(GoalReviewReport {
                candidate: g.review_candidate.clone().unwrap(),
                artifact: format!("artifact://{}", artifact.id),
                content_hash: hash_bytes(b"Necessary contract work remains; implement it next"),
                reference_mapping: vec![],
                evidence_versions: vec![],
                conversation_seq: 0,
            });
            Ok(())
        })
        .unwrap();
    let session = Arc::new(super::review::GoalReviewSession::new(
        current.clone(),
        handle.goal_ctx.goal_store.clone(),
        handle.goal_ctx.usage.clone(),
    ));
    let mut ctx = crate::context::ToolContext::from(handle.goal_ctx.as_ref());
    ctx.goal_adjudication = Some(session.clone());
    let tool = crate::tools::goal::GoalReviewTool;
    let rejected = tool
        .execute(
            &serde_json::json!({"decision":"complete","summary":"claimed complete"}),
            &ctx,
        )
        .unwrap();
    assert!(!rejected.success);
    assert!(
        rejected
            .content
            .contains("independently inspected evidence")
    );
    assert!(!session.has_submission());
    let payload = serde_json::json!({"decision":"continue","summary":"The independent report confirms work remains","next_step":"Implement deliverable"});
    let decoded: GoalReviewSubmission = serde_json::from_value(payload.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(decoded).unwrap(),
        serde_json::json!({"decision":"continue","summary":"The independent report confirms work remains","next_step":"Implement deliverable","remaining_requirements":[]})
    );
    assert!(tool.execute(&payload, &ctx).unwrap().success);
    let review = session.submission.lock().unwrap().clone().unwrap();
    assert_eq!(review.verdict, GoalReviewVerdict::NeedsWork);
    assert!(review.original_request.is_none() && review.criteria.is_empty());
    assert_eq!(
        handle.goal_snapshot().unwrap(),
        current,
        "submission mailbox does not mutate GoalStore"
    );
    close_fixture(runtime, &handle, root).await;
}

struct SimpleCompletionBackend {
    reader: HostConclusionBackend,
    host_calls: AtomicUsize,
}
#[async_trait]
impl LlmBackend for SimpleCompletionBackend {
    fn name(&self) -> &str {
        "simple-completion"
    }
    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        if !request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .rfind(|s| !s.starts_with("<goal-conclusion-required>"))
            .is_some_and(|s| s.starts_with("<goal-review>"))
        {
            return self.reader.stream(request).await;
        }
        use crate::runtime::{LlmEvent, LlmStopEvent};
        let attempt = self.host_calls.fetch_add(1, Ordering::SeqCst);
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(LlmEvent::ToolCall(crate::protocol::ToolCallEvent {
                    id: format!("simple-complete-{attempt}"),
                    name: "GoalReview".into(),
                    input_json: serde_json::json!({"decision":"complete","summary":if attempt < 4 { "" } else { "Independent reviewer inspected and verified the deliverable" }}),
                    fields: Default::default(),
                    parse_error: None,
                    raw_arguments_digest: None,
                })),
                Ok(LlmEvent::Stop(LlmStopEvent {
                    reason: "tool_use".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn simple_completion_retries_semantic_rejection_and_keeps_descriptive_evidence() {
    let root = std::env::temp_dir().join(format!(
        "mink-simple-completion-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "the requested deliverable").unwrap();
    let backend = Arc::new(SimpleCompletionBackend {
        reader: HostConclusionBackend {
            reviewer_requests: AtomicUsize::new(0),
            host_requests: AtomicUsize::new(0),
        },
        host_calls: AtomicUsize::new(0),
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(root.join("home"), &cwd).with_llm_backend(backend.clone()),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let draft = handle
        .draft_goal(GoalSpec::new("deliver proof"), 3600)
        .unwrap();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, GoalStatus::Completed, "{result:?}");
    assert_eq!(backend.reader.reviewer_requests.load(Ordering::SeqCst), 2);
    assert_eq!(backend.host_calls.load(Ordering::SeqCst), 5);
    assert_eq!(result.iterations.len(), 1);
    assert!(result.reviews[0].original_request.is_none());
    assert!(result.reviews[0].criteria.is_empty());
    assert!(result.reviews[0].report.is_some());
    assert_eq!(
        handle.validate_review_evidence(&result).unwrap(),
        result.evidence_versions
    );
    std::fs::write(cwd.join("proof.txt"), "changed").unwrap();
    assert!(handle.validate_review_evidence(&result).is_err());
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
#[async_trait]
impl LlmBackend for SimpleConclusionBackend {
    fn name(&self) -> &str {
        "simple-host-conclusion"
    }
    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        let adjudicating = request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .rfind(|s| !s.starts_with("<goal-conclusion-required>"))
            .is_some_and(|s| s.starts_with("<goal-review>"));
        if !adjudicating {
            return RepeatedContinueBackend.stream(request).await;
        }
        use crate::runtime::{LlmEvent, LlmStopEvent};
        let schema = request
            .tools
            .iter()
            .find(|t| t["name"] == "GoalReview")
            .unwrap();
        assert!(
            schema["input_schema"]["properties"]
                .get("candidate")
                .is_none(),
            "the runtime must bind the candidate, not ask the host to copy it"
        );
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(LlmEvent::ToolCall(crate::protocol::ToolCallEvent {
                    id: format!("conclude-{}", request.messages.len()),
                    name: "GoalReview".into(),
                    input_json: serde_json::json!({"decision":"continue","summary":"Independent report confirms necessary work remains","next_step":"Implement the missing deliverable","remaining_requirements":["criterion-1"]}),
                    fields: Default::default(),
                    parse_error: None,
                    raw_arguments_digest: None,
                })),
                Ok(LlmEvent::Stop(LlmStopEvent {
                    reason: "tool_use".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn simple_host_conclusion_continues_after_screening_and_inspection() {
    let root = std::env::temp_dir().join(format!(
        "mink-simple-conclusion-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "partial").unwrap();
    let runtime = AgentRuntime::start(
        AgentOptions::new(root.join("home"), &cwd)
            .with_llm_backend(Arc::new(SimpleConclusionBackend)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let mut spec = GoalSpec::new("Implement deliverable");
    spec.limits.max_turns = 3;
    let draft = handle.draft_goal(spec, 3600).unwrap();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(result.status, GoalStatus::BudgetExhausted, "{result:?}");
    assert!(result.review_failure_reason.is_none(), "{result:?}");
    assert_eq!(result.iterations.len(), 3);
    assert_eq!(result.continuations.len(), 2);
    assert_eq!(result.reviews.len(), 1);
    assert!(
        result.reviews[0].criteria.is_empty(),
        "do not manufacture per-criterion findings from prose"
    );
}
#[async_trait]
impl LlmBackend for RepeatedContinueBackend {
    fn name(&self) -> &str {
        "repeated-continue-review"
    }
    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        use crate::runtime::{LlmEvent, LlmStopEvent, LlmTextEvent};
        let prompt = request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .rfind(|s| !s.starts_with("<goal-conclusion-required>"))
            .unwrap_or("");
        let forced = prompt.contains("Full evidence review required: true");
        let call = |name: &str, input_json| {
            Ok(LlmEvent::ToolCall(crate::protocol::ToolCallEvent {
                id: format!("{name}-{}", request.messages.len()),
                name: name.into(),
                input_json,
                fields: Default::default(),
                parse_error: None,
                raw_arguments_digest: None,
            }))
        };
        let stop = |reason: &str| {
            Ok(LlmEvent::Stop(LlmStopEvent {
                reason: reason.into(),
            }))
        };
        let events = if prompt.starts_with("<independent-goal-review>") && forced {
            let read_result = request
                .messages
                .last()
                .and_then(|m| m["content"].as_array())
                .is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|b| b["type"] == "tool_result" && b["_mink"]["tool_name"] == "Read")
                });
            if !read_result {
                vec![
                    call("Read", serde_json::json!({"path":"proof.txt"})),
                    stop("tool_use"),
                ]
            } else {
                vec![Ok(LlmEvent::Text(LlmTextEvent {content:"I inspected proof.txt. criterion-1 is still unfinished; implement the missing deliverable. No external blocker.".into()})),stop("end_turn")]
            }
        } else if prompt.starts_with("<goal-review>") {
            let candidate: serde_json::Value = serde_json::from_str(
                prompt
                    .lines()
                    .find_map(|l| l.strip_prefix("Candidate: "))
                    .unwrap(),
            )?;
            let payload = if forced {
                serde_json::json!({"mode":"full","candidate":candidate,"verdict":"needs_work","original_request":{"status":"unknown","reason":"the required work remains","evidence":[]},"criteria":[{"criterion_id":"criterion-1","status":"fail","reason":"proof lacks the deliverable","evidence":["proof.txt"]}],"constraints":[],"summary":"Still incomplete after inspection","next_step":"Implement missing deliverable","blocker":null})
            } else {
                serde_json::json!({"mode":"continue","candidate":candidate,"summary":format!("Partial work, request length {}",request.messages.len()),"remaining_requirements":["criterion-1"],"next_step":format!("Proceed with the deliverable, wording {}",request.messages.len())})
            };
            vec![call("GoalReview", payload), stop("tool_use")]
        } else {
            vec![
                Ok(LlmEvent::Text(LlmTextEvent {
                    content: if prompt.starts_with("<independent-goal-review>") {
                        "Host explicitly leaves criterion-1 unfinished; implement the missing deliverable next. Full verification was not performed.".into()
                    } else {
                        "Part of criterion-1 is complete; next implement the missing deliverable."
                            .into()
                    },
                })),
                stop("end_turn"),
            ]
        };
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn third_repeated_continue_inspects_in_same_reviewer_and_counts_once() {
    let root = std::env::temp_dir().join(format!(
        "mink-repeat-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "partial").unwrap();
    let runtime = AgentRuntime::start(
        AgentOptions::new(root.join("home"), &cwd)
            .with_llm_backend(Arc::new(RepeatedContinueBackend)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let mut spec = GoalSpec::new("Implement deliverable");
    spec.limits.max_turns = 3;
    let draft = handle.draft_goal(spec, 3600).unwrap();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(result.status, GoalStatus::BudgetExhausted, "{result:?}");
    assert_eq!(result.iterations.len(), 3);
    assert_eq!(result.continuations.len(), 2);
    assert_eq!(result.reviews.len(), 1);
    assert_eq!(
        result
            .continuations
            .iter()
            .map(|r| r.consecutive_rounds)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(result.continuation_rounds, 0);
    let last_billing = format!("goal-review:{}:3", result.id);
    let children: std::collections::BTreeSet<_> = result
        .usage_records
        .iter()
        .filter(|r| {
            r.billing_turn_id == last_billing
                && r.kind == crate::session::usage::UsageKind::SubAgent
        })
        .map(|r| r.origin_session_id.clone())
        .collect();
    assert_eq!(
        children.len(),
        1,
        "screening and escalation use one child session"
    );
    assert_eq!(result.usage.request_count, 12);
    assert!(matches!(
        result.latest_round_decision(),
        Some(GoalRoundDecision::Full(_))
    ));
}

#[tokio::test]
async fn continuation_validation_is_negative_only_and_recovery_is_idempotent() {
    use crate::tools::runner::ToolExec;
    let (runtime, handle, root, goal) = blocked_review_fixture().await;
    let artifact = handle
        .goal_ctx
        .artifacts
        .write_text(
            "IndependentGoalReview",
            "screening",
            "criterion-1 remains unfinished; implement it next",
        )
        .unwrap();
    let candidate = goal.review_candidate.clone().unwrap();
    let current = handle
        .goal_ctx
        .goal_store
        .change(&goal.id, goal.revision, |g| {
            g.reviews.clear();
            g.phase = GoalPhase::Verifying;
            g.review_report = Some(GoalReviewReport {
                candidate: candidate.clone(),
                artifact: format!("artifact://{}", artifact.id),
                content_hash: hash_bytes(b"criterion-1 remains unfinished; implement it next"),
                reference_mapping: vec![],
                evidence_versions: vec![],
                conversation_seq: 0,
            });
            Ok(())
        })
        .unwrap();
    let session = Arc::new(super::review::GoalReviewSession::new(
        current.clone(),
        handle.goal_ctx.goal_store.clone(),
        handle.goal_ctx.usage.clone(),
    ));
    let mut ctx = crate::context::ToolContext::from(handle.goal_ctx.as_ref());
    let proposal = GoalContinueProposal {
        candidate: candidate.clone(),
        summary: "partial work".into(),
        remaining_requirements: vec!["criterion-1".into()],
        next_step: "implement remainder".into(),
    };
    let payload = serde_json::to_value(GoalReviewSubmission::Continue(proposal.clone())).unwrap();
    assert!(
        crate::tools::goal::GoalReviewTool
            .execute(&payload, &ctx)
            .is_err()
    );
    ctx.goal_adjudication = Some(session.clone());
    for ids in [
        vec![],
        vec!["unknown".into()],
        vec!["criterion-1".into(), "criterion-1".into()],
    ] {
        let mut bad = proposal.clone();
        bad.remaining_requirements = ids;
        let output = crate::tools::goal::GoalReviewTool
            .execute(
                &serde_json::to_value(GoalReviewSubmission::Continue(bad)).unwrap(),
                &ctx,
            )
            .unwrap();
        assert!(!output.success);
        assert!(!session.has_submission());
    }
    let mut injected = payload.clone();
    injected["verdict"] = serde_json::json!("satisfied");
    assert!(serde_json::from_value::<GoalReviewSubmission>(injected).is_err());
    assert!(
        crate::tools::goal::GoalReviewTool
            .execute(&payload, &ctx)
            .unwrap()
            .success
    );
    assert_eq!(
        handle.goal_snapshot().unwrap(),
        current,
        "mailbox does not publish parent state"
    );
    handle.commit_goal_continue(1, proposal.clone(), 0).unwrap();
    handle.commit_goal_continue(1, proposal, 0).unwrap();
    let after = handle.goal_snapshot().unwrap();
    assert_eq!(after.continuations.len(), 1);
    assert_eq!(after.continuation_rounds, 1);
    assert!(after.reviews.is_empty());
    assert!(after.completion_basis.is_none());
    handle
        .goal_ctx
        .goal_store
        .recover(&after.environment, &[], 0)
        .unwrap();
    let restored = handle.goal_snapshot().unwrap();
    assert_eq!(restored.phase, GoalPhase::Idle);
    assert_eq!(restored.continuation_rounds, 1);
    assert!(matches!(
        restored.latest_round_decision(),
        Some(GoalRoundDecision::Continue(_))
    ));
    let replay = handle
        .goal_ctx
        .goal_store
        .change(&restored.id, restored.revision, |g| {
            g.phase = GoalPhase::Verifying;
            Ok(())
        })
        .unwrap();
    handle.accept_goal(1, replay).await.unwrap();
    let replayed = handle.goal_snapshot().unwrap();
    assert_eq!(replayed.phase, GoalPhase::Idle);
    assert_eq!(replayed.continuation_rounds, 1);
    assert_eq!(replayed.continuations.len(), 1);
    assert!(
        handle.goal_ctx.usage.all_records().unwrap().is_empty(),
        "replay sends no provider requests"
    );
    close_fixture(runtime, &handle, root).await;
}

#[tokio::test]
async fn continuation_streak_ignores_wording_but_changes_with_requirements() {
    let (runtime, handle, root, goal) = blocked_review_fixture().await;
    let candidate = goal.review_candidate.clone().unwrap();
    let a = GoalContinueProposal {
        candidate,
        summary: "partial".into(),
        remaining_requirements: vec!["criterion-1".into(), "request:original".into()],
        next_step: "implement".into(),
    };
    let mut b = a.clone();
    b.remaining_requirements.reverse();
    b.summary = "still incomplete".into();
    b.next_step = "finish the deliverable".into();
    assert_eq!(a.fingerprint().unwrap(), b.fingerprint().unwrap());
    b.remaining_requirements.pop();
    assert_ne!(a.fingerprint().unwrap(), b.fingerprint().unwrap());
    b = a.clone();
    b.candidate.contract_revision += 1;
    assert_ne!(a.fingerprint().unwrap(), b.fingerprint().unwrap());
    let mut forced = goal;
    forced.review_requires_full = true;
    assert!(a.validate(&forced).is_err());
    close_fixture(runtime, &handle, root).await;
}

struct RetryHostOnlyBackend {
    reviewer_calls: AtomicUsize,
    host_calls: AtomicUsize,
}
#[async_trait]
impl LlmBackend for RetryHostOnlyBackend {
    fn name(&self) -> &str {
        "cached-review-report"
    }
    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        let prompt = request
            .messages
            .iter()
            .filter_map(|m| m["content"].as_str())
            .rfind(|s| !s.starts_with("<goal-conclusion-required>"))
            .unwrap_or("");
        if prompt.starts_with("<independent-goal-review>") {
            self.reviewer_calls.fetch_add(1, Ordering::SeqCst);
        }
        if prompt.starts_with("<goal-review>")
            && self.host_calls.fetch_add(1, Ordering::SeqCst) == 0
        {
            anyhow::bail!("host submission unavailable once");
        }
        SimpleConclusionBackend.stream(request).await
    }
}

#[tokio::test]
async fn restart_reuses_successful_screening_report_and_retries_only_host() {
    use crate::runtime::SessionPolicy;
    let root = std::env::temp_dir().join(format!(
        "mink-report-restart-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(RetryHostOnlyBackend {
        reviewer_calls: AtomicUsize::new(0),
        host_calls: AtomicUsize::new(0),
    });
    let options = || AgentOptions::new(root.join("home"), &cwd).with_llm_backend(backend.clone());
    let runtime = AgentRuntime::start(options()).await.unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let mut spec = GoalSpec::new("Implement deliverable");
    spec.limits.max_turns = 1;
    let draft = handle.draft_goal(spec, 3600).unwrap();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    let blocked = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(blocked.status, GoalStatus::Blocked);
    assert_eq!(
        blocked.review_failure_stage,
        Some(GoalReviewStage::Adjudicating)
    );
    assert!(blocked.review_report.is_some());
    let sid = handle.session_info().session_id.clone();
    runtime.shutdown().await.unwrap();
    drop(handle);
    let reopened = AgentRuntime::start(options().with_session(SessionPolicy::Resume(sid)))
        .await
        .unwrap();
    let handle = reopened.handle();
    handle.start_goal_driver().unwrap();
    let current = handle.goal_snapshot().unwrap();
    handle.resume_goal(&current.id, current.revision).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    reopened.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(result.status, GoalStatus::BudgetExhausted, "{result:?}");
    assert_eq!(result.iterations.len(), 1);
    assert_eq!(result.continuations.len(), 1);
    assert_eq!(
        backend.reviewer_calls.load(Ordering::SeqCst),
        1,
        "the successful report survives process restart"
    );
}

#[tokio::test]
async fn unknown_goal_format_version_fails_closed() {
    use crate::runtime::SessionPolicy;
    let root = std::env::temp_dir().join(format!(
        "mink-goal-version-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let options =
        || AgentOptions::new(root.join("home"), &cwd).with_llm_backend(Arc::new(NoRequest));
    let runtime = AgentRuntime::start(options()).await.unwrap();
    let handle = runtime.handle();
    handle
        .goal_ctx
        .goal_store
        .create(
            GoalSpec::new("reject unknown formats"),
            goal_environment(&cwd, &handle.goal_ctx.tool_surface),
        )
        .unwrap();
    let directory = handle.goal_ctx.store.path().parent().unwrap().to_path_buf();
    let sid = handle.session_info().session_id.clone();
    runtime.shutdown().await.unwrap();
    drop(handle);
    // A container written by another format version must never read as
    // executable state.
    for version in [2, 4] {
        let mut state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join("goal.json")).unwrap()).unwrap();
        state["version"] = serde_json::json!(version);
        state["current"]["version"] = serde_json::json!(version);
        let bytes = serde_json::to_vec(&state).unwrap();
        std::fs::write(directory.join("goal.json"), &bytes).unwrap();
        std::fs::write(directory.join("goal-transaction.json"), &bytes).unwrap();
        assert!(
            AgentRuntime::start(options().with_session(SessionPolicy::Resume(sid.clone())))
                .await
                .is_err(),
            "format version {version} must be rejected"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn screening_uses_only_final_committed_response_at_execution_boundary() {
    let history = vec![
        serde_json::json!({"seq":1,"role":"assistant","content":[{"type":"text","text":"old turn"}]}),
        serde_json::json!({"seq":3,"role":"assistant","content":[{"type":"text","text":"progress is not a final conclusion"},{"type":"tool_use","name":"Read"}]}),
        serde_json::json!({"seq":4,"role":"user","content":[{"type":"tool_result","content":"file"}]}),
        serde_json::json!({"seq":5,"role":"assistant","content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"Partial completion."},{"type":"text","text":"Next finish criterion-1."}]}),
        serde_json::json!({"seq":6,"role":"assistant","content":[{"type":"text","text":"later host adjudication"}]}),
    ];
    assert_eq!(
        super::review::execution_conclusion(&history, 3, 5),
        "Partial completion.\nNext finish criterion-1."
    );
    assert_eq!(super::review::execution_conclusion(&history, 3, 4), "");
    assert_eq!(super::review::execution_conclusion(&history, 5, 5), "");
}

/// A backend that holds the first (execution) request until the test releases
/// it, then answers every later request with a plain completion.
struct HoldingBackend {
    calls: AtomicUsize,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl LlmBackend for HoldingBackend {
    fn name(&self) -> &str {
        "holding-goal-execution"
    }
    async fn stream(&self, _: LlmRequest) -> Result<LlmResponseStream> {
        use crate::runtime::{LlmEvent, LlmStopEvent, LlmTextEvent};
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(LlmEvent::Text(LlmTextEvent {
                    content: "held execution finished".into(),
                })),
                Ok(LlmEvent::Stop(LlmStopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

/// A live execution owns its goal: no replacement contract may be created or
/// confirmed underneath it, so a late settlement or side effect can never be
/// written onto another goal. Cancelling the same goal still converges: the run
/// settles once the held execution exits and never restarts itself.
#[tokio::test]
async fn a_live_execution_refuses_a_replacement_until_it_settles() {
    let root = std::env::temp_dir().join(format!(
        "mink-goal-live-owner-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(HoldingBackend {
        calls: AtomicUsize::new(0),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(root.join("home"), &cwd).with_llm_backend(backend.clone()),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let draft = handle
        .draft_goal(GoalSpec::new("hold the execution"), 3600)
        .unwrap();
    let entered = backend.entered.notified();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), entered)
        .await
        .expect("the execution request must reach the backend");

    // While the turn is in flight the run owns the driver and the turn gate:
    // a replacement can be neither drafted nor confirmed, and no late reply can
    // be attributed to a goal that did not start it.
    let busy = handle
        .draft_goal(GoalSpec::new("replacement"), 3600)
        .unwrap_err();
    assert!(matches!(busy, RuntimeError::Busy { .. }), "{busy:?}");
    let stale = handle.confirm_goal(&draft.id, draft.revision).unwrap_err();
    assert!(matches!(stale, RuntimeError::Busy { .. }), "{stale:?}");

    // Cancelling stops the run; the held execution is released and the run
    // settles by itself instead of retrying.
    let current = handle.goal_snapshot().unwrap();
    let cancelled = handle.cancel_goal(&current.id, current.revision).unwrap();
    assert_eq!(cancelled.status, GoalStatus::Cancelled);
    assert_eq!(
        cancelled.phase,
        GoalPhase::Idle,
        "a cancelled goal must not stay verifying"
    );
    backend.release.notify_one();
    let settled = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
        .await
        .expect("the run must settle after its execution exits")
        .unwrap();
    assert_eq!(settled.status, GoalStatus::Cancelled);
    assert_eq!(
        backend.calls.load(Ordering::SeqCst),
        1,
        "a cancelled run must not start another execution"
    );
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// An interrupt set while the goal is `active` and its run is still scheduled
/// stops that run and pauses the goal: the interrupted round settles, the drive
/// loop stays bounded by the turn budget, and no further execution is admitted.
#[tokio::test]
async fn interrupt_during_an_active_round_pauses_without_admitting_another() {
    let root = std::env::temp_dir().join(format!(
        "mink-goal-interrupt-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(HoldingBackend {
        calls: AtomicUsize::new(0),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(root.join("home"), &cwd).with_llm_backend(backend.clone()),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let mut spec = GoalSpec::new("interrupt the only round");
    spec.limits.max_turns = 1;
    let draft = handle.draft_goal(spec, 3600).unwrap();
    let entered = backend.entered.notified();
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), entered)
        .await
        .expect("the execution request must reach the backend");

    // The run is scheduled and the goal is still `active`: the interrupt is
    // set while execution owns the turn.
    assert_eq!(
        handle.goal_snapshot().unwrap().status,
        GoalStatus::Active,
        "the goal must still be active when the interrupt is set"
    );
    handle.goal_ctx.interrupt.store(true, Ordering::SeqCst);
    backend.release.notify_one();

    let settled = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
        .await
        .expect("an interrupt must settle the run instead of spinning")
        .unwrap();
    assert_eq!(
        settled.status,
        GoalStatus::Paused,
        "an interrupted round without a session-wide stop pauses the goal: {settled:?}"
    );
    assert_eq!(settled.phase, GoalPhase::Idle);
    assert_eq!(
        settled.iterations.len(),
        1,
        "the budget must bound the loop: {settled:?}"
    );
    assert_eq!(
        settled.iterations[0].outcome_status,
        Some(crate::runtime::TurnStatus::Interrupted),
        "the round must settle as interrupted: {settled:?}"
    );
    assert_eq!(
        backend.calls.load(Ordering::SeqCst),
        1,
        "no further execution may be admitted after the interrupt"
    );
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn inconclusive_review_keeps_active_and_preserves_candidate() {
    use crate::session::goal::{GoalPhase, GoalStatus, GoalStore, GoalTransition};
    use crate::session::goal_protocol::{GoalReview, GoalReviewVerdict};

    let dir = std::env::temp_dir().join(format!(
        "mink-goal-inconclusive-{}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let store = GoalStore::load(
        &dir,
        crate::session::persistence::PersistenceFault::default(),
    )
    .unwrap();
    let goal = store
        .create(
            crate::session::goal::GoalSpec::new("inconclusive"),
            "env".into(),
        )
        .unwrap();
    let goal = store
        .transition(
            &goal.id,
            goal.revision,
            GoalTransition::PrepareReview {
                conversation_seq: 0,
                reviewer_id: "reviewer".into(),
            },
        )
        .unwrap();
    let candidate = goal.review_candidate.clone().unwrap();
    let goal = store
        .transition(
            &goal.id,
            goal.revision,
            GoalTransition::CommitReview {
                review: GoalReview {
                    candidate: candidate.clone(),
                    verdict: GoalReviewVerdict::Inconclusive,
                    report: None,
                    original_request: None,
                    criteria: Vec::new(),
                    constraints: Vec::new(),
                    summary: "review could not reach a conclusion".into(),
                    next_step: "repeat the evidence review".into(),
                    blocker: None,
                },
                evidence_versions: Vec::new(),
                expected_conversation_seq: 0,
                blocker_fingerprint: None,
            },
        )
        .unwrap();

    assert_eq!(
        goal.status,
        GoalStatus::Active,
        "inconclusive must not end the goal"
    );
    assert_eq!(goal.phase, GoalPhase::Idle);
    assert_eq!(
        goal.review_candidate,
        Some(candidate),
        "the candidate is preserved for an explicit re-audit"
    );
    assert!(
        goal.review_failure_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("inconclusive")),
        "{:?}",
        goal.review_failure_reason
    );
    assert_eq!(
        goal.review_failure_stage,
        Some(crate::session::goal_protocol::GoalReviewStage::Adjudicating)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_blocker_evidence_names_only_the_unavailable_reference() {
    let references = vec!["file://present".to_string(), "file://missing".to_string()];
    let error = super::verification::blocker_evidence_versions(&references, |reference| {
        if reference.ends_with("missing") {
            Err(anyhow::anyhow!("not found"))
        } else {
            Ok("hash-present".into())
        }
    })
    .expect_err("a missing reference must reject the evidence list")
    .to_string();
    assert!(error.contains("file://missing"), "{error}");
    assert!(error.contains("unavailable"), "{error}");
    assert!(!error.contains("file://present"), "{error}");

    let ok = super::verification::blocker_evidence_versions(&references[..1], |_| {
        Ok("hash-present".into())
    })
    .expect("the readable reference still resolves");
    assert_eq!(ok, vec![("file://present", "hash-present".to_string())]);
}
