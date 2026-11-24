use super::*;

/// Why a goal change was refused.
///
/// A rejected command (stale revision, invalid transition) says nothing about
/// the run: it must not be treated as a stop-intent failure. Only a `Publish`
/// failure means the intent could not be stored, which is what stop commands
/// have to act on.
pub(crate) enum GoalChangeError {
    /// The command itself was rejected; the run keeps its lifecycle.
    Rejected(anyhow::Error),
    /// The new state could not be published; the authoritative snapshot is
    /// unchanged.
    Publish(anyhow::Error),
}

impl GoalChangeError {
    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Rejected(error) | Self::Publish(error) => error,
        }
    }
}

/// Named mutations accepted by the goal store. The persisted snapshot remains
/// the compatibility DTO; transition rules live here so callers cannot assemble
/// arbitrary lifecycle combinations with a closure.
pub(crate) enum GoalTransition {
    CancelDraft,
    Pause,
    Cancel,
    Resume {
        environment: String,
    },
    UpdateLimits {
        limits: GoalLimits,
    },
    ReconcileEffects {
        evidence: String,
    },
    Clear,
    Approve {
        evidence: Vec<String>,
        evidence_versions: Vec<(String, String)>,
        checker_evidence_valid: bool,
        now_unix_secs: u64,
    },
    RestartReview {
        expected: GoalReviewCandidate,
        carry_checker: bool,
    },
    SetDraftReady {
        ready: bool,
    },
    MarkReviewAmbiguous,
    InvalidateReviewReport,
    EscalateRepeatedContinuation {
        conversation_seq: usize,
    },
    SetReviewStage {
        expected: GoalReviewCandidate,
        stage: GoalReviewStage,
    },
    PrepareReview {
        conversation_seq: usize,
        reviewer_id: String,
    },
    FinishContinuation {
        expected: GoalReviewCandidate,
    },
    CommitContinuation {
        proposal: GoalContinueProposal,
        report: String,
        fingerprint: String,
        conversation_seq: usize,
    },
    PublishReviewReport {
        report: GoalReviewReport,
    },
    CommitReview {
        review: GoalReview,
        evidence_versions: Vec<(String, String)>,
        expected_conversation_seq: usize,
        blocker_fingerprint: Option<String>,
    },
    CompleteReview {
        expected: GoalReviewCandidate,
        checker: bool,
    },
    FailRun {
        reason: String,
    },
    MarkVerifierUnavailable {
        verifier_id: String,
    },
    ExhaustBudget {
        reason: String,
    },
    BeginChecker {
        expected: GoalReviewCandidate,
        effect_id: String,
    },
    RecordCheckerResult {
        effect_id: String,
        record: crate::session::goal_protocol::GoalCheckerRecord,
        trusted: bool,
        evidence_versions: Vec<(String, Option<String>)>,
    },
    ReviewFailure {
        expected: GoalReviewCandidate,
        reason: String,
        outcome: GoalReviewFailureStatus,
    },
    VerificationFailure {
        reason: String,
        fingerprint: String,
        blocked: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoalReviewFailureStatus {
    Blocked,
    BudgetExhausted,
}

impl GoalTransition {
    pub(super) fn apply_to(self, goal: &mut GoalSnapshot) -> Result<()> {
        match self {
            Self::CancelDraft
            | Self::Pause
            | Self::Cancel
            | Self::Resume { .. }
            | Self::UpdateLimits { .. }
            | Self::ReconcileEffects { .. }
            | Self::Clear
            | Self::Approve { .. }
            | Self::SetDraftReady { .. } => self.apply_control(goal),
            Self::RestartReview { .. }
            | Self::MarkReviewAmbiguous
            | Self::InvalidateReviewReport
            | Self::EscalateRepeatedContinuation { .. }
            | Self::SetReviewStage { .. }
            | Self::PrepareReview { .. }
            | Self::FinishContinuation { .. }
            | Self::CommitContinuation { .. }
            | Self::PublishReviewReport { .. }
            | Self::CommitReview { .. }
            | Self::CompleteReview { .. } => self.apply_review_lifecycle(goal),
            Self::FailRun { .. }
            | Self::MarkVerifierUnavailable { .. }
            | Self::ExhaustBudget { .. }
            | Self::BeginChecker { .. }
            | Self::RecordCheckerResult { .. }
            | Self::ReviewFailure { .. }
            | Self::VerificationFailure { .. } => self.apply_failure_and_checker(goal),
        }
    }

    fn apply_control(self, goal: &mut GoalSnapshot) -> Result<()> {
        match self {
            Self::CancelDraft => {
                ensure!(goal.status == GoalStatus::Draft, "goal is not a draft");
                goal.status = GoalStatus::Cancelled;
            }
            Self::Pause => {
                ensure!(
                    !goal.status.is_terminal() && goal.status != GoalStatus::Draft,
                    "confirm or cancel a draft before pausing"
                );
                goal.status = GoalStatus::Paused;
                goal.review_stage = None;
                goal.reason = Some("paused by user".into());
            }
            Self::Cancel => {
                ensure!(!goal.status.is_terminal(), "goal has ended");
                goal.status = GoalStatus::Cancelled;
                goal.review_stage = None;
                goal.reason = Some("cancelled by user".into());
            }
            Self::Resume { environment } => {
                ensure!(
                    goal.status.can_resume(),
                    "goal cannot resume from status {:?}; drafts require confirmation and completed/cancelled goals cannot resume. Original reason: {}",
                    goal.status,
                    goal.reason.as_deref().unwrap_or("not recorded")
                );
                ensure!(
                    goal.pending_effects.is_empty(),
                    "side effect result unknown; explicit reconciliation required"
                );
                let retry_review =
                    goal.review_failure_reason.is_some() || goal.phase == GoalPhase::Verifying;
                ensure!(
                    if retry_review {
                        goal.review_budget_reason().is_none()
                    } else {
                        goal.budget_reason().is_none()
                    },
                    "adjust exhausted goal limits before resume"
                );
                ensure!(
                    goal.environment == environment,
                    "goal environment changed; cancel and recreate with the new permissions"
                );
                ensure!(
                    !goal.needs_confirmation && goal.draft_duration_secs.is_none(),
                    "goal requires explicit contract confirmation"
                );
                goal.status = GoalStatus::Active;
                goal.hidden = false;
                goal.reason = None;
                goal.phase = if retry_review {
                    GoalPhase::Verifying
                } else {
                    GoalPhase::Idle
                };
                goal.review_failure_reason = None;
                goal.review_failure_stage = None;
            }
            Self::UpdateLimits { limits } => {
                limits.validate()?;
                ensure!(
                    goal.status.can_adjust_limits(),
                    "completed or cancelled goal limits cannot change"
                );
                if goal.status == GoalStatus::Draft && goal.draft_duration_secs.is_some() {
                    goal.draft_duration_secs =
                        Some(limits.deadline_unix_secs.saturating_sub(goal_now()).max(1));
                }
                goal.spec.limits = limits;
            }
            Self::ReconcileEffects { evidence } => {
                ensure!(
                    matches!(
                        goal.status,
                        GoalStatus::Blocked | GoalStatus::Draft | GoalStatus::Failed
                    ) && !goal.pending_effects.is_empty(),
                    "no unknown effects to reconcile"
                );
                ensure!(
                    !evidence.trim().is_empty(),
                    "reconciliation requires human evidence"
                );
                goal.pending_effects.clear();
                goal.reason = Some(format!("human effect reconciliation: {evidence}"));
            }
            Self::Clear => {
                ensure!(goal.status.is_terminal(), "only ended goals can be cleared");
                goal.hidden = true;
            }
            Self::Approve {
                evidence,
                evidence_versions,
                checker_evidence_valid,
                now_unix_secs,
            } => {
                ensure!(
                    !evidence.is_empty() && evidence.iter().all(|item| !item.trim().is_empty()),
                    "human approval requires evidence"
                );
                ensure!(
                    goal.spec.acceptance == GoalAcceptance::Human
                        && goal.status == GoalStatus::Blocked
                        && goal.phase == GoalPhase::Verifying
                        && goal
                            .reviews
                            .last()
                            .is_some_and(|review| review.verdict == GoalReviewVerdict::Satisfied),
                    "no pending human acceptance"
                );
                ensure!(
                    evidence_versions == goal.evidence_versions,
                    "completion evidence changed since human acceptance was requested"
                );
                if goal.spec.checker_id.is_some() {
                    ensure!(
                        checker_evidence_valid,
                        "configured checker has not passed this candidate"
                    );
                }
                ensure!(
                    now_unix_secs < goal.spec.limits.deadline_unix_secs,
                    "goal deadline expired; explicitly adjust the budget before approval"
                );
                goal.status = GoalStatus::Completed;
                goal.phase = GoalPhase::Idle;
                goal.completion_basis = Some(GoalCompletionBasis::HumanApproved);
                goal.reason = Some(evidence.join("\n"));
            }
            Self::SetDraftReady { ready } => {
                ensure!(goal.status == GoalStatus::Draft, "goal is not a draft");
                goal.draft_ready = ready;
            }
            _ => unreachable!("handled by apply_to dispatch"),
        }
        Ok(())
    }

    fn apply_review_lifecycle(self, goal: &mut GoalSnapshot) -> Result<()> {
        match self {
            Self::RestartReview {
                expected,
                carry_checker,
            } => {
                ensure!(
                    goal.status == GoalStatus::Active,
                    "goal is no longer active"
                );
                ensure!(
                    goal.review_candidate.as_ref() == Some(&expected),
                    "stale review candidate"
                );
                ensure!(
                    !carry_checker || goal.checker_passed_candidate.as_ref() == Some(&expected),
                    "checker result no longer matches the review candidate"
                );
                goal.review_generation = goal
                    .review_generation
                    .max(expected.generation)
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("review generation overflow"))?;
                let candidate = GoalReviewCandidate {
                    generation: goal.review_generation,
                    ..expected
                };
                goal.checker_passed_candidate = carry_checker.then(|| candidate.clone());
                if !carry_checker {
                    goal.checker_evidence_versions.clear();
                }
                goal.review_candidate = Some(candidate);
                goal.review_requires_full = true;
                goal.review_escalation_reason = Some("checker or evidence version changed".into());
                goal.review_report = None;
                goal.review_stage = Some(GoalReviewStage::Inspecting);
                goal.evidence_versions.clear();
                goal.review_failure_reason = None;
                goal.review_failure_stage = None;
                goal.phase = GoalPhase::Verifying;
                goal.reason =
                    Some("evidence changed; independently re-audit this execution round".into());
            }
            Self::MarkReviewAmbiguous => {
                ensure!(
                    goal.status == GoalStatus::Active,
                    "goal is no longer active"
                );
                goal.review_requires_full = true;
                goal.review_escalation_reason =
                    Some("execution has no unambiguous final conclusion".into());
            }
            Self::InvalidateReviewReport => {
                ensure!(
                    goal.status == GoalStatus::Active,
                    "goal is no longer active"
                );
                goal.review_report = None;
                goal.review_requires_full = true;
                goal.review_escalation_reason =
                    Some("cached report, evidence or history boundary changed".into());
            }
            Self::EscalateRepeatedContinuation { conversation_seq } => {
                ensure!(
                    goal.status == GoalStatus::Active,
                    "goal is no longer active"
                );
                goal.review_requires_full = true;
                goal.review_escalation_reason = Some(
                    "three consecutive rounds leave the same requirements unfinished; inspect actual progress"
                        .into(),
                );
                goal.review_report = None;
                goal.review_stage = Some(GoalReviewStage::Inspecting);
                goal.acceptance_conversation_seq = conversation_seq;
            }
            Self::SetReviewStage { expected, stage } => {
                ensure!(
                    goal.status == GoalStatus::Active,
                    "goal is no longer active"
                );
                ensure!(
                    goal.review_candidate.as_ref() == Some(&expected),
                    "stale review candidate"
                );
                goal.review_stage = Some(stage);
                if stage == GoalReviewStage::Inspecting {
                    goal.review_requires_full = true;
                    if goal.review_escalation_reason.is_none() {
                        goal.review_escalation_reason =
                            Some("reviewer entered evidence inspection".into());
                    }
                }
            }
            Self::PrepareReview {
                conversation_seq,
                reviewer_id,
            } => {
                ensure!(
                    goal.status == GoalStatus::Active,
                    "goal is no longer active"
                );
                goal.review_generation = goal
                    .review_generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("review generation overflow"))?;
                goal.review_candidate = Some(GoalReviewCandidate {
                    generation: goal.review_generation,
                    goal_id: goal.id.clone(),
                    contract_revision: goal.contract_revision,
                    iteration: goal.iterations.len() as u32,
                    conversation_seq,
                    reviewer_id,
                });
                goal.acceptance_conversation_seq = conversation_seq;
                goal.review_report = None;
                goal.review_stage = Some(if goal.review_requires_full {
                    GoalReviewStage::Inspecting
                } else {
                    GoalReviewStage::Screening
                });
                goal.evidence_versions.clear();
                goal.review_failure_reason = None;
                goal.review_failure_stage = None;
                goal.checker_passed_candidate = None;
                goal.checker_evidence_versions.clear();
            }
            Self::FinishContinuation { expected } => {
                ensure!(
                    goal.status == GoalStatus::Active,
                    "goal is no longer active"
                );
                ensure!(
                    goal.review_candidate.as_ref() == Some(&expected),
                    "continued candidate changed"
                );
                goal.phase = GoalPhase::Idle;
                goal.review_stage = None;
            }
            Self::CommitContinuation {
                proposal,
                report,
                fingerprint,
                conversation_seq,
            } => {
                ensure!(
                    goal.status == GoalStatus::Active,
                    "goal is no longer active"
                );
                ensure!(
                    goal.review_candidate.as_ref() == Some(&proposal.candidate),
                    "stale review candidate"
                );
                ensure!(
                    !goal
                        .reviews
                        .iter()
                        .any(|r| r.candidate == proposal.candidate),
                    "full decision already committed"
                );
                if !goal
                    .continuations
                    .iter()
                    .any(|r| r.proposal.candidate == proposal.candidate)
                {
                    goal.continuation_rounds =
                        if goal.continuation_fingerprint.as_ref() == Some(&fingerprint) {
                            goal.continuation_rounds.saturating_add(1)
                        } else {
                            1
                        };
                    ensure!(
                        goal.continuation_rounds < 3,
                        "repeated continuation requires evidence review"
                    );
                    goal.continuation_fingerprint = Some(fingerprint.clone());
                    goal.continuations.push(GoalContinueRecord {
                        proposal: proposal.clone(),
                        report,
                        fingerprint,
                        consecutive_rounds: goal.continuation_rounds,
                        conversation_seq,
                    });
                }
                goal.reason = Some(format!(
                    "Partial completion; continue: {}\nNext: {}",
                    proposal.summary, proposal.next_step
                ));
                goal.review_failure_reason = None;
                goal.review_failure_stage = None;
                goal.acceptance_conversation_seq = conversation_seq;
                goal.phase = GoalPhase::Idle;
                goal.review_stage = None;
                goal.blocked_rounds = 0;
                goal.blocker_rounds = 0;
                goal.blocker_fingerprint = None;
                if let Some(reason) = goal.budget_reason() {
                    goal.status = GoalStatus::BudgetExhausted;
                    goal.reason = Some(format!(
                        "{}\n{}",
                        goal.reason.as_deref().unwrap_or(""),
                        reason
                    ));
                }
            }
            Self::PublishReviewReport { report } => {
                ensure!(
                    goal.status == GoalStatus::Active,
                    "goal is no longer active"
                );
                ensure!(
                    goal.review_candidate.as_ref() == Some(&report.candidate),
                    "review candidate is no longer active"
                );
                goal.acceptance_conversation_seq = report.conversation_seq;
                goal.review_report = Some(report);
            }
            Self::CommitReview {
                review,
                evidence_versions,
                expected_conversation_seq,
                blocker_fingerprint,
            } => {
                ensure!(
                    goal.status == GoalStatus::Active,
                    "goal is no longer active"
                );
                ensure!(
                    goal.review_candidate.as_ref() == Some(&review.candidate),
                    "review candidate is no longer active"
                );
                ensure!(
                    !goal
                        .continuations
                        .iter()
                        .any(|r| r.proposal.candidate == review.candidate),
                    "continuation already committed for this candidate"
                );
                let newly_recorded = goal
                    .reviews
                    .last()
                    .is_none_or(|r| r.candidate != review.candidate);
                if newly_recorded {
                    goal.reviews.push(review.clone());
                    goal.continuation_rounds = 0;
                    goal.continuation_fingerprint = None;
                }
                goal.review_stage = None;
                goal.evidence_versions = evidence_versions;
                goal.acceptance_conversation_seq = expected_conversation_seq;
                goal.review_failure_reason = None;
                goal.review_failure_stage = None;
                goal.reason = Some(format!("{}\nNext: {}", review.summary, review.next_step));
                if newly_recorded && review.verdict == GoalReviewVerdict::Blocked {
                    let fingerprint = blocker_fingerprint.ok_or_else(|| {
                        anyhow::anyhow!("blocked review is missing its evidence fingerprint")
                    })?;
                    goal.blocked_rounds = goal.blocked_rounds.saturating_add(1);
                    goal.blocker_rounds = if goal.blocker_fingerprint.as_ref() == Some(&fingerprint)
                    {
                        goal.blocker_rounds.saturating_add(1)
                    } else {
                        1
                    };
                    goal.blocker_fingerprint = Some(fingerprint);
                    if goal.blocker_rounds >= goal.spec.limits.blocked_after_consecutive_rounds {
                        goal.status = GoalStatus::Blocked;
                    }
                } else if newly_recorded {
                    goal.blocked_rounds = 0;
                    goal.blocker_rounds = 0;
                    goal.blocker_fingerprint = None;
                }
                if review.verdict != GoalReviewVerdict::Satisfied {
                    goal.phase = GoalPhase::Idle;
                    if review.verdict == GoalReviewVerdict::Inconclusive {
                        goal.review_failure_reason = Some(
                            "independent review returned inconclusive; re-audit required".into(),
                        );
                        goal.review_failure_stage = Some(GoalReviewStage::Adjudicating);
                        goal.reason = Some(
                            "review was inconclusive; the candidate is preserved for an explicit re-audit"
                                .into(),
                        );
                    }
                }
            }
            Self::CompleteReview { expected, checker } => {
                ensure!(
                    goal.status == GoalStatus::Active,
                    "goal is no longer active"
                );
                ensure!(
                    goal.review_candidate.as_ref() == Some(&expected),
                    "review candidate changed before completion"
                );
                if goal_now() >= goal.spec.limits.deadline_unix_secs {
                    goal.status = GoalStatus::BudgetExhausted;
                    goal.phase = GoalPhase::Idle;
                    goal.reason = Some("goal deadline reached during review".into());
                } else {
                    if checker {
                        goal.checker_passed_candidate = Some(expected);
                    }
                    if goal.spec.acceptance == GoalAcceptance::Human {
                        goal.status = GoalStatus::Blocked;
                        goal.reason = Some("waiting for human acceptance".into());
                    } else {
                        goal.status = GoalStatus::Completed;
                        goal.phase = GoalPhase::Idle;
                        goal.completion_basis = Some(if checker {
                            GoalCompletionBasis::Verified
                        } else {
                            GoalCompletionBasis::AgentReviewed
                        });
                    }
                }
            }
            _ => unreachable!("handled by apply_to dispatch"),
        }
        Ok(())
    }

    fn apply_failure_and_checker(self, goal: &mut GoalSnapshot) -> Result<()> {
        match self {
            Self::FailRun { reason } => {
                if goal.status == GoalStatus::Active {
                    goal.status = GoalStatus::Failed;
                    goal.reason = Some(reason);
                }
            }
            Self::MarkVerifierUnavailable { verifier_id } => {
                if goal.status == GoalStatus::Active {
                    goal.status = GoalStatus::Blocked;
                    goal.reason = Some(format!(
                        "goal verifier unavailable or identity changed: {verifier_id}"
                    ));
                }
            }
            Self::ExhaustBudget { reason } => {
                if goal.status == GoalStatus::Active {
                    goal.status = GoalStatus::BudgetExhausted;
                    goal.reason = Some(reason);
                }
            }
            Self::BeginChecker {
                expected,
                effect_id,
            } => {
                ensure!(
                    goal.status == GoalStatus::Active
                        && goal.review_candidate.as_ref() == Some(&expected),
                    "checker superseded"
                );
                if !goal.pending_effects.contains(&effect_id) {
                    goal.pending_effects.push(effect_id);
                }
            }
            Self::RecordCheckerResult {
                effect_id,
                record,
                trusted,
                evidence_versions,
            } => {
                goal.checker_records.push(record.clone());
                if record.status != crate::session::goal_protocol::GoalCriterionStatus::Unknown {
                    goal.pending_effects.retain(|effect| effect != &effect_id);
                }
                if goal.review_candidate.as_ref() == Some(&record.candidate) {
                    goal.checker_passed_candidate = trusted.then(|| record.candidate.clone());
                    goal.checker_evidence_versions = evidence_versions;
                }
            }
            Self::ReviewFailure {
                expected,
                reason,
                outcome,
            } => {
                if goal.status == GoalStatus::Active
                    && goal.review_candidate.as_ref() == Some(&expected)
                {
                    goal.status = match outcome {
                        GoalReviewFailureStatus::Blocked => GoalStatus::Blocked,
                        GoalReviewFailureStatus::BudgetExhausted => GoalStatus::BudgetExhausted,
                    };
                    goal.review_failure_stage = goal.review_stage;
                    goal.review_stage = None;
                    goal.phase = GoalPhase::Idle;
                    goal.review_failure_reason = Some(reason.clone());
                    goal.reason = Some(format!("independent review did not complete: {reason}"));
                }
            }
            Self::VerificationFailure {
                reason,
                fingerprint,
                blocked,
            } => {
                if goal.status == GoalStatus::Active {
                    goal.repeated_failures =
                        if goal.failure_fingerprint.as_ref() == Some(&fingerprint) {
                            goal.repeated_failures.saturating_add(1)
                        } else {
                            1
                        };
                    goal.failure_fingerprint = Some(fingerprint);
                    if blocked || goal.repeated_failures >= 3 {
                        goal.status = GoalStatus::Blocked;
                    }
                    goal.phase = GoalPhase::Idle;
                    goal.evidence_versions.clear();
                    goal.review_candidate = None;
                    goal.review_failure_reason = None;
                    goal.review_failure_stage = None;
                    goal.checker_passed_candidate = None;
                    goal.checker_evidence_versions.clear();
                    goal.reason = Some(format!("goal evidence failure: {reason}"));
                }
            }
            _ => unreachable!("handled by apply_to dispatch"),
        }
        Ok(())
    }
}
