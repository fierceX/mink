//! Goal acceptance, evidence versions and verification failures.

use super::*;

pub(super) fn blocker_evidence_versions(
    references: &[String],
    version: impl Fn(&str) -> Result<String>,
) -> Result<Vec<(&str, String)>> {
    references
        .iter()
        .map(|reference| {
            version(reference)
                .map(|hash| (reference.as_str(), hash))
                .map_err(|error| {
                    anyhow::anyhow!(
                        "blocked evidence {reference} is unavailable ({error:#}); re-read it or remove it from the blocker"
                    )
                })
        })
        .collect()
}

impl AgentRuntimeHandle {
    pub(crate) fn evidence_version(&self, reference: &str) -> Result<String> {
        let cwd = self.goal_ctx.cwd.canonicalize()?;
        let path = if let Some(id) = reference.strip_prefix("artifact://") {
            ensure!(
                !id.contains('/') && !id.contains(".."),
                "invalid artifact reference"
            );
            self.session.artifacts_dir.join(format!("{id}.txt"))
        } else {
            let p = Path::new(reference.strip_prefix("file://").unwrap_or(reference));
            let p = if p.is_absolute() {
                p.to_path_buf()
            } else {
                cwd.join(p)
            }
            .canonicalize()?;
            ensure!(
                p.starts_with(&cwd),
                "goal file evidence is outside the working directory"
            );
            p
        };
        hash_file(&path)
    }
    fn checker_evidence_version(&self, reference: &str) -> Result<Option<String>> {
        match self.evidence_version(reference) {
            Ok(hash) => Ok(Some(hash)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
    pub(crate) fn checker_snapshot_matches(&self, goal: &GoalSnapshot) -> Result<bool> {
        if goal.checker_passed_candidate.is_none()
            || goal.checker_passed_candidate != goal.review_candidate
        {
            return Ok(false);
        }
        for (reference, version) in &goal.checker_evidence_versions {
            if &self.checker_evidence_version(reference)? != version {
                return Ok(false);
            }
        }
        Ok(true)
    }
    /// A checker has a known result, but its writes invalidate the earlier audit.
    /// Publish a distinct candidate without admitting another execution turn.
    fn restart_goal_review(&self, run_id: u64, old: &GoalSnapshot) -> Result<Option<GoalSnapshot>> {
        let mut restarted = None;
        self.apply_if_run_scheduled(run_id, || {
            let Some(current) = self.goal_snapshot() else {
                return Ok(());
            };
            if current.status != GoalStatus::Active
                || current.review_candidate != old.review_candidate
                || self.goal_ctx.interrupt.load(Ordering::SeqCst)
                || self.goal_ctx.cancel.is_cancelled()
            {
                return Ok(());
            }
            let carry_checker = self.checker_snapshot_matches(&current)?;
            let previous = current
                .review_candidate
                .clone()
                .ok_or_else(|| anyhow::anyhow!("missing review candidate"))?;
            let next = self.goal_ctx.goal_store.transition(
                &current.id,
                current.revision,
                GoalTransition::RestartReview {
                    expected: previous,
                    carry_checker,
                },
            )?;
            self.announce_goal(&next);
            restarted = Some(next);
            Ok(())
        })?;
        Ok(restarted)
    }
    /// Apply an acceptance outcome only while `run_id` is still scheduled.
    ///
    /// `apply` runs under the driver admission lock — the same boundary a
    /// stop command holds before latching `Stopping` — so a stop decision and
    /// an acceptance commit linearize: whichever takes the lock first wins, and
    /// a failed stop can never coexist with a `Completed` goal.
    fn apply_if_run_scheduled(
        &self,
        run_id: u64,
        apply: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let _guard = self
            .goal_driver
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !self.run_scheduled(run_id) {
            return Ok(());
        }
        apply()
    }
    pub(crate) async fn accept_goal(&self, run_id: u64, mut goal: GoalSnapshot) -> Result<()> {
        if goal.iterations.last().is_some_and(|i| {
            self.input_inbox.outstanding().iter().any(|r| {
                r.guidance
                    && r.turn_id == i.turn_id
                    && matches!(
                        r.status,
                        crate::runtime::InputStatus::Pending
                            | crate::runtime::InputStatus::Applying
                            | crate::runtime::InputStatus::Unapplied
                    )
            })
        }) {
            self.apply_if_run_scheduled(run_id, || self.verification_failure(&goal.id, "unapplied human guidance invalidates completion; explicitly resume or withdraw the input".into(), Vec::new(), true))?;
            return Ok(());
        }
        let permit = match self.begin_goal_verification(&goal.id, goal.revision) {
            Ok(permit) => permit,
            Err(first) => {
                if !self.run_scheduled(run_id)
                    || self
                        .goal_snapshot()
                        .is_none_or(|g| g.status != GoalStatus::Active)
                    || self.is_closed()
                {
                    return Ok(());
                }
                // A legitimate `/goal budget` edit during acceptance advances the
                // revision. Retry the CAS once against the fresh snapshot instead
                // of failing the whole run (which would need a session reopen).
                let Some(fresh) = self
                    .goal_snapshot()
                    .filter(|g| g.id == goal.id && g.status == GoalStatus::Active)
                else {
                    return Ok(());
                };
                if fresh.revision == goal.revision {
                    return Err(first.into());
                }
                goal = fresh;
                match self.begin_goal_verification(&goal.id, goal.revision) {
                    Ok(permit) => permit,
                    Err(_)
                        if !self.run_scheduled(run_id)
                            || self
                                .goal_snapshot()
                                .is_none_or(|g| g.status != GoalStatus::Active)
                            || self.is_closed() =>
                    {
                        return Ok(());
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        };
        if !self.run_scheduled(run_id) {
            return Ok(());
        }
        let seq = self.goal_ctx.store.committed_len().await?;
        let reviewer_id = format!(
            "goal-review:{}:{}",
            self.goal_ctx
                .active_model
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .actual,
            self.goal_ctx.tool_surface.fingerprint()
        );
        if goal.review_candidate.as_ref().is_none_or(|c| {
            c.goal_id != goal.id
                || c.contract_revision != goal.contract_revision
                || c.iteration as usize != goal.iterations.len()
                || goal.acceptance_conversation_seq != seq
                || c.reviewer_id != reviewer_id
        }) {
            goal = self.goal_ctx.goal_store.transition(
                &goal.id,
                goal.revision,
                GoalTransition::PrepareReview {
                    conversation_seq: seq,
                    reviewer_id,
                },
            )?;
        }
        self.announce_goal(&goal);
        let candidate = goal
            .review_candidate
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing review candidate"))?;
        let handle = self.clone();
        let audit_goal = goal.clone();
        let mut task = tokio::spawn(async move {
            let _permit = permit;
            handle.review_and_accept(run_id, audit_goal).await
        });
        let interrupt = self.goal_ctx.interrupt.clone();
        let stopped = async {
            loop {
                if interrupt.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        };
        tokio::select! {
            result = &mut task => {
                match result {
                    Ok(Ok(())) => {},
                    Ok(Err(error)) => self.apply_if_run_scheduled(run_id, || {
                        if self.goal_ctx.interrupt.load(Ordering::SeqCst)
                            || self.goal_ctx.cancel.is_cancelled()
                        {
                            return Ok(());
                        }
                        self.goal_review_failure(
                            &candidate,
                            format!("goal review failed: {error:#}"),
                            crate::session::goal::GoalReviewFailureStatus::Blocked,
                        )
                    })?,
                    Err(error) => self.apply_if_run_scheduled(run_id, || {
                        if self.goal_ctx.interrupt.load(Ordering::SeqCst)
                            || self.goal_ctx.cancel.is_cancelled()
                        {
                            return Ok(());
                        }
                        self.goal_review_failure(
                            &candidate,
                            format!("independent review task failed: {error}"),
                            crate::session::goal::GoalReviewFailureStatus::Blocked,
                        )
                    })?,
                }
            }
            _ = self.wait_verification_deadline(&goal) => {
                self.goal_ctx.interrupt.store(true, Ordering::SeqCst);
                self.apply_if_run_scheduled(run_id, || {
                    if self.goal_snapshot().is_some_and(|g| goal_now() >= g.spec.limits.deadline_unix_secs) {
                        self.goal_review_failure(
                            &candidate,
                            "goal deadline reached during review; execution ownership retained until exit".into(),
                            crate::session::goal::GoalReviewFailureStatus::BudgetExhausted,
                        )
                    } else {
                        self.goal_review_failure(
                            &candidate,
                            "review timed out; execution ownership retained until exit".into(),
                            crate::session::goal::GoalReviewFailureStatus::Blocked,
                        )
                    }
                })?;
            }
            _ = stopped => {},
            _ = self.goal_ctx.cancel.cancelled() => {},
        }
        Ok(())
    }

    pub(super) fn commit_goal_continue(
        &self,
        run_id: u64,
        proposal: GoalContinueProposal,
        seq: usize,
    ) -> Result<()> {
        self.apply_if_run_scheduled(run_id, || {
            let current = self
                .goal_snapshot()
                .ok_or_else(|| anyhow::anyhow!("goal missing"))?;
            ensure!(
                current.status == GoalStatus::Active
                    && !self.goal_ctx.interrupt.load(Ordering::SeqCst)
                    && !self.goal_ctx.cancel.is_cancelled(),
                "goal stopped during continuation adjudication"
            );
            proposal.validate(&current)?;
            ensure!(
                !self.input_inbox.outstanding().iter().any(|r| r.guidance
                    && matches!(
                        r.status,
                        crate::runtime::InputStatus::Pending
                            | crate::runtime::InputStatus::Applying
                            | crate::runtime::InputStatus::Unapplied
                    )),
                "human guidance arrived during adjudication; explicitly resume or withdraw it"
            );
            let report = current
                .review_report
                .as_ref()
                .filter(|r| r.candidate == proposal.candidate && r.conversation_seq == seq)
                .ok_or_else(|| anyhow::anyhow!("continuation report boundary changed"))?;
            for (reference, hash) in &report.evidence_versions {
                ensure!(
                    self.evidence_version(reference)? == *hash,
                    "evidence changed during screening"
                );
            }
            let fingerprint = proposal.fingerprint()?;
            let report_artifact = report.artifact.clone();
            let next = self.goal_ctx.goal_store.transition(
                &current.id,
                current.revision,
                GoalTransition::CommitContinuation {
                    proposal,
                    report: report_artifact,
                    fingerprint,
                    conversation_seq: seq,
                },
            )?;
            self.announce_goal(&next);
            Ok(())
        })
    }

    async fn review_and_accept(&self, run_id: u64, mut goal: GoalSnapshot) -> Result<()> {
        loop {
            let candidate = goal
                .review_candidate
                .clone()
                .ok_or_else(|| anyhow::anyhow!("missing review candidate"))?;
            if let Some(GoalRoundDecision::Continue(record)) = goal.latest_round_decision()
                && record.proposal.candidate == candidate
            {
                ensure!(
                    self.goal_ctx.store.committed_len().await? == record.conversation_seq,
                    "continued decision history changed"
                );
                self.apply_if_run_scheduled(run_id, || {
                    let current = self
                        .goal_snapshot()
                        .ok_or_else(|| anyhow::anyhow!("goal missing"))?;
                    ensure!(
                        current.status == GoalStatus::Active
                            && current.review_candidate.as_ref() == Some(&candidate),
                        "continued candidate changed"
                    );
                    let next = self.goal_ctx.goal_store.transition(
                        &current.id,
                        current.revision,
                        GoalTransition::FinishContinuation {
                            expected: candidate.clone(),
                        },
                    )?;
                    self.announce_goal(&next);
                    Ok(())
                })?;
                return Ok(());
            }
            let existing = goal
                .reviews
                .last()
                .filter(|r| r.candidate == candidate)
                .cloned();
            let (review, reads, expected_conversation_seq) = if let Some(review) = existing {
                if !self
                    .validate_review_evidence(&goal)
                    .is_ok_and(|v| v == goal.evidence_versions)
                {
                    // A known checker result may have been published just before a
                    // crash or pause. Re-audit; never replay unknown checker effects.
                    if !goal.pending_effects.is_empty() {
                        anyhow::bail!("checker side effects require reconciliation");
                    }
                    let Some(next) = self.restart_goal_review(run_id, &goal)? else {
                        return Ok(());
                    };
                    goal = next;
                    continue;
                }
                (
                    review,
                    goal.evidence_versions.clone(),
                    goal.acceptance_conversation_seq,
                )
            } else {
                if let Some(reason) = self
                    .goal_snapshot()
                    .and_then(|g| g.review_budget_reason().map(str::to_string))
                {
                    self.apply_if_run_scheduled(run_id, || {
                        self.goal_review_failure(
                            &candidate,
                            format!("review budget exhausted: {reason}"),
                            crate::session::goal::GoalReviewFailureStatus::BudgetExhausted,
                        )
                    })?;
                    return Ok(());
                }
                let session = Arc::new(super::review::GoalReviewSession::new(
                    goal.clone(),
                    self.goal_ctx.goal_store.clone(),
                    self.goal_ctx.usage.clone(),
                ));
                let outcome = self.run_goal_review(goal.clone(), session.clone()).await;
                self.goal_ctx
                    .goal_store
                    .reconcile_usage(&self.goal_ctx.usage.all_records()?)?;
                if self.goal_ctx.cancel.is_cancelled()
                    || self.goal_ctx.interrupt.load(Ordering::SeqCst)
                {
                    return Ok(());
                }
                if let Some(error) = outcome.as_ref().err()
                    && let Some(exhausted) =
                        error.downcast_ref::<super::review::GoalReviewBudgetExhausted>()
                {
                    self.apply_if_run_scheduled(run_id, || {
                        self.goal_review_failure(
                            &candidate,
                            exhausted.to_string(),
                            crate::session::goal::GoalReviewFailureStatus::BudgetExhausted,
                        )
                    })?;
                    return Ok(());
                }
                let expected_conversation_seq = outcome?;
                let proposal = session.continuation.lock().unwrap().clone();
                if let Some(proposal) = proposal {
                    self.commit_goal_continue(run_id, proposal, expected_conversation_seq)?;
                    return Ok(());
                }
                let (review, reads) = session.export(self)?;
                (review, reads, expected_conversation_seq)
            };
            let conversation_seq = self.goal_ctx.store.committed_len().await?;
            self.apply_if_run_scheduled(run_id, || {
                let Some(current) = self.goal_snapshot() else {
                    return Ok(());
                };
                if current.status != GoalStatus::Active
                    || current.review_candidate.as_ref() != Some(&candidate)
                    || self.goal_ctx.interrupt.load(Ordering::SeqCst)
                    || self.goal_ctx.cancel.is_cancelled()
                {
                    return Ok(());
                }
                ensure!(
                    conversation_seq == expected_conversation_seq,
                    "conversation changed during review"
                );
                ensure!(
                    !self.input_inbox.outstanding().iter().any(|r| r.guidance
                        && matches!(
                            r.status,
                            crate::runtime::InputStatus::Pending
                                | crate::runtime::InputStatus::Applying
                                | crate::runtime::InputStatus::Unapplied
                        )),
                    "human guidance arrived during adjudication; explicitly resume or withdraw it"
                );
                for (reference, hash) in &reads {
                    ensure!(
                        self.evidence_version(reference)? == *hash,
                        "evidence changed during review"
                    );
                }
                let blocker_fingerprint = if current
                    .reviews
                    .last()
                    .is_none_or(|r| r.candidate != candidate)
                    && review.verdict == GoalReviewVerdict::Blocked
                {
                    let blocker = review
                        .blocker
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("missing blocker"))?;
                    let mut ids = blocker.criterion_ids.clone();
                    ids.sort();
                    ids.dedup();
                    let versions = blocker_evidence_versions(&blocker.evidence, |reference| {
                        self.evidence_version(reference)
                    })?;
                    Some(if review.report.is_some() {
                        // Descriptions can vary while the dependency is unchanged.
                        // Fresh artifact IDs are not new facts.
                        let facts: std::collections::BTreeSet<_> = versions
                            .iter()
                            .map(|(reference, hash)| {
                                (
                                    if reference.starts_with("artifact://") {
                                        "artifact"
                                    } else {
                                        *reference
                                    },
                                    hash,
                                )
                            })
                            .collect();
                        hash_bytes(&serde_json::to_vec(&(
                            current.contract_revision,
                            &blocker.category,
                            ids,
                            facts,
                        ))?)
                    } else {
                        hash_bytes(&serde_json::to_vec(&(
                            &blocker.category,
                            ids,
                            &blocker.dependency,
                            versions,
                        ))?)
                    })
                } else {
                    None
                };
                goal = self.goal_ctx.goal_store.transition(
                    &current.id,
                    current.revision,
                    GoalTransition::CommitReview {
                        review: review.clone(),
                        evidence_versions: reads.clone(),
                        expected_conversation_seq,
                        blocker_fingerprint,
                    },
                )?;
                self.announce_goal(&goal);
                Ok(())
            })?;
            if review.verdict != GoalReviewVerdict::Satisfied
                || !self.run_scheduled(run_id)
                || goal.status != GoalStatus::Active
                || self.goal_ctx.interrupt.load(Ordering::SeqCst)
                || self.goal_ctx.cancel.is_cancelled()
            {
                return Ok(());
            }
            let checker = match &goal.spec.acceptance {
                GoalAcceptance::Verifier { id } => Some(id.clone()),
                _ => goal.spec.checker_id.clone(),
            };
            if let Some(id) = &checker
                && !self.checker_snapshot_matches(&goal)?
            {
                let verifier = self
                    .goal_driver
                    .verifiers
                    .iter()
                    .find(|v| v.id() == id)
                    .ok_or_else(|| anyhow::anyhow!("configured checker unavailable: {id}"))?
                    .clone();
                let context = GoalVerificationContext {
                    cwd: self.goal_ctx.cwd.clone(),
                    goal: goal.clone(),
                    cancelled: self.goal_ctx.interrupt.clone(),
                    tools: Arc::new(crate::context::ToolContext::from(self.goal_ctx.as_ref())),
                };
                let effect_id = format!(
                    "checker:{}:{}:{}",
                    candidate.iteration, candidate.generation, id
                );
                self.apply_if_run_scheduled(run_id, || {
                    if let Some(g) = self.goal_ctx.goal_store.transition_if_current(
                        &goal.id,
                        GoalTransition::BeginChecker {
                            expected: candidate.clone(),
                            effect_id: effect_id.clone(),
                        },
                    )? {
                        self.announce_goal(&g);
                    }
                    Ok(())
                })?;
                if !self.run_scheduled(run_id)
                    || self.goal_ctx.interrupt.load(Ordering::SeqCst)
                    || self.goal_ctx.cancel.is_cancelled()
                {
                    return Ok(());
                }
                let result = verifier.verify(context).await?;
                let checked_versions = if result.status == GoalVerificationStatus::Pass {
                    goal.evidence_versions
                        .iter()
                        .map(|(reference, _)| {
                            self.checker_evidence_version(reference)
                                .map(|v| (reference.clone(), v))
                        })
                        .collect::<Result<Vec<_>>>()
                } else {
                    Ok(Vec::new())
                };
                let trusted_checker =
                    result.status == GoalVerificationStatus::Pass && checked_versions.is_ok();
                let saved_versions = checked_versions
                    .as_ref()
                    .map_or_else(|_| Vec::new(), Clone::clone);
                let checker_record = GoalCheckerRecord {
                    candidate: candidate.clone(),
                    checker_id: id.clone(),
                    status: match result.status {
                        GoalVerificationStatus::Pass => GoalCriterionStatus::Pass,
                        GoalVerificationStatus::Fail => GoalCriterionStatus::Fail,
                        GoalVerificationStatus::Inconclusive => GoalCriterionStatus::Unknown,
                    },
                    reason: result.reason.clone(),
                    evidence: result.evidence.clone(),
                };
                if let Some(updated) = self.goal_ctx.goal_store.transition_if_current(
                    &goal.id,
                    GoalTransition::RecordCheckerResult {
                        effect_id: effect_id.clone(),
                        record: checker_record,
                        trusted: trusted_checker,
                        evidence_versions: saved_versions,
                    },
                )? {
                    self.announce_goal(&updated);
                    goal = updated;
                }
                if let Err(error) = checked_versions {
                    self.apply_if_run_scheduled(run_id, || {
                        self.verification_failure(
                            &goal.id,
                            format!("checker evidence could not be versioned: {error}"),
                            Vec::new(),
                            true,
                        )
                    })?;
                    return Ok(());
                }
                if !self.run_scheduled(run_id)
                    || self.goal_ctx.interrupt.load(Ordering::SeqCst)
                    || self.goal_ctx.cancel.is_cancelled()
                {
                    return Ok(());
                }
                if result.status != GoalVerificationStatus::Pass {
                    self.apply_if_run_scheduled(run_id, || {
                        self.verification_failure(
                            &goal.id,
                            result.reason.clone(),
                            result.evidence.clone(),
                            result.status == GoalVerificationStatus::Inconclusive,
                        )
                    })?;
                    return Ok(());
                }
                if !self
                    .validate_review_evidence(&goal)
                    .is_ok_and(|v| v == goal.evidence_versions)
                {
                    let Some(next) = self.restart_goal_review(run_id, &goal)? else {
                        return Ok(());
                    };
                    goal = next;
                    continue;
                }
            }
            let conversation_seq = self.goal_ctx.store.committed_len().await?;
            self.apply_if_run_scheduled(run_id, || {
                let Some(current) = self.goal_snapshot() else {
                    return Ok(());
                };
                if current.status != GoalStatus::Active
                    || current.review_candidate.as_ref() != Some(&candidate)
                    || self.goal_ctx.interrupt.load(Ordering::SeqCst)
                    || self.goal_ctx.cancel.is_cancelled()
                {
                    return Ok(());
                }
                ensure!(
                    conversation_seq == current.acceptance_conversation_seq
                        && self.validate_review_evidence(&current)? == current.evidence_versions
                        && (checker.is_none() || self.checker_snapshot_matches(&current)?),
                    "candidate changed before completion"
                );
                let completed = self.goal_ctx.goal_store.transition(
                    &current.id,
                    current.revision,
                    GoalTransition::CompleteReview {
                        expected: candidate.clone(),
                        checker: checker.is_some(),
                    },
                )?;
                self.announce_goal(&completed);
                Ok(())
            })?;
            return Ok(());
        }
    }
    /// Record that the current independent review could not be completed.
    ///
    /// This is an infrastructure outcome, not evidence that the goal failed.
    /// Keep the candidate and its history so an explicit resume retries this
    /// audit without replaying the execution round.
    pub(crate) fn goal_review_failure(
        &self,
        candidate: &GoalReviewCandidate,
        reason: String,
        outcome: crate::session::goal::GoalReviewFailureStatus,
    ) -> Result<()> {
        let Some(goal) = self.goal_ctx.goal_store.transition_if_current(
            &candidate.goal_id,
            GoalTransition::ReviewFailure {
                expected: candidate.clone(),
                reason,
                outcome,
            },
        )?
        else {
            return Ok(());
        };
        self.announce_goal(&goal);
        Ok(())
    }

    /// Apply an acceptance failure to the goal it belongs to.
    ///
    /// A replacement must never inherit a stale failure: the update is bound
    /// to the identity under acceptance, not to whatever is current now.
    pub(crate) fn verification_failure(
        &self,
        goal_id: &str,
        reason: String,
        evidence: Vec<String>,
        blocked: bool,
    ) -> Result<()> {
        let mut references = evidence;
        if let Some(review) = self.goal_snapshot().and_then(|g| g.reviews.last().cloned()) {
            references.extend(review.evidence());
        }
        references.sort();
        references.dedup();
        let versions: Vec<_> = references
            .iter()
            .filter_map(|r| self.evidence_version(r).ok().map(|v| (r, v)))
            .collect();
        let fingerprint = hash_bytes(&serde_json::to_vec(&(reason.as_str(), versions))?);
        let Some(g) = self.goal_ctx.goal_store.transition_if_current(
            goal_id,
            GoalTransition::VerificationFailure {
                reason,
                fingerprint,
                blocked,
            },
        )?
        else {
            return Ok(());
        };
        self.announce_goal(&g);
        Ok(())
    }
}
