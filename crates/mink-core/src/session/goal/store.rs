use super::transitions::GoalTransition;
use super::types::State;
use super::*;

/// Identity of one admitted goal iteration.
///
/// Settlement and late callbacks name the goal, iteration and turn they belong
/// to instead of "the latest iteration of whatever is current now".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IterationKey {
    pub(crate) goal_id: String,
    pub(crate) iteration: u32,
    pub(crate) turn_id: String,
}

/// Outcome of an identity-bound settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApplyResult {
    Applied,
    AlreadyApplied,
}

/// Facts the orchestrator prepared at the persistence boundary.
#[derive(Debug, Clone, Default)]
pub(crate) struct SettlementFacts {
    /// An interrupted turn with no session-wide stop in flight pauses the goal.
    pub(crate) interrupted_pause: bool,
}

pub struct GoalStore {
    path: PathBuf,
    journal: PathBuf,
    state: Mutex<State>,
    pub(super) fault: PersistenceFault,
    changes: tokio::sync::watch::Sender<Option<GoalSnapshot>>,
}

impl GoalStore {
    pub(crate) fn load(directory: &Path, fault: PersistenceFault) -> Result<Self> {
        let path = directory.join("goal.json");
        let journal = directory.join("goal-transaction.json");
        let read = |p: &Path| -> Result<Option<State>> {
            match std::fs::read(p) {
                Ok(bytes) => {
                    let state: State = serde_json::from_slice(&bytes)?;
                    ensure!(
                        state.version == GOAL_FORMAT_VERSION,
                        "unsupported goal state version"
                    );
                    for goal in state.current.iter().chain(state.history.iter()) {
                        ensure!(
                            goal.version == GOAL_FORMAT_VERSION
                                && goal.spec.version == GOAL_FORMAT_VERSION,
                            "unsupported goal contract version"
                        );
                    }
                    Ok(Some(state))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        };
        let mut state = read(&path)?.unwrap_or(State {
            version: GOAL_FORMAT_VERSION,
            ..State::default()
        });
        for goal in state.current.iter_mut().chain(state.history.iter_mut()) {
            goal.normalize_phase();
        }
        if let Some(mut pending) = read(&journal)? {
            ensure!(
                pending.version == GOAL_FORMAT_VERSION,
                "unsupported goal journal"
            );
            for goal in pending.current.iter_mut().chain(pending.history.iter_mut()) {
                goal.normalize_phase();
            }
            publish_state(&path, &serde_json::to_vec(&pending)?, &fault)?;
            state = pending;
        }
        ensure!(
            state.version == GOAL_FORMAT_VERSION,
            "unsupported goal state"
        );
        let (changes, _) = tokio::sync::watch::channel(state.current.clone());
        Ok(Self {
            path,
            journal,
            state: Mutex::new(state),
            fault,
            changes,
        })
    }
    pub fn snapshot(&self) -> Option<GoalSnapshot> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .clone()
            .map(|mut goal| {
                goal.normalize_phase();
                goal
            })
    }
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<GoalSnapshot>> {
        self.changes.subscribe()
    }
    fn publish(&self, state: &mut State, mut next: State) -> Result<()> {
        self.fault.check()?;
        next.version = GOAL_FORMAT_VERSION;
        for goal in next.current.iter_mut().chain(next.history.iter_mut()) {
            goal.version = GOAL_FORMAT_VERSION;
            goal.normalize_phase();
        }
        let bytes = serde_json::to_vec(&next)?;
        publish_state(&self.journal, &bytes, &self.fault)?;
        if let Err(error) = publish_state(&self.path, &bytes, &self.fault) {
            return Err(self.fault.raise(
                &self.path,
                format!("goal journal committed, snapshot publication failed: {error:#}"),
            ));
        }
        *state = next;
        self.changes.send_replace(state.current.clone());
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn create(&self, spec: GoalSpec, environment: String) -> Result<GoalSnapshot> {
        self.create_snapshot(spec, environment, None, GoalStatus::Active, true)
    }
    pub(crate) fn draft(
        &self,
        spec: GoalSpec,
        environment: String,
        duration_secs: u64,
    ) -> Result<GoalDraftSnapshot> {
        self.command(GoalCommand::CreateDraft {
            spec: Box::new(spec),
            environment,
            duration_secs,
            ready: true,
        })
    }
    pub(crate) fn draft_pending(
        &self,
        spec: GoalSpec,
        environment: String,
        duration_secs: u64,
    ) -> Result<GoalDraftSnapshot> {
        self.command(GoalCommand::CreateDraft {
            spec: Box::new(spec),
            environment,
            duration_secs,
            ready: false,
        })
    }
    pub(crate) fn command(&self, command: GoalCommand) -> Result<GoalSnapshot> {
        if let GoalCommand::CreateDraft {
            spec,
            environment,
            duration_secs,
            ready,
        } = command
        {
            ensure!(duration_secs > 0, "draft duration must be positive");
            return self.create_snapshot(
                *spec,
                environment,
                Some(duration_secs),
                GoalStatus::Draft,
                ready,
            );
        }
        let (id, revision) = command.identity().expect("existing goal command");
        let id = id.to_string();
        self.change_with(&id, revision, |g| command.apply_to(g))
    }

    fn create_snapshot(
        &self,
        spec: GoalSpec,
        environment: String,
        duration: Option<u64>,
        status: GoalStatus,
        draft_ready: bool,
    ) -> Result<GoalSnapshot> {
        spec.validate()?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            state
                .current
                .as_ref()
                .is_none_or(|g| g.status.is_terminal()),
            "session already has an unfinished goal"
        );
        let mut next = state.clone();
        if let Some(old) = next.current.take() {
            next.history.push(old);
        }
        let goal = GoalSnapshot {
            version: GOAL_FORMAT_VERSION,
            id: format!(
                "goal-{}",
                time::OffsetDateTime::now_utc().unix_timestamp_nanos()
            ),
            revision: 1,
            contract_revision: 1,
            spec,
            status,
            phase: GoalPhase::Idle,
            reason: None,
            review_failure_reason: None,
            review_stage: None,
            review_failure_stage: None,
            review_requires_full: false,
            review_escalation_reason: None,
            review_report: None,
            continuations: Vec::new(),
            continuation_fingerprint: None,
            continuation_rounds: 0,
            completion_basis: None,
            iterations: Vec::new(),
            report: None,
            report_repair_used: false,
            usage_records: Vec::new(),
            usage: Default::default(),
            pending_effects: Vec::new(),
            failure_fingerprint: None,
            repeated_failures: 0,
            blocked_rounds: 0,
            blocker_rounds: 0,
            blocker_fingerprint: None,
            evidence_versions: Vec::new(),
            acceptance_conversation_seq: 0,
            environment,
            hidden: false,
            draft_duration_secs: duration,
            needs_confirmation: false,
            review_candidate: None,
            review_generation: 0,
            reviews: Vec::new(),
            checker_passed_candidate: None,
            checker_evidence_versions: Vec::new(),
            checker_records: Vec::new(),
            confirmed_after_iteration: 0,
            draft_ready,
        };
        next.current = Some(goal.clone());
        self.publish(&mut state, next)?;
        Ok(goal)
    }
    pub(crate) fn replace_draft(
        &self,
        id: &str,
        revision: u64,
        draft: GoalDraftSpec,
    ) -> Result<GoalDraftSnapshot> {
        self.command(GoalCommand::Draft {
            id: id.into(),
            revision,
            spec: draft,
        })
    }
    pub(crate) fn confirm(
        &self,
        id: &str,
        revision: u64,
        environment: String,
    ) -> Result<GoalSnapshot> {
        self.command(GoalCommand::Confirm {
            id: id.into(),
            revision,
            environment,
        })
    }

    pub(crate) fn transition_from_snapshot_classified(
        &self,
        id: &str,
        revision: u64,
        prepare: impl FnOnce(&GoalSnapshot) -> Result<GoalTransition>,
    ) -> std::result::Result<GoalSnapshot, GoalChangeError> {
        self.change_classified(id, revision, move |goal| prepare(goal)?.apply_to(goal))
    }

    pub(crate) fn transition(
        &self,
        id: &str,
        revision: u64,
        transition: GoalTransition,
    ) -> Result<GoalSnapshot> {
        self.change_with(id, revision, move |goal| transition.apply_to(goal))
    }

    /// Test-only escape hatch for constructing legacy and fault-recovery states.
    /// Runtime code must use a named transition or an identity-bound maintenance update.
    #[cfg(test)]
    pub(crate) fn change(
        &self,
        id: &str,
        revision: u64,
        f: impl FnOnce(&mut GoalSnapshot) -> Result<()>,
    ) -> Result<GoalSnapshot> {
        self.change_with(id, revision, f)
    }

    fn change_with(
        &self,
        id: &str,
        revision: u64,
        f: impl FnOnce(&mut GoalSnapshot) -> Result<()>,
    ) -> Result<GoalSnapshot> {
        self.change_classified(id, revision, f)
            .map_err(GoalChangeError::into_anyhow)
    }

    /// [`Self::change_with`] with the refusal classified as command or publication.
    ///
    /// Stop commands must stop the run when their intent cannot be stored, but
    /// must not touch the run for a stale revision or an invalid transition:
    /// only this split makes that distinction.
    fn change_classified(
        &self,
        id: &str,
        revision: u64,
        f: impl FnOnce(&mut GoalSnapshot) -> Result<()>,
    ) -> std::result::Result<GoalSnapshot, GoalChangeError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = state.clone();
        let g = next
            .current
            .as_mut()
            .ok_or_else(|| GoalChangeError::Rejected(anyhow::anyhow!("no goal")))?;
        if g.id != id || g.revision != revision {
            return Err(GoalChangeError::Rejected(anyhow::anyhow!(
                "stale goal ID or revision; reread goal_snapshot"
            )));
        }
        f(g).map_err(GoalChangeError::Rejected)?;
        g.revision = g
            .revision
            .checked_add(1)
            .ok_or_else(|| GoalChangeError::Rejected(anyhow::anyhow!("goal revision overflow")))?;
        g.version = GOAL_FORMAT_VERSION;
        g.normalize_phase();
        let result = g.clone();
        self.publish(&mut state, next)
            .map_err(GoalChangeError::Publish)?;
        Ok(result)
    }
    /// Maintenance update for the whole snapshot (recovery, reconciliation,
    /// projection helpers). Round decisions must use a typed entry point so a
    /// replaced goal can never be modified.
    pub(super) fn update(
        &self,
        f: impl FnOnce(&mut GoalSnapshot) -> Result<()>,
    ) -> Result<GoalSnapshot> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = state.clone();
        let g = next
            .current
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no goal"))?;
        f(g)?;
        g.revision = g
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("goal revision overflow"))?;
        g.version = GOAL_FORMAT_VERSION;
        g.normalize_phase();
        let result = g.clone();
        self.publish(&mut state, next)?;
        Ok(result)
    }
    #[cfg(test)]
    pub(crate) fn update_for_test(
        &self,
        f: impl FnOnce(&mut GoalSnapshot) -> Result<()>,
    ) -> Result<GoalSnapshot> {
        self.update(f)
    }
    /// Apply a named runtime update only while `id` is still the current goal.
    ///
    /// A driver can still be settling a round after the goal was cancelled and
    /// replaced. Revisions restart at 1 for every goal, so an unguarded update
    /// would land the old round's usage and decisions on the replacement.
    /// Returns `None` when the goal was replaced, so callers can skip.
    pub(crate) fn transition_if_current(
        &self,
        id: &str,
        transition: GoalTransition,
    ) -> Result<Option<GoalSnapshot>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .current
            .as_ref()
            .is_none_or(|current| current.id != id)
        {
            return Ok(None);
        }
        let mut next = state.clone();
        let goal = next
            .current
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no goal"))?;
        transition.apply_to(goal)?;
        goal.revision = goal
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("goal revision overflow"))?;
        goal.version = GOAL_FORMAT_VERSION;
        goal.normalize_phase();
        let result = goal.clone();
        self.publish(&mut state, next)?;
        Ok(Some(result))
    }

    pub(crate) fn mark_effect(&self, call_id: &str) -> Result<()> {
        if self
            .snapshot()
            .is_none_or(|g| g.phase != GoalPhase::Executing)
        {
            return Ok(());
        }
        self.update(|g| {
            ensure!(
                g.status == GoalStatus::Active && g.phase == GoalPhase::Executing,
                "goal stopped before tool dispatch"
            );
            if !g.pending_effects.iter().any(|id| id == call_id) {
                g.pending_effects.push(call_id.into());
            }
            Ok(())
        })?;
        Ok(())
    }
    pub(crate) fn close_effects(&self, ids: &[String]) -> Result<()> {
        if self
            .snapshot()
            .is_none_or(|g| !g.pending_effects.iter().any(|id| ids.contains(id)))
        {
            return Ok(());
        }
        self.update(|g| {
            g.pending_effects.retain(|id| !ids.contains(id));
            Ok(())
        })?;
        Ok(())
    }
    /// Append the terminal goal facts once, at the tail of the conversation.
    ///
    /// Informational by contract: the lifecycle has already settled, so a failed
    /// append is reported by the caller instead of changing the goal state or
    /// latching the session.
    pub(crate) async fn append_ended_row(
        &self,
        store: &crate::session::store::ConversationStore,
        max_chars: usize,
    ) -> Result<bool> {
        let Some(g) = self
            .snapshot()
            .filter(|g| g.status.is_terminal() && !g.hidden)
        else {
            return Ok(false);
        };
        if store
            .lines()
            .await?
            .iter()
            .any(|m| m["_mink"]["goal_id"] == g.id && m["_mink"]["goal_state_kind"] == "ended")
        {
            return Ok(false);
        }
        let message = serde_json::json!({
            "role": "user",
            "content": state_text(&g, GoalStateKind::Ended, max_chars),
            "_mink": {
                "internal": true,
                "goal_id": g.id,
                "goal_revision": g.revision,
                "goal_state_kind": "ended",
            },
        });
        store.append_runtime_message_durable(message).await?;
        Ok(true)
    }
    /// Contract checkpoint mounted inside the active window after a compaction.
    ///
    /// The rendering carries no per-round facts, so the checkpoint stays
    /// byte-stable between rounds and the request prefix survives the very
    /// compaction that introduced it.
    pub(crate) fn checkpoint(&self, max_chars: usize) -> Result<Option<serde_json::Value>> {
        let Some(g) = self
            .snapshot()
            .filter(|g| !g.status.is_terminal() && !g.hidden)
        else {
            return Ok(None);
        };
        Ok(Some(serde_json::json!({
            "role": "user",
            "content": state_text(&g, GoalStateKind::Contract, max_chars),
            "_mink": {
                "internal": true,
                "goal_id": g.id,
                "goal_revision": g.revision,
                "goal_state_kind": "contract",
            },
        })))
    }
    /// Begin an iteration for the current schedulable goal.
    ///
    /// Schedulability, phase reset and the iteration row land in one atomic
    /// commit; the caller maps the error to its own surface.
    pub(crate) fn begin_iteration(
        &self,
        turn_id: &str,
        conversation_start: usize,
    ) -> Result<GoalSnapshot> {
        self.update(|g| {
            anyhow::ensure!(
                g.status == GoalStatus::Active && g.phase == GoalPhase::Idle,
                "goal is not schedulable"
            );
            anyhow::ensure!(g.budget_reason().is_none(), "goal budget exhausted");
            g.phase = GoalPhase::Executing;
            g.review_stage = None;
            g.review_requires_full = false;
            g.review_escalation_reason = None;
            g.review_report = None;
            g.report = None;
            g.review_candidate = None;
            g.review_failure_stage = None;
            g.checker_passed_candidate = None;
            g.checker_evidence_versions.clear();
            g.evidence_versions.clear();
            g.iterations.push(GoalIteration {
                iteration: g.iterations.len() as u32 + 1,
                turn_id: turn_id.to_string(),
                billing_turn_id: None,
                finished: false,
                conversation_start,
                outcome_status: None,
                outcome_error: None,
            });
            Ok(())
        })
    }

    /// Associate the running iteration with the turn that bills it.
    ///
    /// Identity-bound to the iteration that is currently executing: a
    /// finished or missing iteration is left untouched.
    pub(crate) fn associate_iteration_billing(&self, billing_turn_id: &str) -> Result<()> {
        self.update(|g| {
            if g.phase == GoalPhase::Executing
                && let Some(iteration) = g.iterations.last_mut()
                && !iteration.finished
            {
                iteration.billing_turn_id = Some(billing_turn_id.to_string());
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Pause the current goal when it is still active.
    ///
    /// The user interrupt targets "the goal that is running now"; a replaced
    /// or already paused goal must not be touched. Checked atomically under
    /// `self.state` lock so a concurrent transition never causes a no-op write
    /// or revision bump, and clears `review_stage` matching `GoalTransition::Pause`.
    pub(crate) fn pause_if_active(&self, reason: &str) -> Result<Option<GoalSnapshot>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(current) = state.current.as_ref() else {
            return Ok(None);
        };
        if current.status != GoalStatus::Active {
            return Ok(None);
        }
        let mut next = state.clone();
        let goal = next.current.as_mut().expect("checked current goal");
        goal.status = GoalStatus::Paused;
        goal.reason = Some(reason.to_string());
        goal.review_stage = None;
        goal.revision = goal
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("goal revision overflow"))?;
        goal.version = GOAL_FORMAT_VERSION;
        goal.normalize_phase();
        let paused = goal.clone();
        self.publish(&mut state, next)?;
        Ok(Some(paused))
    }

    /// Settle one iteration by identity, exactly once.
    ///
    /// A repeat with identical facts is idempotent; a repeat with different
    /// facts is an error instead of silently overwriting. The status decision
    /// of the round (failed/interrupted) lands in the same atomic commit as
    /// the finished/billing/usage facts, so a crash cannot leave them split.
    pub(crate) fn settle_iteration(
        &self,
        key: &IterationKey,
        result: &crate::agent::orchestrator::TurnRunResult,
        facts: &SettlementFacts,
    ) -> Result<ApplyResult> {
        use crate::runtime::TurnStatus;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = state.clone();
        let is_current = next
            .current
            .as_ref()
            .is_some_and(|goal| goal.id == key.goal_id);
        let goal = if is_current {
            next.current.as_mut().expect("checked current goal")
        } else {
            next.history
                .iter_mut()
                .find(|goal| goal.id == key.goal_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "goal iteration settlement names an unknown goal: {}",
                        key.goal_id
                    )
                })?
        };
        let iteration = goal
            .iterations
            .iter_mut()
            .find(|iteration| {
                iteration.iteration == key.iteration && iteration.turn_id == key.turn_id
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "goal {} has no iteration {} for turn {}",
                    key.goal_id,
                    key.iteration,
                    key.turn_id
                )
            })?;
        if iteration.finished {
            let same = iteration.billing_turn_id.as_deref()
                == Some(result.billing_turn_id.as_str())
                && iteration.outcome_status == Some(result.status);
            return if same {
                Ok(ApplyResult::AlreadyApplied)
            } else {
                anyhow::bail!(
                    "goal iteration {} was already settled with different facts",
                    key.iteration
                );
            };
        }
        iteration.finished = true;
        iteration.billing_turn_id = Some(result.billing_turn_id.clone());
        iteration.outcome_status = Some(result.status);
        iteration.outcome_error = result.error.clone();
        for record in &result.usage_records {
            if !goal
                .usage_records
                .iter()
                .any(|old| old.request_id == record.request_id)
            {
                goal.usage_records.push(record.clone());
            }
        }
        goal.usage = crate::session::usage::UsageSummary::from_records(&goal.usage_records);
        if is_current {
            let goal = next.current.as_mut().expect("checked current goal");
            goal.phase = GoalPhase::Idle;
            if goal.status == GoalStatus::Active {
                match result.status {
                    TurnStatus::Failed => {
                        goal.status = GoalStatus::Failed;
                        goal.reason = result.error.clone();
                    }
                    TurnStatus::Interrupted if facts.interrupted_pause => {
                        goal.status = GoalStatus::Paused;
                        goal.reason = Some("execution stopped".into());
                    }
                    _ => {}
                }
            }
            // Natural completion only creates an audit phase. The execution model
            // cannot decide the goal's completion or consume a repair round.
            if goal.status == GoalStatus::Active && result.status == TurnStatus::Ok {
                goal.phase = GoalPhase::Verifying;
                goal.review_candidate = None;
                goal.review_stage = Some(GoalReviewStage::Screening);
            }
            goal.revision = goal
                .revision
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("goal revision overflow"))?;
            goal.version = GOAL_FORMAT_VERSION;
            goal.normalize_phase();
        }
        self.publish(&mut state, next)?;
        Ok(ApplyResult::Applied)
    }
    pub(crate) fn reconcile_usage(
        &self,
        records: &[crate::session::usage::UsageRecord],
    ) -> Result<()> {
        let Some(goal) = self.snapshot() else {
            return Ok(());
        };
        let additional: Vec<_> = records
            .iter()
            .filter(|r| {
                (goal
                    .iterations
                    .iter()
                    .any(|i| i.billing_turn_id.as_deref() == Some(r.billing_turn_id.as_str()))
                    || r.billing_turn_id
                        .starts_with(&format!("goal-review:{}:", goal.id)))
                    && !goal
                        .usage_records
                        .iter()
                        .any(|old| old.request_id == r.request_id)
            })
            .cloned()
            .collect();
        if additional.is_empty() {
            return Ok(());
        }
        self.update(|g| {
            for record in additional {
                if !g
                    .usage_records
                    .iter()
                    .any(|old| old.request_id == record.request_id)
                {
                    g.usage_records.push(record);
                }
            }
            g.usage = crate::session::usage::UsageSummary::from_records(&g.usage_records);
            Ok(())
        })?;
        Ok(())
    }
    pub(crate) fn recover(
        &self,
        environment: &str,
        history: &[serde_json::Value],
        committed_sequence: usize,
    ) -> Result<()> {
        let Some(g) = self.snapshot().filter(|g| !g.status.is_terminal()) else {
            return Ok(());
        };
        self.change_with(&g.id, g.revision, |g| {
            let start = g.iterations.last().map_or(0, |i| i.conversation_start);
            let resolved: Vec<_> = history
                .iter()
                .filter(|message| {
                    message["seq"]
                        .as_u64()
                        .is_some_and(|sequence| sequence as usize > start)
                })
                .filter_map(|m| m["content"].as_array())
                .flatten()
                .filter(|b| {
                    b["type"] == "tool_result"
                        && serde_json::from_value::<crate::tools::metadata::ToolStatus>(
                            b["_mink"]["status"].clone(),
                        )
                        .is_ok()
                })
                .filter_map(|b| b["tool_use_id"].as_str())
                .collect();
            g.pending_effects
                .retain(|id| !resolved.contains(&id.as_str()));
            ensure!(
                g.spec.version == GOAL_FORMAT_VERSION,
                "unsupported goal contract version"
            );
            g.spec.validate()?;
            if g.status != GoalStatus::Active {
                if !g.pending_effects.is_empty() {
                    if g.status != GoalStatus::Draft {
                        g.status = GoalStatus::Blocked;
                    }
                    g.reason = Some("side effect result unknown; reconcile before resume".into());
                }
                return Ok(());
            }
            if g.environment != environment {
                g.status = GoalStatus::Blocked;
                g.reason = Some(
                    "goal permissions or working directory changed; reconcile before resume".into(),
                );
            } else if !g.pending_effects.is_empty() {
                g.status = GoalStatus::Blocked;
                g.reason = Some("side effect result unknown; reconcile before resume".into());
            } else if g.iterations.last().is_some_and(|i| i.finished) {
                let settled_review = g.reviews.last().filter(|review| {
                    review.candidate.goal_id == g.id
                        && review.candidate.contract_revision == g.contract_revision
                        && review.candidate.iteration as usize == g.iterations.len()
                        && g.acceptance_conversation_seq == committed_sequence
                        && review.verdict != GoalReviewVerdict::Satisfied
                });
                match g.iterations.last().and_then(|i| i.outcome_status) {
                    Some(crate::runtime::TurnStatus::Failed) => {
                        g.status = GoalStatus::Failed;
                        g.reason = g.iterations.last().and_then(|i| i.outcome_error.clone());
                        g.phase = GoalPhase::Idle;
                    }
                    Some(crate::runtime::TurnStatus::Ok)
                        if settled_review.is_some()
                            || g.continuations.last().is_some_and(|c| {
                                c.proposal.candidate.goal_id == g.id
                                    && c.proposal.candidate.contract_revision == g.contract_revision
                                    && c.proposal.candidate.iteration as usize == g.iterations.len()
                                    && c.conversation_seq == committed_sequence
                            }) =>
                    {
                        // This boundary already includes an atomic review/decision;
                        // resume the next execution rather than replaying its audit.
                        g.phase = GoalPhase::Idle;
                        g.review_candidate =
                            settled_review.map(|r| r.candidate.clone()).or_else(|| {
                                g.continuations.last().map(|c| c.proposal.candidate.clone())
                            });
                    }
                    Some(crate::runtime::TurnStatus::Ok)
                        if matches!(g.phase, GoalPhase::Idle | GoalPhase::Executing) =>
                    {
                        g.phase = GoalPhase::Verifying;
                        g.review_candidate = None;
                        g.reason = Some(
                            "recovered normal execution boundary; independent audit required"
                                .into(),
                        );
                    }
                    _ => {}
                }
            } else if g.phase == GoalPhase::Executing {
                g.phase = GoalPhase::Idle;
                g.reason = Some("interrupted iteration; recovery iteration required".into());
            }
            Ok(())
        })?;
        Ok(())
    }
}
