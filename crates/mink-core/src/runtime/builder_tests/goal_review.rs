use super::*;

#[tokio::test]
async fn goal_missing_review_blocks_without_a_report_repair_round() {
    use crate::runtime::{GoalSpec, GoalStatus};
    let home = unique_temp_dir("goal-missing-home");
    let cwd = unique_temp_dir("goal-missing-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let handle = runtime.handle();
    handle
        .confirmed_test_goal(GoalSpec::new("do work"))
        .unwrap();
    assert!(matches!(
        handle.stream_turn("other"),
        Err(crate::runtime::RuntimeError::Busy { .. })
    ));
    handle.start_goal_driver_and_run().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, GoalStatus::Blocked);
    assert_eq!(result.iterations.len(), 1);
    assert!(result.iterations.iter().all(|i| i.finished));
    assert_eq!(
        runtime
            .ctx
            .stores
            .store
            .lines()
            .await
            .unwrap()
            .iter()
            .filter(|m| m["_mink"]["source"] == "goal_continuation")
            .count(),
        1
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_natural_end_continues_then_completes_with_independent_review() {
    use crate::runtime::*;
    let (runtime, home, cwd, backend) = goal_runtime("goal-complete", true, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    let goal = handle.wait_goal().await.unwrap();
    assert_eq!(goal.status, GoalStatus::Completed, "{goal:?}");
    assert_eq!(
        goal.completion_basis,
        Some(GoalCompletionBasis::AgentReviewed)
    );
    assert_eq!(goal.iterations.len(), 2);
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 6);
    assert_eq!(goal.usage.request_count, 6);
    assert_eq!(goal.usage.unreported_request_count, 6);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_review_failure_can_resume_same_candidate_without_reexecuting() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-review-retry-home");
    let cwd = unique_temp_dir("goal-review-retry-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(RetryableReviewBackend {
        review_calls: Default::default(),
        execution_calls: Default::default(),
    });
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();

    let blocked = tokio::time::timeout(std::time::Duration::from_secs(15), handle.wait_goal())
        .await
        .expect("first review should finish with an unavailable result")
        .unwrap();
    assert_eq!(blocked.status, GoalStatus::Blocked, "{blocked:?}");
    assert_eq!(blocked.iterations.len(), 1);
    assert_eq!(blocked.remaining_turns(), 0);
    assert!(
        blocked
            .review_failure_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("independent review backend unavailable"))
    );
    let candidate = blocked.review_candidate.clone().expect("retry candidate");

    let resumed = handle.resume_goal(&blocked.id, blocked.revision).unwrap();
    assert_eq!(resumed.phase, GoalPhase::Verifying);
    assert_eq!(resumed.review_candidate.as_ref(), Some(&candidate));
    assert_eq!(resumed.review_failure_reason, None);
    let completed = tokio::time::timeout(std::time::Duration::from_secs(15), handle.wait_goal())
        .await
        .expect("the resumed audit should complete")
        .unwrap();
    assert_eq!(completed.status, GoalStatus::Completed, "{completed:?}");
    assert_eq!(
        completed.iterations.len(),
        1,
        "resume must not run the agent again"
    );
    assert_eq!(
        backend
            .execution_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "review retry must reuse the settled execution round"
    );
    assert!(
        backend
            .review_calls
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 2
    );

    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_host_corrects_unread_evidence_after_repeated_submission_rejections() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-review-format-continuation-home");
    let cwd = unique_temp_dir("goal-review-format-continuation-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(HostSubmissionRecoveryBackend {
        review_calls: Default::default(),
        execution_calls: Default::default(),
        recover_after_rejections: true,
        end_without_conclusion: false,
    });
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();

    let completed = tokio::time::timeout(std::time::Duration::from_secs(15), handle.wait_goal())
        .await
        .expect("host correction should complete within the shared deadline")
        .unwrap();
    assert_eq!(completed.status, GoalStatus::Completed, "{completed:?}");
    assert_eq!(
        completed.iterations.len(),
        1,
        "review must not rerun execution"
    );
    assert_eq!(completed.reviews.len(), 1);
    assert_eq!(
        backend
            .execution_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "host correction does not replay the execution round"
    );
    assert_eq!(
        backend
            .review_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        5,
        "four unread-evidence rejections are followed by a corrected host submission"
    );

    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_host_reports_terminal_reason_after_malformed_tool_calls() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-review-format-failure-home");
    let cwd = unique_temp_dir("goal-review-format-failure-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(HostSubmissionRecoveryBackend {
        review_calls: Default::default(),
        execution_calls: Default::default(),
        recover_after_rejections: false,
        end_without_conclusion: false,
    });
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();

    let blocked = tokio::time::timeout(std::time::Duration::from_secs(15), handle.wait_goal())
        .await
        .expect("the bounded host turn should finish")
        .unwrap();
    assert_eq!(blocked.status, GoalStatus::Blocked, "{blocked:?}");
    let reason = blocked.review_failure_reason.as_deref().unwrap_or_default();
    assert!(reason.contains("format_recovery_exhausted"), "{reason}");
    assert!(reason.contains("invalid host tool-call JSON"), "{reason}");
    assert!(!reason.contains("Ok(Err("), "{reason}");
    assert_eq!(
        backend
            .review_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        4,
        "malformed host calls still obey the configured format window"
    );

    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_host_continues_after_prose_end_without_submitting_conclusion() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-review-empty-end-home");
    let cwd = unique_temp_dir("goal-review-empty-end-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(HostSubmissionRecoveryBackend {
        review_calls: Default::default(),
        execution_calls: Default::default(),
        recover_after_rejections: true,
        end_without_conclusion: true,
    });
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();

    let completed = tokio::time::timeout(std::time::Duration::from_secs(15), handle.wait_goal())
        .await
        .expect("host correction should complete within the shared deadline")
        .unwrap();
    assert_eq!(completed.status, GoalStatus::Completed, "{completed:?}");
    assert_eq!(
        completed.iterations.len(),
        1,
        "review must not rerun execution"
    );
    assert_eq!(completed.reviews.len(), 1);
    assert_eq!(
        backend
            .execution_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "host correction does not replay the execution round"
    );
    assert_eq!(
        backend
            .review_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the host's prose end is followed by an internal reminder and a tool submission"
    );

    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_human_approval_checks_evidence_versions() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-human", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Human;
    handle.confirmed_test_goal(spec).unwrap();
    let goal = handle.wait_goal().await.unwrap();
    assert_eq!(goal.status, GoalStatus::Blocked);
    std::fs::write(
        cwd.join("proof.txt"),
        "changed since completion application",
    )
    .unwrap();
    assert!(
        handle
            .approve_goal(&goal.id, goal.revision, vec!["reviewed".into()])
            .is_err(),
        "approval must reject changed evidence"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}
#[tokio::test]
async fn goal_verifier_failure_stalls_and_missing_verifier_never_downgrades() {
    use crate::runtime::*;
    for missing in [false, true] {
        let verifier = if missing {
            None
        } else {
            Some(Arc::new(TestGoalVerifier {
                status: GoalVerificationStatus::Fail,
                mutate: false,
            }) as Arc<dyn GoalVerifier>)
        };
        let (runtime, home, cwd, backend) = goal_runtime("goal-verifier", false, verifier).await;
        let handle = runtime.handle();
        handle.start_goal_driver_and_run().unwrap();
        let mut spec = GoalSpec::new("produce proof");
        spec.acceptance = GoalAcceptance::Verifier {
            id: "test-check-v1".into(),
        };
        handle.confirmed_test_goal(spec).unwrap();
        let goal = handle.wait_goal().await.unwrap();
        assert_eq!(goal.status, GoalStatus::Blocked);
        assert_eq!(goal.completion_basis, None);
        assert_eq!(goal.iterations.len(), if missing { 0 } else { 3 });
        assert_eq!(
            backend.calls.load(std::sync::atomic::Ordering::SeqCst),
            if missing { 0 } else { 9 }
        );
        runtime.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(cwd);
    }
}

#[tokio::test]
async fn goal_review_can_read_its_own_spilled_contract_artifact() {
    use crate::runtime::*;
    struct SpillReader(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl LlmBackend for SpillReader {
        fn name(&self) -> &str {
            "review-spill-reader"
        }
        async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
            let events = if mock_review_parts(&request).is_none() {
                natural_goal_end()
            } else {
                let stage = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if stage < 2 {
                    let (name, input) = if stage == 0 {
                        ("GoalRead", serde_json::json!({}))
                    } else {
                        let serialized = serde_json::to_string(request.messages.last().unwrap())?;
                        let suffix = serialized
                            .split("artifact://")
                            .nth(1)
                            .expect("GoalRead must spill");
                        let id: String = suffix
                            .chars()
                            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                            .collect();
                        (
                            "Read",
                            serde_json::json!({"path":format!("artifact://{id}")}),
                        )
                    };
                    vec![
                        Ok(LlmEvent::ToolCall(crate::protocol::ToolCallEvent {
                            id: format!("spill-{stage}"),
                            name: name.into(),
                            input_json: input,
                            fields: Default::default(),
                            parse_error: None,
                            raw_arguments_digest: None,
                        })),
                        Ok(LlmEvent::Stop(LlmStopEvent {
                            reason: "tool_use".into(),
                        })),
                    ]
                } else {
                    mock_goal_review(&request, "satisfied", "")
                }
            };
            Ok(LlmResponseStream {
                events: Box::pin(futures::stream::iter(events)),
                attempt_count: 1,
            })
        }
    }
    let home = unique_temp_dir("review-spill-home");
    let cwd = unique_temp_dir("review-spill-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_tool_result_max_bytes(512)
            .with_llm_backend(Arc::new(SpillReader(Default::default()))),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut spec = crate::runtime::GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle.wait_goal().await.unwrap();
    let artifacts: Vec<_> = goal
        .evidence_versions
        .iter()
        .filter(|(r, _)| r.starts_with("artifact://"))
        .collect();
    let imported = artifacts
        .iter()
        .map(|(r, _)| {
            runtime
                .ctx
                .stores
                .artifacts
                .read_text(r.strip_prefix("artifact://").unwrap())
        })
        .collect::<anyhow::Result<Vec<_>>>();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(
        goal.status,
        crate::runtime::GoalStatus::Completed,
        "{:?}",
        goal.reason
    );
    assert_eq!(artifacts.len(), 1);
    assert!(imported.unwrap()[0].contains("produce proof"));
    assert_eq!(goal.iterations.len(), 1);
}

#[tokio::test]
async fn goal_known_checker_result_closes_marker_when_evidence_cannot_be_hashed() {
    use crate::runtime::*;
    struct MakeDirectory;
    #[async_trait::async_trait]
    impl GoalVerifier for MakeDirectory {
        fn id(&self) -> &str {
            "directory-result"
        }
        async fn verify(
            &self,
            context: GoalVerificationContext,
        ) -> anyhow::Result<GoalVerification> {
            let path = context.cwd.join("proof.txt");
            std::fs::remove_file(&path)?;
            std::fs::create_dir(path)?;
            Ok(GoalVerification {
                status: GoalVerificationStatus::Pass,
                reason: "known checker result".into(),
                evidence: vec![],
            })
        }
    }
    let (runtime, home, cwd, _) = goal_runtime(
        "goal-checker-evidence-error",
        false,
        Some(Arc::new(MakeDirectory)),
    )
    .await;
    let handle = runtime.handle();
    handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle.wait_goal().await.unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(goal.status, GoalStatus::Blocked);
    assert!(
        goal.pending_effects.is_empty(),
        "the checker returned a known result"
    );
    assert_eq!(goal.checker_records.len(), 1);
    assert_eq!(goal.checker_records[0].status, GoalCriterionStatus::Pass);
    assert!(goal.checker_passed_candidate.is_none());
}

#[tokio::test]
async fn goal_recovery_reaudits_known_checker_changes_without_replaying_checker() {
    use crate::runtime::*;
    struct MustNotReplay;
    #[async_trait::async_trait]
    impl GoalVerifier for MustNotReplay {
        fn id(&self) -> &str {
            "known-checker"
        }
        async fn verify(&self, _: GoalVerificationContext) -> anyhow::Result<GoalVerification> {
            anyhow::bail!("a durably known unchanged checker result must not be replayed")
        }
    }
    let verifier = Arc::new(MustNotReplay);
    let (runtime, home, cwd, _) =
        goal_runtime("goal-checker-recovery", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();
    let sid = handle.session_info().session_id.clone();
    let executing = runtime
        .ctx
        .stores
        .goal_store
        .begin_iteration("committed-execution", 0)
        .unwrap();
    let old_hash = handle.evidence_version("proof.txt").unwrap();
    std::fs::write(cwd.join("proof.txt"), "known checker refreshed proof").unwrap();
    let new_hash = handle.evidence_version("proof.txt").unwrap();
    let candidate = GoalReviewCandidate {
        generation: 1,
        goal_id: executing.id.clone(),
        contract_revision: executing.contract_revision,
        iteration: 1,
        conversation_seq: 0,
        reviewer_id: format!(
            "goal-review:{}:{}",
            runtime.ctx.active_model.lock().unwrap().actual,
            runtime.ctx.tools.tool_surface.fingerprint()
        ),
    };
    let review: GoalReview = serde_json::from_value(serde_json::json!({
        "candidate":candidate,"verdict":"satisfied",
        "original_request":{"status":"pass","reason":"checked","evidence":["proof.txt"]},
        "criteria":[{"criterion_id":"criterion-1","status":"pass","reason":"checked","evidence":["proof.txt"]}],
        "constraints":[],"summary":"checked","next_step":"","blocker":null
    })).unwrap();
    runtime
        .ctx
        .stores
        .goal_store
        .change(&executing.id, executing.revision, |g| {
            g.iterations[0].finished = true;
            g.iterations[0].outcome_status = Some(TurnStatus::Ok);
            g.phase = GoalPhase::Verifying;
            g.review_generation = 1;
            g.review_candidate = Some(candidate.clone());
            g.reviews.push(review);
            g.evidence_versions = vec![("proof.txt".into(), old_hash)];
            g.checker_records.push(GoalCheckerRecord {
                candidate: candidate.clone(),
                checker_id: "known-checker".into(),
                status: GoalCriterionStatus::Pass,
                reason: "known result".into(),
                evidence: vec!["proof.txt".into()],
            });
            g.checker_passed_candidate = Some(candidate);
            g.checker_evidence_versions = vec![("proof.txt".into(), Some(new_hash))];
            Ok(())
        })
        .unwrap();
    runtime.shutdown().await.unwrap();
    drop(handle);
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let resumed = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_session(SessionPolicy::Resume(sid))
            .with_llm_backend(backend.clone())
            .with_goal_verifier(verifier),
    )
    .await
    .unwrap();
    let h = resumed.handle();
    h.start_goal_driver_and_run().unwrap();
    let goal = h.wait_goal().await.unwrap();
    resumed.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(goal.status, GoalStatus::Completed, "{:?}", goal.reason);
    assert_eq!(goal.completion_basis, Some(GoalCompletionBasis::Verified));
    assert_eq!(goal.iterations.len(), 1);
    assert_eq!(goal.reviews.len(), 2);
    assert_eq!(goal.checker_records.len(), 1);
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(goal.pending_effects.is_empty());
}

#[tokio::test]
async fn goal_last_round_reaudits_checker_changes_without_reexecuting() {
    use crate::runtime::*;
    struct MutateOnce(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl GoalVerifier for MutateOnce {
        fn id(&self) -> &str {
            "mutate-once"
        }
        async fn verify(
            &self,
            context: GoalVerificationContext,
        ) -> anyhow::Result<GoalVerification> {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                std::fs::write(context.cwd.join("proof.txt"), "proof refreshed by checker")?;
            }
            Ok(GoalVerification {
                status: GoalVerificationStatus::Pass,
                reason: "checked refreshed proof".into(),
                evidence: vec!["proof.txt".into()],
            })
        }
    }
    let verifier = Arc::new(MutateOnce(Default::default()));
    let (runtime, home, cwd, backend) =
        goal_runtime("goal-reaudit-last", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle.wait_goal().await.unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(goal.status, GoalStatus::Completed);
    assert_eq!(goal.completion_basis, Some(GoalCompletionBasis::Verified));
    assert_eq!(goal.iterations.len(), 1);
    assert_eq!(goal.reviews.len(), 2);
    assert_ne!(goal.reviews[0].candidate, goal.reviews[1].candidate);
    assert_eq!(verifier.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(goal.checker_records.len(), 1);
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 5);
    assert_eq!(goal.usage.request_count, 5);
    assert!(goal.pending_effects.is_empty());
}

#[tokio::test]
async fn goal_verifier_pass_and_evidence_mutation_have_distinct_completion() {
    use crate::runtime::*;
    for mutate in [false, true] {
        let verifier = Arc::new(TestGoalVerifier {
            status: GoalVerificationStatus::Pass,
            mutate,
        });
        let (runtime, home, cwd, _) =
            goal_runtime("goal-proof-version", false, Some(verifier)).await;
        let handle = runtime.handle();
        let mut spec = GoalSpec::new("produce proof");
        spec.acceptance = GoalAcceptance::Verifier {
            id: "test-check-v1".into(),
        };
        spec.limits.max_turns = 2;
        handle.confirmed_test_goal(spec).unwrap();
        handle.start_goal_driver_and_run().unwrap();
        let goal = handle.wait_goal().await.unwrap();
        assert_eq!(goal.status, GoalStatus::Completed);
        assert_eq!(goal.completion_basis, Some(GoalCompletionBasis::Verified));
        assert_eq!(goal.iterations.len(), 1);
        assert_eq!(goal.reviews.len(), if mutate { 2 } else { 1 });
        assert_eq!(goal.checker_records.len(), 1);
        runtime.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(cwd);
    }
}

#[tokio::test]
async fn goal_human_approval_rejects_expired_deadline() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-human-deadline", false, None).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Human;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let waiting = handle.wait_goal().await.unwrap();
    let mut limits = waiting.spec.limits.clone();
    limits.deadline_unix_secs = 1;
    let expired = handle
        .update_goal_limits(&waiting.id, waiting.revision, limits)
        .unwrap();
    let approved = handle.approve_goal(&expired.id, expired.revision, vec!["checked proof".into()]);
    let current = handle.goal_snapshot().unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert!(
        approved.is_err(),
        "expired approval was accepted: {approved:?}"
    );
    assert_eq!(
        current, expired,
        "a rejected approval must not publish success"
    );
}

#[tokio::test]
async fn goal_human_wait_owns_evidence_and_approval_has_human_basis() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-human-pass", false, None).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Human;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle.wait_goal().await.unwrap();
    assert!(matches!(
        handle.stream_turn("change evidence"),
        Err(RuntimeError::Busy { .. })
    ));
    let completed = handle
        .approve_goal(&goal.id, goal.revision, vec!["reviewed proof".into()])
        .unwrap();
    assert_eq!(
        completed.completion_basis,
        Some(GoalCompletionBasis::HumanApproved)
    );
    assert!(
        handle
            .approve_goal(&goal.id, goal.revision, vec!["duplicate".into()])
            .is_err()
    );
    assert!(
        handle
            .resume_goal(&completed.id, completed.revision)
            .is_err()
    );
    handle
        .clear_goal(&completed.id, completed.revision)
        .unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}
#[tokio::test]
async fn goal_pause_during_verifier_keeps_execution_owned_until_checker_exits() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("goal-held-check", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    let g = handle.goal_snapshot().unwrap();
    handle.pause_goal(&g.id, g.revision).unwrap();
    let paused = handle.wait_goal().await.unwrap();
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(paused.completion_basis, None);
    assert!(
        matches!(
            handle.stream_turn("independent work"),
            Err(RuntimeError::Busy { .. })
        ),
        "checker still owns permit"
    );
    assert!(handle.resume_goal(&paused.id, paused.revision).is_err());
    verifier.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while handle.active_turn_id().is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let g = handle.goal_snapshot().unwrap();
    handle.cancel_goal(&g.id, g.revision).unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// Audit8 F2: a paused run settles immediately, but the host lease must
/// outlive the checker that is still exiting with the turn gate. The lease is
/// dropped only after the gate goes idle, and no caller blocks meanwhile.
#[tokio::test]
async fn audit8_paused_checker_keeps_the_host_lease_until_it_exits() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("audit8-lease-held", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let alive = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle
        .confirmed_test_goal_with_lease(spec, Box::new(CountingLease::new(&alive)))
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 1);
    let g = handle.goal_snapshot().unwrap();
    handle.pause_goal(&g.id, g.revision).unwrap();
    // The pause settles the run and `wait_goal` returns while the checker is
    // still exiting: the lease must not be released by that return.
    let paused = handle.wait_goal().await.unwrap();
    assert_eq!(paused.status, GoalStatus::Paused);
    assert!(
        handle.active_turn_id().is_some(),
        "the checker still owns the turn gate"
    );
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "host capacity must stay occupied while the checker exits"
    );
    verifier.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while handle.active_turn_id().is_some()
            || alive.load(std::sync::atomic::Ordering::SeqCst) > 0
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the lease must be released once the checker exits");
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_checker_deadline_cannot_release_ownership_or_complete() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("goal-check-deadline", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    spec.limits.deadline_unix_secs = crate::session::goal::goal_now() + 4;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    verifier.entered.notified().await;
    let exhausted = tokio::time::timeout(std::time::Duration::from_secs(8), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exhausted.status, GoalStatus::BudgetExhausted);
    assert_eq!(exhausted.completion_basis, None);
    assert!(matches!(
        handle.stream_turn("independent"),
        Err(RuntimeError::Busy { .. })
    ));
    verifier.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while handle.active_turn_id().is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_limit_change_during_verification_preserves_valid_acceptance() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("goal-check-budget", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    verifier.entered.notified().await;
    let goal = handle.goal_snapshot().unwrap();
    let mut limits = goal.spec.limits.clone();
    limits.max_turns = 30;
    handle
        .update_goal_limits(&goal.id, goal.revision, limits)
        .unwrap();
    verifier.release.notify_one();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.status, GoalStatus::Completed);
    assert_eq!(
        completed.completion_basis,
        Some(GoalCompletionBasis::Verified)
    );
    assert_eq!(completed.spec.limits.max_turns, 30);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

// ── 第三轮审计回归（源自 goal-audit-2026-10-09/round-3）──

#[tokio::test]
async fn audit3_verifier_obeys_shortened_deadline() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("audit3-deadline", false, Some(verifier.clone())).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    let g = handle.goal_snapshot().unwrap();
    let mut limits = g.spec.limits.clone();
    limits.deadline_unix_secs = crate::session::goal::goal_now() + 1;
    handle
        .update_goal_limits(&g.id, g.revision, limits)
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let observed = handle.goal_snapshot().unwrap().status;
    verifier.release.notify_one();
    handle.wait_goal().await.unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(
        observed,
        GoalStatus::BudgetExhausted,
        "shortened deadline must apply during verification"
    );
}
#[tokio::test]
async fn audit3_failed_sink_resets_turn_activity() {
    use crate::runtime::*;
    let home = unique_temp_dir("audit3-sink-home");
    let cwd = unique_temp_dir("audit3-sink-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(PendingEstablishBackend::default());
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_llm_backend(backend)
            .with_goal_event_sink(Arc::new(Audit3FailingSink)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let activity = handle.subscribe_goal_turns();
    handle
        .confirmed_test_goal(GoalSpec::new("test sink"))
        .unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .unwrap();
    let goal = handle.goal_snapshot().unwrap();
    let active = *activity.borrow();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    // The sink failure faults the driver, and the failed status it managed to
    // publish stays authoritative.
    assert!(
        result.is_err(),
        "a sink failure must surface as a runtime fault: {result:?}"
    );
    assert_eq!(goal.status, GoalStatus::Failed);
    assert!(!active, "failed goal left turn_active=true");
}

#[tokio::test]
async fn audit3_human_approval_requires_nonblank_evidence() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("audit3-blank", false, None).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Human;
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let g = handle.wait_goal().await.unwrap();
    let approved = handle.approve_goal(&g.id, g.revision, vec![String::new()]);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert!(
        approved.is_err(),
        "blank approval evidence was accepted: {approved:?}"
    );
}
#[tokio::test]
async fn audit3_old_iteration_cannot_bill_replacement_goal() {
    use crate::runtime::*;
    let home = unique_temp_dir("audit3-replace-home");
    let cwd = unique_temp_dir("audit3-replace-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "proof").unwrap();
    let sink = Arc::new(Audit3HoldFinalSink {
        entered: Default::default(),
        release: Default::default(),
        held: Default::default(),
    });
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_llm_backend(backend)
            .with_goal_event_sink(sink.clone()),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle
        .confirmed_test_goal(GoalSpec::new("old goal"))
        .unwrap();
    handle.start_goal_driver_and_run().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), sink.entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while handle.active_turn_id().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let old = handle.goal_snapshot().unwrap();
    let old_bill = old.iterations[0].billing_turn_id.clone().unwrap();
    handle.cancel_goal(&old.id, old.revision).unwrap();
    assert!(matches!(
        handle.draft_goal(GoalSpec::new("new goal"), 3600),
        Err(RuntimeError::Busy { .. })
    ));
    // Inject a replacement to exercise stale callbacks even though the public
    // protocol refuses replacement until the previous run has exited.
    let new = handle
        .goal_ctx
        .stores
        .goal_store
        .create(
            GoalSpec::new("new goal"),
            crate::runtime::goal::goal_environment(&cwd, &handle.goal_ctx.tools.tool_surface),
        )
        .unwrap();
    sink.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    let contaminated = outcome
        .usage_records
        .iter()
        .any(|r| r.billing_turn_id == old_bill);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(outcome.id, new.id);
    assert!(
        !contaminated,
        "replacement inherited prior goal billing; repair_used={}, iterations={}",
        outcome.report_repair_used,
        outcome.iterations.len()
    );
}

#[tokio::test]
async fn audit3_recover_normal_end_after_idle_publication() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("audit3-recovery", false, None).await;
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("produce proof");
    spec.limits.max_turns = 1;
    handle.confirmed_test_goal(spec).unwrap();
    let sid = handle.session_info().session_id.clone();
    // Exact persisted boundary in drive_goal: result committed, phase=Idle,
    // normal completion must recover into independent verification.
    runtime
        .ctx
        .stores
        .goal_store
        .update_for_test(|s| {
            s.phase = GoalPhase::Idle;
            s.iterations.push(crate::session::goal::GoalIteration {
                iteration: 1,
                turn_id: "crash-boundary".into(),
                billing_turn_id: None,
                finished: true,
                conversation_start: 0,
                outcome_status: Some(TurnStatus::Ok),
                outcome_error: None,
            });
            Ok(())
        })
        .unwrap();
    runtime.shutdown().await.unwrap();
    drop(handle);
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let resumed = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_session(SessionPolicy::Resume(sid))
            .with_llm_backend(backend),
    )
    .await
    .unwrap();
    let h = resumed.handle();
    h.start_goal_driver_and_run().unwrap();
    let result = h.wait_goal().await.unwrap();
    resumed.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(
        result.status,
        GoalStatus::Completed,
        "recovery lost pending completion report: {:?}",
        result.reason
    );
}
#[tokio::test]
async fn audit4_old_sink_error_cannot_fail_replacement() {
    use crate::runtime::*;
    let home = unique_temp_dir("audit3-replace-home");
    let cwd = unique_temp_dir("audit3-replace-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "proof").unwrap();
    let sink = Arc::new(Audit4HoldFinalErrorSink {
        entered: Default::default(),
        release: Default::default(),
        held: Default::default(),
    });
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_llm_backend(backend)
            .with_goal_event_sink(sink.clone()),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle
        .confirmed_test_goal(GoalSpec::new("old goal"))
        .unwrap();
    handle.start_goal_driver_and_run().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), sink.entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while handle.active_turn_id().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let old = handle.goal_snapshot().unwrap();
    let old_bill = old.iterations[0].billing_turn_id.clone().unwrap();
    handle.cancel_goal(&old.id, old.revision).unwrap();
    assert!(matches!(
        handle.draft_goal(GoalSpec::new("new goal"), 3600),
        Err(RuntimeError::Busy { .. })
    ));
    // Inject a replacement to exercise stale callbacks even though the public
    // protocol refuses replacement until the previous run has exited.
    let new = handle
        .goal_ctx
        .stores
        .goal_store
        .create(
            GoalSpec::new("new goal"),
            crate::runtime::goal::goal_environment(&cwd, &handle.goal_ctx.tools.tool_surface),
        )
        .unwrap();
    sink.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    let contaminated = outcome
        .usage_records
        .iter()
        .any(|r| r.billing_turn_id == old_bill);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(outcome.id, new.id);
    assert!(!contaminated);
    assert_eq!(
        outcome.status,
        GoalStatus::Completed,
        "old sink error affected replacement: {:?}",
        outcome.reason
    );
}

#[tokio::test]
async fn audit4_empty_approval_is_rejected() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("audit4-empty", false, None).await;
    let h = runtime.handle();
    let mut spec = GoalSpec::new("proof");
    spec.acceptance = GoalAcceptance::Human;
    h.confirmed_test_goal(spec).unwrap();
    h.start_goal_driver_and_run().unwrap();
    let g = h.wait_goal().await.unwrap();
    let result = h.approve_goal(&g.id, g.revision, vec![]);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert!(result.is_err(), "empty approval accepted");
}
#[cfg_attr(
    not(feature = "slow-tests"),
    ignore = "62s wall-clock verifier timeout regression; run with --features slow-tests"
)]
#[tokio::test]
async fn audit4_verifier_attempt_exceeds_old_sixty_second_cap() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) = goal_runtime("audit4-cap", false, Some(verifier.clone())).await;
    let h = runtime.handle();
    let mut spec = GoalSpec::new("proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    h.confirmed_test_goal(spec).unwrap();
    h.start_goal_driver_and_run().unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(62)).await;
    let observed = h.goal_snapshot().unwrap().status;
    verifier.release.notify_one();
    h.wait_goal().await.unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    assert_eq!(
        observed,
        GoalStatus::Active,
        "verification must not inherit the old sixty-second cap"
    );
}

#[tokio::test]
async fn goal_human_draft_requires_explicit_selection_with_multiple_checkers() {
    struct NamedChecker(&'static str);
    #[async_trait::async_trait]
    impl crate::runtime::GoalVerifier for NamedChecker {
        fn id(&self) -> &str {
            self.0
        }
        async fn verify(
            &self,
            _: crate::runtime::GoalVerificationContext,
        ) -> anyhow::Result<crate::runtime::GoalVerification> {
            anyhow::bail!("not executed in a draft")
        }
    }
    let home = unique_temp_dir("goal-multiple-checkers-home");
    let cwd = unique_temp_dir("goal-multiple-checkers-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_llm_backend(Arc::new(GoalReportBackend {
                calls: Default::default(),
                continue_first: false,
            }))
            .with_goal_verifier(Arc::new(NamedChecker("first")))
            .with_goal_verifier(Arc::new(NamedChecker("second"))),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let mut spec = GoalSpec::new("ship");
    spec.acceptance = crate::runtime::GoalAcceptance::Human;
    assert!(handle.draft_goal(spec.clone(), 3600).is_err());
    assert!(handle.goal_snapshot().is_none());
    spec.checker_id = Some("first".into());
    assert_eq!(
        handle
            .draft_goal(spec, 3600)
            .unwrap()
            .spec
            .checker_id
            .as_deref(),
        Some("first")
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_draft_discussion_is_internal_and_does_not_register_a_human_request() {
    let (runtime, home, cwd, backend) = goal_runtime("goal-draft-discussion", false, None).await;
    let handle = runtime.handle();
    let draft = handle
        .request_goal_draft(GoalSpec::new("ship the parser"), 3600)
        .unwrap();
    assert!(!draft.draft_ready);
    assert!(handle.confirm_goal(&draft.id, draft.revision).is_err());
    assert!(
        handle
            .generate_goal_draft_at(&draft.id, draft.revision + 1, "stale feedback")
            .await
            .is_err()
    );
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        handle.goal_draft_snapshot().unwrap().revision,
        draft.revision
    );
    let discussed = handle
        .generate_goal_draft("also cover invalid input")
        .await
        .unwrap();
    assert_eq!(discussed.status, crate::runtime::GoalStatus::Draft);
    assert!(discussed.iterations.is_empty());
    assert!(handle.input_inbox.entries().is_empty());
    assert!(handle.input_inbox.active_turn().is_none());
    let reader = crate::runtime::session::SessionReader::new(
        handle.session_info().conversation_path.parent().unwrap(),
    );
    let rows = reader.conversation().unwrap();
    let row = rows
        .iter()
        .find(|row| row.pointer("/_mink/source").and_then(|v| v.as_str()) == Some("goal_draft"))
        .unwrap();
    assert_eq!(
        row.pointer("/_mink/internal"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(
        row.pointer("/_mink/goal_message/kind")
            .and_then(|v| v.as_str()),
        Some("draft")
    );
    assert_eq!(
        row.pointer("/_mink/goal_message/feedback")
            .and_then(|v| v.as_str()),
        Some("also cover invalid input")
    );
    assert_eq!(rows.iter().filter(|row| row["role"] == "user").count(), 1);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_draft_rejects_mutation_and_requires_host_confirmation() {
    let (runtime, home, cwd, backend) = goal_runtime("goal-draft-authority", false, None).await;
    let handle = runtime.handle();
    assert!(handle.create_goal(GoalSpec::new("ship")).is_err());
    let draft = handle.draft_goal(GoalSpec::new("ship"), 3600).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    tokio::task::yield_now().await;
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let tools = crate::tools::runner::ToolRunner::new(Arc::new(crate::context::ToolContext::from(
        handle.goal_ctx.as_ref(),
    )));
    let call = crate::protocol::ToolCallEvent {
        id: "forbidden-write".into(),
        name: "Write".into(),
        input_json: serde_json::json!({"path":"forbidden.txt","content":"bad"}),
        fields: Default::default(),
        parse_error: None,
        raw_arguments_digest: None,
    };
    let result = tools.execute_all(vec![call]).await.unwrap();
    assert!(!result[0].succeeded());
    assert!(!cwd.join("forbidden.txt").exists());
    assert!(handle.confirm_goal(&draft.id, draft.revision + 1).is_err());
    handle.confirm_goal(&draft.id, draft.revision).unwrap();
    let completed = handle.wait_goal().await.unwrap();
    assert_eq!(
        completed.completion_basis,
        Some(crate::runtime::GoalCompletionBasis::AgentReviewed)
    );
    assert!(completed.report.is_none());
    assert_eq!(completed.iterations.len(), 1);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_review_rejects_unread_forged_or_changed_evidence() {
    use crate::session::goal::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-review-evidence", false, None).await;
    let handle = runtime.handle();
    let goal = handle
        .goal_ctx
        .stores
        .goal_store
        .create(
            GoalSpec::new("ship"),
            crate::runtime::goal::goal_environment(&cwd, &handle.goal_ctx.tools.tool_surface),
        )
        .unwrap();
    let candidate = GoalReviewCandidate {
        generation: 0,
        goal_id: goal.id.clone(),
        contract_revision: goal.contract_revision,
        iteration: 1,
        conversation_seq: 0,
        reviewer_id: "test-reviewer".into(),
    };
    let goal = handle
        .goal_ctx
        .stores
        .goal_store
        .change(&goal.id, goal.revision, |g| {
            g.review_candidate = Some(candidate.clone());
            g.spec.constraints = vec!["only the requested output".into()];
            Ok(())
        })
        .unwrap();
    let session = Arc::new(crate::runtime::goal::review::GoalReviewSession::new(
        goal.clone(),
        handle.goal_ctx.stores.goal_store.clone(),
        handle.goal_ctx.stores.usage.clone(),
    ));
    let mut ctx = crate::context::ToolContext::from(handle.goal_ctx.as_ref());
    ctx.goal_adjudication = Some(session.clone());
    let evidence_ctx = ctx.goal_review_tool_context().unwrap();
    let review = GoalReview {
        candidate,
        verdict: GoalReviewVerdict::Satisfied,
        report: None,
        original_request: Some(GoalRequestReview {
            status: GoalCriterionStatus::Pass,
            reason: "checked original user request".into(),
            evidence: vec!["proof.txt".into()],
        }),
        criteria: vec![GoalCriterionReview {
            criterion_id: "criterion-1".into(),
            status: GoalCriterionStatus::Pass,
            reason: "verified".into(),
            evidence: vec!["proof.txt".into()],
        }],
        constraints: vec![GoalConstraintReview {
            constraint: "only the requested output".into(),
            status: GoalCriterionStatus::Pass,
            reason: "inspected output".into(),
            evidence: vec!["proof.txt".into()],
        }],
        summary: "complete".into(),
        next_step: String::new(),
        blocker: None,
    };
    assert!(
        session.submit(review.clone(), &evidence_ctx).is_err(),
        "file existence is not inspection"
    );
    let call = crate::protocol::ToolCallEvent {
        id: "read".into(),
        name: "Read".into(),
        input_json: serde_json::json!({"path":"proof.txt"}),
        fields: Default::default(),
        parse_error: None,
        raw_arguments_digest: None,
    };
    session
        .finish_read(session.capture_read(&call, &evidence_ctx).unwrap().unwrap())
        .unwrap();
    let host_goal_before_submission = handle.goal_snapshot().unwrap();
    for bad in [
        {
            let mut r = review.clone();
            r.constraints[0].evidence.clear();
            r
        },
        {
            let mut r = review.clone();
            r.original_request.as_mut().unwrap().reason.clear();
            r
        },
        {
            let mut r = review.clone();
            r.criteria.clear();
            r
        },
        {
            let mut r = review.clone();
            r.criteria.push(r.criteria[0].clone());
            r
        },
        {
            let mut r = review.clone();
            r.criteria[0].criterion_id = "invented".into();
            r
        },
        {
            let mut r = review.clone();
            r.criteria[0].evidence.clear();
            r
        },
        {
            let mut r = review.clone();
            r.candidate.contract_revision += 1;
            r
        },
    ] {
        assert!(session.submit(bad, &evidence_ctx).is_err());
    }
    assert!(
        serde_json::from_value::<GoalReview>(serde_json::to_value(&review).unwrap()).is_ok(),
        "the host tool protocol accepts its explicit verdict"
    );
    session.submit(review.clone(), &evidence_ctx).unwrap();
    assert_eq!(
        handle.goal_snapshot().unwrap(),
        host_goal_before_submission,
        "recording the host conclusion must not bypass the runtime commit"
    );
    assert!(session.submitted_conclusion().is_some());
    std::fs::write(cwd.join("proof.txt"), "changed after inspection").unwrap();
    assert!(session.submit(review, &evidence_ctx).is_err());
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}
