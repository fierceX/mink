use super::*;

#[test]
fn new_contract_requires_independent_review_protocol() {
    let spec = serde_json::to_value(GoalSpec::new("produce proof")).unwrap();
    assert_eq!(spec["version"], GOAL_FORMAT_VERSION);
    assert_eq!(spec["acceptance"]["mode"], "agent_reviewed");
    assert_eq!(spec["original_request"], "produce proof");
    assert_eq!(spec["criteria"][0]["id"], "criterion-1");
}

fn store() -> (PathBuf, GoalStore) {
    let dir = std::env::temp_dir().join(format!(
        "mink-goal-{}-{:?}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let store = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
    (dir, store)
}

#[test]
fn draft_confirmation_is_cas_and_starts_the_deadline_only_once() {
    let (dir, store) = store();
    let mut spec = GoalSpec::new("ship");
    spec.limits.deadline_unix_secs = 1;
    let draft = store.draft(spec, "env".into(), 120).unwrap();
    assert_eq!(draft.status, GoalStatus::Draft);
    assert!(store.begin_iteration("turn", 0).is_err());
    assert!(
        store
            .confirm(&draft.id, draft.revision + 1, "env".into())
            .is_err()
    );
    let confirmed = store
        .confirm(&draft.id, draft.revision, "env".into())
        .unwrap();
    assert_eq!(confirmed.status, GoalStatus::Active);
    assert!(confirmed.spec.limits.deadline_unix_secs >= goal_now() + 119);
    assert!(
        store
            .confirm(&draft.id, draft.revision, "env".into())
            .is_err()
    );
    let restored = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
    assert_eq!(restored.snapshot(), Some(confirmed));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn inactive_goals_cannot_stay_in_verifying_phase() {
    let (dir, store) = store();
    let mut goal = store.create(GoalSpec::new("ship"), "env".into()).unwrap();
    goal.status = GoalStatus::Cancelled;
    goal.phase = GoalPhase::Verifying;
    goal.review_stage = Some(GoalReviewStage::Inspecting);
    goal.normalize_phase();
    assert_eq!(goal.phase, GoalPhase::Idle);
    assert_eq!(goal.review_stage, None);

    // Human acceptance deliberately parks in Verifying until the host
    // decision arrives; normalization must not erase that gate.
    goal.status = GoalStatus::Blocked;
    goal.phase = GoalPhase::Verifying;
    goal.reason = Some("waiting for human acceptance".into());
    goal.normalize_phase();
    assert_eq!(goal.phase, GoalPhase::Verifying);

    // Pausing during review must keep Verifying so resume retries the
    // review rather than starting a fresh execution round.
    goal.status = GoalStatus::Paused;
    goal.phase = GoalPhase::Verifying;
    goal.normalize_phase();
    assert_eq!(goal.phase, GoalPhase::Verifying);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn loading_a_legacy_cancelled_goal_repairs_its_phase() {
    let (dir, store) = store();
    let mut goal = store.create(GoalSpec::new("ship"), "env".into()).unwrap();
    goal.status = GoalStatus::Cancelled;
    goal.phase = GoalPhase::Verifying;
    goal.review_stage = Some(GoalReviewStage::Screening);
    let state = State {
        version: GOAL_FORMAT_VERSION,
        current: Some(goal),
        history: Vec::new(),
    };
    std::fs::write(dir.join("goal.json"), serde_json::to_vec(&state).unwrap()).unwrap();
    let _ = std::fs::remove_file(dir.join("goal-transaction.json"));
    let loaded = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
    let snapshot = loaded.snapshot().unwrap();
    assert_eq!(snapshot.status, GoalStatus::Cancelled);
    assert_eq!(snapshot.phase, GoalPhase::Idle);
    assert_eq!(snapshot.review_stage, None);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn pending_request_is_never_confirmable_before_a_proposal_is_saved() {
    let (dir, store) = store();
    let pending = store
        .draft_pending(GoalSpec::new("ship"), "env".into(), 120)
        .unwrap();
    assert!(!pending.draft_ready);
    assert!(
        store
            .confirm(&pending.id, pending.revision, "env".into())
            .is_err()
    );
    let spec = &pending.spec;
    let ready = store
        .replace_draft(
            &pending.id,
            pending.revision,
            GoalDraftSpec {
                objective: spec.objective.clone(),
                scope: spec.scope.clone(),
                constraints: spec.constraints.clone(),
                subgoals: spec.subgoals.clone(),
                criteria: spec.criteria.clone(),
                suggested_actions: Vec::new(),
            },
        )
        .unwrap();
    assert!(ready.draft_ready);
    assert!(
        store
            .confirm(&pending.id, pending.revision, "env".into())
            .is_err()
    );
    assert!(
        store
            .confirm(&ready.id, ready.revision, "env".into())
            .is_ok()
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Hosts render the budget from this one derivation instead of each
/// recomputing remaining turns, deadline and blocked threshold themselves.
#[test]
fn budget_view_derives_the_stopping_contract() {
    let (dir, store) = store();
    let created = store
        .create(GoalSpec::new("bounded"), "env".into())
        .unwrap();
    let budget = created.budget();
    assert_eq!(budget.max_turns, 20);
    assert_eq!(budget.turns_used, 0);
    assert_eq!(budget.remaining_turns, 20);
    assert_eq!(budget.blocked_after_consecutive_rounds, 3);
    assert_eq!(budget.blocked_rounds, 0);
    assert_eq!(budget.blocker_rounds, 0);
    assert_eq!(budget.soft_token_limit, None);
    assert!(budget.remaining_secs > 0 && budget.remaining_secs <= 3600);
    assert_eq!(
        budget.deadline_unix_secs,
        created.spec.limits.deadline_unix_secs
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn budget_defaults_match_the_documented_contract() {
    let limits = default_limits();
    assert_eq!(limits.max_turns, 20);
    assert_eq!(limits.blocked_after_consecutive_rounds, 3);
    assert!(limits.deadline_unix_secs >= goal_now() + 3600 - 5);
}

/// The model-visible goal state is the only goal state the model ever reads,
/// so every rendering must be readable text — and the post-compaction
/// checkpoint must not carry per-round facts, or it would invalidate the
/// provider prefix cache on every round.
#[test]
fn contract_checkpoint_is_readable_and_free_of_round_facts() {
    let (dir, store) = store();
    let mut spec = GoalSpec::new("ship the parser");
    spec.set_acceptance_criteria(vec!["parses fixtures".into()]);
    spec.constraints = vec!["no network".into()];
    spec.acceptance = GoalAcceptance::Verifier { id: "v1".into() };
    let created = store.create(spec, "env".into()).unwrap();
    let text = state_text(&created, GoalStateKind::Contract, 4096);

    assert!(text.starts_with("<current-goal>"), "{text}");
    assert!(text.ends_with("</current-goal>"), "{text}");
    assert!(text.contains("Objective: ship the parser"), "{text}");
    assert!(text.contains("parses fixtures"), "{text}");
    assert!(text.contains("no network"), "{text}");
    assert!(text.contains("verifier"), "{text}");
    assert!(text.contains("Deadline (unix):"), "{text}");
    assert!(
        text.contains("Stop for the same blocker only after 3 consecutive reviewed rounds"),
        "{text}"
    );
    for forbidden in [
        "Round:",
        "Remaining turns",
        "GoalReport iteration",
        "Phase:",
        "Decision:",
        "Completed rounds",
    ] {
        assert!(
            !text.contains(forbidden),
            "the contract checkpoint must stay byte-stable across rounds, found {forbidden:?}:\n{text}"
        );
    }
    // Only per-round state moves; the checkpoint bytes must not.
    let mut progressed = created.clone();
    progressed.iterations.push(GoalIteration {
        iteration: 1,
        turn_id: "turn-1".into(),
        billing_turn_id: None,
        finished: true,
        conversation_start: 0,
        outcome_status: None,
        outcome_error: None,
    });
    progressed.phase = GoalPhase::Executing;
    progressed.reason = Some("round one finished".into());
    progressed.blocked_rounds = 2;
    progressed.revision += 1;
    assert_eq!(
        state_text(&progressed, GoalStateKind::Contract, 4096),
        text,
        "the contract checkpoint changed with per-round state"
    );
    // No Rust syntax may leak into the model's context.
    for leak in [
        "Some(",
        "None",
        "Ok(",
        "GoalSpec",
        "GoalLimits",
        "{:?",
        "[\"",
    ] {
        assert!(!text.contains(leak), "projection leaked {leak:?}:\n{text}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// The per-round facts travel with the appended round prompt: exactly one
/// number may be read as the report iteration, and it is the admitted one.
#[test]
fn round_prompt_states_one_round_number_and_the_host_limits() {
    let (dir, store) = store();
    let mut spec = GoalSpec::new("ship the parser");
    spec.set_acceptance_criteria(vec!["parses fixtures".into()]);
    spec.constraints = vec!["no network".into()];
    let created = store.create(spec, "env".into()).unwrap();
    let mut admitted = created.clone();
    admitted.iterations.push(GoalIteration {
        iteration: 1,
        turn_id: "turn-1".into(),
        billing_turn_id: None,
        finished: false,
        conversation_start: 0,
        outcome_status: None,
        outcome_error: None,
    });
    admitted.phase = GoalPhase::Executing;
    admitted.reason = Some("initial iteration".into());

    let text = round_prompt(&admitted, 4096);

    assert!(text.starts_with("<current-goal>"), "{text}");
    assert!(text.contains("Round: 1/20"), "{text}");
    assert!(text.contains("Remaining turns: 19"), "{text}");
    assert!(!text.contains("GoalReport iteration:"), "{text}");
    assert!(text.contains("Phase: executing"), "{text}");
    assert!(text.contains("Decision: initial iteration"), "{text}");
    assert!(text.contains("Deadline (unix):"), "{text}");
    assert!(
        text.contains("Stop for the same blocker only after 3 consecutive reviewed rounds"),
        "{text}"
    );
    // The mandate stays outside the facts block and never advertises a
    // neighbouring round.
    assert!(text.contains("</current-goal>"), "{text}");
    assert!(text.contains("end this turn naturally"), "{text}");
    assert!(!text.contains("Round: 0"), "{text}");
    assert!(!text.contains("iteration: 0"), "{text}");
    let _ = std::fs::remove_dir_all(dir);
}

/// Terminal facts replace the removed projection as the model-visible end
/// signal.
#[test]
fn ended_facts_report_the_terminal_status_and_basis() {
    let (dir, store) = store();
    let goal = store
        .create(GoalSpec::new("ship the parser"), "env".into())
        .unwrap();
    let mut finished = goal.clone();
    finished.status = GoalStatus::Completed;
    finished.phase = GoalPhase::Idle;
    finished.completion_basis = Some(GoalCompletionBasis::Verified);
    finished.reason = Some("checker passed".into());

    let text = state_text(&finished, GoalStateKind::Ended, 4096);
    assert!(text.contains("Status: completed"), "{text}");
    assert!(text.contains("(basis: verified)"), "{text}");
    assert!(text.contains("Decision: checker passed"), "{text}");
    assert!(text.contains("The goal has ended"), "{text}");
    assert!(!text.contains("Round:"), "{text}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn goal_conflicts_stale_revision_and_limits_are_durable() {
    let (dir, store) = store();
    let g = store.create(GoalSpec::new("build"), "env".into()).unwrap();
    assert_eq!(g.remaining_turns(), 20);
    assert!(
        store
            .create(GoalSpec::new("replace"), "env".into())
            .is_err()
    );
    assert!(store.change(&g.id, 0, |_| Ok(())).is_err());
    store
        .change(&g.id, g.revision, |g| {
            g.status = GoalStatus::Paused;
            Ok(())
        })
        .unwrap();
    let restored = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
    assert_eq!(restored.snapshot().unwrap().status, GoalStatus::Paused);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unknown_side_effect_blocks_recovery() {
    let (dir, store) = store();
    let g = store.create(GoalSpec::new("build"), "env".into()).unwrap();
    store
        .change(&g.id, g.revision, |g| {
            g.phase = GoalPhase::Executing;
            Ok(())
        })
        .unwrap();
    store.mark_effect("call").unwrap();
    store.recover("env", &[], 0).unwrap();
    assert_eq!(store.snapshot().unwrap().status, GoalStatus::Blocked);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn journal_recovers_snapshot_failure_and_fault_latches() {
    let (dir, store) = store();
    let goal = store.create(GoalSpec::new("build"), "env".into()).unwrap();
    std::fs::remove_file(dir.join("goal.json")).unwrap();
    std::fs::create_dir(dir.join("goal.json")).unwrap();
    assert!(
        store
            .change(&goal.id, goal.revision, |g| {
                g.status = GoalStatus::Paused;
                Ok(())
            })
            .is_err()
    );
    assert!(store.fault.check().is_err());
    assert_eq!(store.snapshot().unwrap().status, GoalStatus::Active);
    std::fs::remove_dir(dir.join("goal.json")).unwrap();
    let restored = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
    assert_eq!(restored.snapshot().unwrap().status, GoalStatus::Paused);
    assert_eq!(restored.snapshot().unwrap().revision, goal.revision + 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn formal_result_resolves_effect_but_synthetic_repair_does_not() {
    for (kind, paused) in (0..4).flat_map(|kind| [false, true].map(|paused| (kind, paused))) {
        let formal = kind == 1;
        let (dir, store) = store();
        let goal = store.create(GoalSpec::new("build"), "env".into()).unwrap();
        store
            .change(&goal.id, goal.revision, |g| {
                g.phase = GoalPhase::Executing;
                Ok(())
            })
            .unwrap();
        store.mark_effect("write").unwrap();
        if paused {
            store
                .update(|g| {
                    g.status = GoalStatus::Paused;
                    Ok(())
                })
                .unwrap();
        }
        let mut block = serde_json::json!({"type":"tool_result","tool_use_id":"write","content":"result unavailable"});
        if formal {
            block["_mink"] = serde_json::json!({"status":{"state":"succeeded"}});
        }
        if kind == 3 {
            block["_mink"] = serde_json::json!({"status":{"state":"failed","reason":"unknown"},"execution_outcome":"unknown"});
        }
        if kind == 2 {
            block["_mink"] = serde_json::json!({"status":{"state":"unknown"}});
        }
        store
            .recover(
                "env",
                &[serde_json::json!({"seq":1,"role":"user","content":[block]})],
                1,
            )
            .unwrap();
        let recovered = store.snapshot().unwrap();
        assert_eq!(
            recovered.status,
            if formal && paused {
                GoalStatus::Paused
            } else if formal {
                GoalStatus::Active
            } else {
                GoalStatus::Blocked
            }
        );
        assert_eq!(recovered.pending_effects.is_empty(), formal);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn recovery_resolves_effects_by_physical_sequence_after_blank_lines() {
    let (dir, store) = store();
    let goal = store.create(GoalSpec::new("build"), "env".into()).unwrap();
    store
        .change(&goal.id, goal.revision, |goal| {
            goal.phase = GoalPhase::Executing;
            goal.iterations.push(GoalIteration {
                iteration: 1,
                turn_id: "turn-1".into(),
                billing_turn_id: None,
                finished: false,
                conversation_start: 2,
                outcome_status: None,
                outcome_error: None,
            });
            Ok(())
        })
        .unwrap();
    store.mark_effect("write").unwrap();
    let history = [serde_json::json!({
        "seq": 3,
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": "write",
            "_mink": {"status": {"state": "succeeded"}},
        }],
    })];

    store.recover("env", &history, 3).unwrap();

    let recovered = store.snapshot().unwrap();
    assert!(recovered.pending_effects.is_empty());
    assert_eq!(recovered.status, GoalStatus::Active);
    assert_eq!(recovered.phase, GoalPhase::Idle);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn checkpoint_is_bounded_and_terminal_facts_are_published_once() {
    let (dir, store) = store();
    let mut spec = GoalSpec::new("x".repeat(4000));
    spec.set_acceptance_criteria(vec!["proof".into()]);
    let goal = store.create(spec, "env".into()).unwrap();
    let checkpoint = store.checkpoint(256).unwrap().unwrap();
    assert_eq!(checkpoint["_mink"]["goal_state_kind"], "contract");
    assert!(checkpoint["content"].as_str().unwrap().chars().count() < 400);
    assert_eq!(store.snapshot().unwrap().spec.objective.len(), 4000);

    let history = crate::session::store::ConversationStore::new(dir.join("conversation.jsonl"));
    history.ensure().await.unwrap();
    assert!(
        !store.append_ended_row(&history, 4096).await.unwrap(),
        "a live goal has no terminal row"
    );
    store
        .change(&goal.id, goal.revision, |g| {
            g.status = GoalStatus::Completed;
            g.completion_basis = Some(GoalCompletionBasis::ModelReported);
            g.reason = Some("model-reported completion".into());
            Ok(())
        })
        .unwrap();
    assert!(store.append_ended_row(&history, 4096).await.unwrap());
    assert!(
        !store.append_ended_row(&history, 4096).await.unwrap(),
        "the terminal row is published exactly once"
    );
    let lines = history.lines().await.unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["_mink"]["goal_state_kind"], "ended");
    assert_eq!(lines[0]["_mink"]["internal"], true);
    assert!(
        store.checkpoint(4096).unwrap().is_none(),
        "a terminal goal leaves the request prefix alone"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn crash_usage_reconciliation_counts_all_kinds_once_and_unknown_attempts() {
    use crate::session::usage::{TokenUsage, UsageKind, UsageRecord, UsageStatus};
    let (dir, store) = store();
    let goal = store.create(GoalSpec::new("work"), "env".into()).unwrap();
    store
        .change(&goal.id, goal.revision, |g| {
            g.iterations.push(GoalIteration {
                iteration: 1,
                turn_id: "goal-turn".into(),
                billing_turn_id: Some("billing".into()),
                finished: false,
                conversation_start: 0,
                outcome_status: None,
                outcome_error: None,
            });
            Ok(())
        })
        .unwrap();
    let records: Vec<_> = [UsageKind::Agent, UsageKind::Compaction, UsageKind::SubAgent]
        .into_iter()
        .enumerate()
        .map(|(n, kind)| UsageRecord {
            version: 1,
            billing_turn_id: "billing".into(),
            request_id: format!("request-{n}"),
            kind,
            origin_session_id: "session".into(),
            model: "mock".into(),
            attempt_count: 2,
            status: if n == 2 {
                UsageStatus::Unreported
            } else {
                UsageStatus::Reported
            },
            tokens: if n == 2 {
                None
            } else {
                Some(TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    ..Default::default()
                })
            },
            cost_nano_cny: None,
            reason: None,
            completed_at: "mock".into(),
        })
        .collect();
    store.reconcile_usage(&records).unwrap();
    let revision = store.snapshot().unwrap().revision;
    store.reconcile_usage(&records).unwrap();
    let goal = store.snapshot().unwrap();
    assert_eq!(goal.revision, revision);
    assert_eq!(goal.usage.request_count, 3);
    assert_eq!(goal.usage.attempt_count, 6);
    assert_eq!(goal.usage.unreported_request_count, 1);
    assert_eq!(goal.usage.tokens.input_tokens, 20);
    let restored = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
    assert_eq!(restored.snapshot().unwrap().usage, goal.usage);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn settled_failure_is_not_restarted_and_report_is_idempotent() {
    let (dir, store) = store();
    let goal = store.create(GoalSpec::new("work"), "env".into()).unwrap();
    store
        .change(&goal.id, goal.revision, |g| {
            g.phase = GoalPhase::Executing;
            Ok(())
        })
        .unwrap();
    let report = GoalReport {
        goal_id: goal.id.clone(),
        iteration: 0,
        kind: GoalReportKind::Continue,
        summary: "progress".into(),
        next_step: "next".into(),
        criteria: vec![],
        evidence: vec![],
    };
    store
        .update(|g| {
            g.report = Some(report.clone());
            Ok(())
        })
        .unwrap();
    let revision = store.snapshot().unwrap().revision;
    assert_eq!(store.snapshot().unwrap().report, Some(report));
    assert_eq!(store.snapshot().unwrap().revision, revision);
    store
        .update(|g| {
            g.iterations.push(GoalIteration {
                iteration: 1,
                turn_id: "turn".into(),
                billing_turn_id: None,
                finished: true,
                conversation_start: 0,
                outcome_status: Some(crate::runtime::TurnStatus::Failed),
                outcome_error: Some("format_recovery_exhausted".into()),
            });
            Ok(())
        })
        .unwrap();
    store.recover("env", &[], 0).unwrap();
    let recovered = store.snapshot().unwrap();
    assert_eq!(recovered.status, GoalStatus::Failed);
    assert_eq!(
        recovered.reason.as_deref(),
        Some("format_recovery_exhausted")
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// A round belongs to the goal that admitted it. Late work that names the
/// replaced goal must be refused (revision/identity) or a no-op (stale side
/// effect), never written onto whatever is current now.
#[test]
fn late_work_for_a_replaced_goal_never_touches_the_replacement() {
    let (dir, store) = store();
    let first = store.create(GoalSpec::new("first"), "env".into()).unwrap();
    let running = store.begin_iteration("turn-a", 0).unwrap();
    assert_eq!(running.phase, GoalPhase::Executing);
    store.mark_effect("call-a").unwrap();
    assert_eq!(
        store.snapshot().unwrap().pending_effects,
        vec!["call-a".to_string()]
    );

    // The first goal is cancelled and replaced before its round settled.
    let executing = store.snapshot().unwrap();
    let cancelled = store
        .change(&first.id, executing.revision, |g| {
            g.status = GoalStatus::Cancelled;
            Ok(())
        })
        .unwrap();
    assert_eq!(cancelled.status, GoalStatus::Cancelled);
    let replacement = store.create(GoalSpec::new("second"), "env".into()).unwrap();

    // Every identity-bound entry point refuses the replaced goal instead of
    // applying its work to the replacement.
    assert!(
        store
            .transition_if_current(
                &first.id,
                GoalTransition::FailRun {
                    reason: "stale decision".into(),
                },
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(store.snapshot().unwrap().revision, replacement.revision);
    assert!(
        store
            .change(&replacement.id, cancelled.revision, |_| Ok(()))
            .is_err(),
        "a stale revision of the replacement must be refused by CAS"
    );
    assert_eq!(store.snapshot().unwrap().revision, replacement.revision);

    // A stale side-effect marker for the dead run is a no-op: the
    // replacement must not be blocked on effects it never dispatched, and
    // closing the old ids changes nothing either.
    store.mark_effect("late-call").unwrap();
    store.close_effects(&["call-a".into()]).unwrap();
    assert!(store.snapshot().unwrap().pending_effects.is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn pause_if_active_is_noop_when_inactive_and_clears_review_stage() {
    let (dir, store) = store();
    let goal = store.create(GoalSpec::new("work"), "env".into()).unwrap();
    store
        .change(&goal.id, goal.revision, |g| {
            g.phase = GoalPhase::Verifying;
            g.review_stage = Some(GoalReviewStage::Inspecting);
            Ok(())
        })
        .unwrap();
    let paused = store
        .pause_if_active("interrupted")
        .unwrap()
        .expect("active goal should pause");
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(paused.review_stage, None);
    let rev = paused.revision;
    assert!(
        store.pause_if_active("again").unwrap().is_none(),
        "inactive goal must not be mutated or re-paused"
    );
    assert_eq!(store.snapshot().unwrap().revision, rev);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn publish_omits_redundant_journal_and_load_cleans_legacy_journal() {
    let (dir, store) = store();
    let goal = store.create(GoalSpec::new("ship"), "env".into()).unwrap();
    assert!(
        !dir.join("goal-transaction.json").exists(),
        "single-file publish_state must not leave a redundant goal-transaction.json"
    );

    let legacy_state = State {
        version: GOAL_FORMAT_VERSION,
        current: Some(goal.clone()),
        history: Vec::new(),
    };
    std::fs::write(
        dir.join("goal-transaction.json"),
        serde_json::to_vec(&legacy_state).unwrap(),
    )
    .unwrap();
    let reloaded = GoalStore::load(&dir, PersistenceFault::default()).unwrap();
    assert_eq!(reloaded.snapshot().unwrap().id, goal.id);
    assert!(
        !dir.join("goal-transaction.json").exists(),
        "loading a legacy goal-transaction.json must replay and delete it"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn abort_unstarted_iteration_rolls_back_idle_phase_and_entry() {
    let (dir, store) = store();
    let goal = store
        .create(GoalSpec::new("rollback"), "env".into())
        .unwrap();
    let started = store.begin_iteration("turn-x", 0).unwrap();
    assert_eq!(started.phase, GoalPhase::Executing);
    assert_eq!(started.iterations.len(), 1);

    // Mismatched turn id is ignored cleanly.
    store
        .abort_unstarted_iteration(&goal.id, 1, "wrong-turn")
        .unwrap();
    assert_eq!(store.snapshot().unwrap().phase, GoalPhase::Executing);

    // Matching unstarted iteration rolls back phase to Idle and pops entry.
    store
        .abort_unstarted_iteration(&goal.id, 1, "turn-x")
        .unwrap();
    let rolled_back = store.snapshot().unwrap();
    assert_eq!(rolled_back.phase, GoalPhase::Idle);
    assert!(rolled_back.iterations.is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}
