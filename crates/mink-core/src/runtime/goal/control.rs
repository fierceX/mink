//! Goal control surface: contract lifecycle, leases and admission.

use super::*;

impl AgentRuntimeHandle {
    pub fn goal_verifier_ids(&self) -> Vec<String> {
        self.goal_driver
            .verifiers
            .iter()
            .map(|v| v.id().to_string())
            .collect()
    }
    pub fn goal_snapshot(&self) -> Option<GoalSnapshot> {
        self.goal_ctx.goal_store.snapshot()
    }
    pub fn subscribe_goal(&self) -> tokio::sync::watch::Receiver<Option<GoalSnapshot>> {
        self.goal_ctx.goal_store.subscribe()
    }
    /// Retired direct activation. Hosts must explicitly confirm a durable draft.
    pub fn create_goal(&self, _spec: GoalSpec) -> RuntimeResult<GoalSnapshot> {
        Err(RuntimeError::Command(
            "goal protocol upgraded; use draft_goal then confirm_goal".into(),
        ))
    }
    pub fn create_and_start_goal(
        &self,
        _spec: GoalSpec,
        _lease: Box<dyn GoalRunLease>,
    ) -> RuntimeResult<GoalSnapshot> {
        Err(RuntimeError::Command(
            "goal protocol upgraded; use confirm_and_start_goal".into(),
        ))
    }
    /// Save a host-prepared contract, still requiring explicit confirmation.
    pub fn draft_goal(
        &self,
        spec: GoalSpec,
        duration_secs: u64,
    ) -> RuntimeResult<GoalDraftSnapshot> {
        self.draft_goal_inner(spec, duration_secs, true)
    }
    /// Save a request awaiting model decomposition; not confirmable yet.
    pub fn request_goal_draft(
        &self,
        spec: GoalSpec,
        duration_secs: u64,
    ) -> RuntimeResult<GoalDraftSnapshot> {
        self.draft_goal_inner(spec, duration_secs, false)
    }
    fn draft_goal_inner(
        &self,
        mut spec: GoalSpec,
        duration_secs: u64,
        ready: bool,
    ) -> RuntimeResult<GoalDraftSnapshot> {
        let _guard = self
            .goal_driver
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.ensure_goal_activation_allowed()?;
        self.ensure_goal_driver_idle()?;
        if let Some(active_turn_id) = self.active_turn_id() {
            return Err(RuntimeError::Busy { active_turn_id });
        }
        if !self.goal_ctx.tool_surface.has("GoalRead") {
            return Err(RuntimeError::Configuration(
                "goal requires GoalRead in the resolved tool surface".into(),
            ));
        }
        if spec.checker_id.is_none() && self.goal_driver.verifiers.len() == 1 {
            spec.checker_id = Some(self.goal_driver.verifiers[0].id().into());
        }
        if spec.checker_id.is_none()
            && self.goal_driver.verifiers.len() > 1
            && !matches!(spec.acceptance, GoalAcceptance::Verifier { .. })
        {
            return Err(RuntimeError::Configuration(
                "select a checker_id when multiple goal checkers are registered".into(),
            ));
        }
        let environment = goal_environment(&self.goal_ctx.cwd, &self.goal_ctx.tool_surface);
        let goal = if ready {
            self.goal_ctx
                .goal_store
                .draft(spec, environment, duration_secs)
        } else {
            self.goal_ctx
                .goal_store
                .draft_pending(spec, environment, duration_secs)
        }
        .map_err(command_error)?;
        self.announce_goal(&goal);
        Ok(goal)
    }
    pub fn goal_draft_snapshot(&self) -> Option<GoalDraftSnapshot> {
        self.goal_snapshot()
            .filter(|g| g.status == GoalStatus::Draft)
    }
    pub fn cancel_goal_draft(&self, id: &str, revision: u64) -> RuntimeResult<GoalSnapshot> {
        self.goal_control(
            id,
            revision,
            |g| {
                GoalCommand::CancelDraft {
                    id: id.into(),
                    revision,
                }
                .apply_to(g)
            },
            true,
        )
    }
    pub fn confirm_goal(&self, id: &str, revision: u64) -> RuntimeResult<GoalSnapshot> {
        self.confirm_goal_inner(id, revision, None)
    }
    pub fn confirm_and_start_goal(
        &self,
        id: &str,
        revision: u64,
        lease: Box<dyn GoalRunLease>,
    ) -> RuntimeResult<GoalSnapshot> {
        self.confirm_goal_inner(id, revision, Some(lease))
    }
    fn confirm_goal_inner(
        &self,
        id: &str,
        revision: u64,
        lease: Option<Box<dyn GoalRunLease>>,
    ) -> RuntimeResult<GoalSnapshot> {
        self.ensure_goal_activation_allowed()?;
        self.ensure_goal_submission_available()?;
        if lease.is_some() {
            self.ensure_host_activation_ready()?;
        } else if self.goal_driver.lock_control().enabled {
            self.ensure_goal_executor_available()?;
        }
        let goal = {
            let _guard = self
                .goal_driver
                .admission
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.ensure_goal_activation_allowed()?;
            self.ensure_goal_driver_idle()?;
            if let Some(active_turn_id) = self.active_turn_id() {
                return Err(RuntimeError::Busy { active_turn_id });
            }
            if !self.goal_ctx.tool_surface.has("GoalRead") {
                return Err(RuntimeError::Configuration(
                    "goal requires GoalRead in the resolved tool surface".into(),
                ));
            }
            self.goal_ctx
                .goal_store
                .confirm(
                    id,
                    revision,
                    goal_environment(&self.goal_ctx.cwd, &self.goal_ctx.tool_surface),
                )
                .map_err(command_error)?
        };
        self.announce_goal(&goal);
        if let Err(error) = self.start_goal_run(lease) {
            self.fault_after_failed_activation(&goal, &error);
            return Err(error);
        }
        Ok(goal)
    }
    /// Read-only exploration and proposal generation; no activation occurs.
    pub async fn generate_goal_draft(&self, feedback: &str) -> RuntimeResult<GoalDraftSnapshot> {
        let current = self
            .goal_draft_snapshot()
            .ok_or_else(|| RuntimeError::Command("no goal draft".into()))?;
        self.generate_goal_draft_at(&current.id, current.revision, feedback)
            .await
    }
    /// Refine exactly the draft version the host displayed, rejecting stale requests.
    pub async fn generate_goal_draft_at(
        &self,
        id: &str,
        revision: u64,
        feedback: &str,
    ) -> RuntimeResult<GoalDraftSnapshot> {
        if !self.goal_ctx.tool_surface.has("GoalDraft") {
            return Err(RuntimeError::Configuration(
                "draft generation requires GoalDraft in the resolved tool surface".into(),
            ));
        }
        let goal = {
            let _guard = self
                .goal_driver
                .admission
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.ensure_goal_activation_allowed()?;
            if let Some(active_turn_id) = self.active_turn_id() {
                return Err(RuntimeError::Busy { active_turn_id });
            }
            let current = self
                .goal_draft_snapshot()
                .ok_or_else(|| RuntimeError::Command("no goal draft".into()))?;
            if current.id != id || current.revision != revision {
                return Err(RuntimeError::Command(
                    "stale draft; reread before refining".into(),
                ));
            }
            self.goal_ctx
                .goal_store
                .change(&current.id, current.revision, |g| {
                    g.draft_ready = false;
                    Ok(())
                })
                .map_err(command_error)?
        };
        self.announce_goal(&goal);
        let prompt = format!(
            "<goal-draft>\nOriginal request: {}\nFeedback: {}\nRead GoalRead. Explore only as needed, preserve every original requirement, and propose a one-level hierarchy of subgoals and observable acceptance criteria with required evidence. Save the full proposal with GoalDraft using the current ID and revision. Ask for clarification if needed; do not execute the proposed work. End by showing the contract and asking the human to confirm its exact version. Only the host can confirm.\n</goal-draft>",
            goal.spec.original_request, feedback
        );
        let mut stream = self.stream_goal_draft_discussion(
            prompt,
            GoalDraftDiscussion {
                goal_id: goal.id.clone(),
                revision: goal.revision,
                feedback: feedback.into(),
            },
        )?;
        while let Some(event) = stream.recv().await {
            if let Some(sink) = &self.goal_driver.sink {
                sink.on_event(event)
                    .await
                    .map_err(|e| RuntimeError::Command(e.to_string()))?;
            }
        }
        let outcome = stream.outcome().await?;
        if outcome.status != crate::runtime::TurnStatus::Ok {
            return Err(RuntimeError::Command(format!(
                "draft generation stopped: {:?}",
                outcome.status
            )));
        }
        let draft = self
            .goal_draft_snapshot()
            .ok_or_else(|| RuntimeError::Command("draft changed while generating".into()))?;
        self.announce_goal(&draft);
        Ok(draft)
    }
    pub fn pause_goal(&self, id: &str, revision: u64) -> RuntimeResult<GoalSnapshot> {
        self.goal_control(
            id,
            revision,
            |g| {
                ensure!(
                    !g.status.is_terminal() && g.status != GoalStatus::Draft,
                    "confirm or cancel a draft before pausing"
                );
                g.status = GoalStatus::Paused;
                g.review_stage = None;
                g.reason = Some("paused by user".into());
                Ok(())
            },
            true,
        )
    }
    pub fn cancel_goal(&self, id: &str, revision: u64) -> RuntimeResult<GoalSnapshot> {
        self.goal_control(
            id,
            revision,
            |g| {
                ensure!(!g.status.is_terminal(), "goal has ended");
                g.status = GoalStatus::Cancelled;
                g.review_stage = None;
                g.reason = Some("cancelled by user".into());
                Ok(())
            },
            true,
        )
    }
    pub fn resume_goal(&self, id: &str, revision: u64) -> RuntimeResult<GoalSnapshot> {
        self.resume_goal_inner(id, revision, None)
    }

    /// Resume a goal and hand its run a host-managed capacity lease.
    pub fn resume_and_start_goal(
        &self,
        id: &str,
        revision: u64,
        lease: Box<dyn GoalRunLease>,
    ) -> RuntimeResult<GoalSnapshot> {
        self.resume_goal_inner(id, revision, Some(lease))
    }

    fn resume_goal_inner(
        &self,
        id: &str,
        revision: u64,
        lease: Option<Box<dyn GoalRunLease>>,
    ) -> RuntimeResult<GoalSnapshot> {
        self.ensure_goal_activation_allowed()?;
        self.ensure_goal_submission_available()?;
        if lease.is_some() {
            self.ensure_host_activation_ready()?;
        } else {
            // Resume always intends to start a run.
            self.ensure_goal_executor_available()?;
        }
        let g = self.goal_control(
            id,
            revision,
            |g| {
                // `active` is the restored goal of a crashed run: recovery
                // leaves it idle and only an explicit resume starts a run
                // again. A live run is refused earlier (the host activation
                // check or the active-turn check below), so this branch can
                // never start a second run.
                ensure!(
                    g.status.can_resume(),
                    "goal cannot resume from status {:?}; drafts require confirmation and completed/cancelled goals cannot resume. Original reason: {}",
                    g.status,
                    g.reason.as_deref().unwrap_or("not recorded")
                );
                ensure!(
                    self.active_turn_id().is_none(),
                    "previous goal execution has not exited"
                );
                ensure!(
                    g.pending_effects.is_empty(),
                    "side effect result unknown; explicit reconciliation required"
                );
                let retry_review = g.review_failure_reason.is_some() || g.phase == GoalPhase::Verifying;
                ensure!(
                    if retry_review {
                        g.review_budget_reason().is_none()
                    } else {
                        g.budget_reason().is_none()
                    },
                    "adjust exhausted goal limits before resume"
                );
                ensure!(
                    g.environment
                        == goal_environment(&self.goal_ctx.cwd, &self.goal_ctx.tool_surface),
                    "goal environment changed; cancel and recreate with the new permissions"
                );
                ensure!(
                    !g.needs_confirmation && g.draft_duration_secs.is_none(),
                    "goal requires explicit contract confirmation"
                );
                g.status = GoalStatus::Active;
                g.hidden = false;
                g.reason = None;
                g.phase = if retry_review { GoalPhase::Verifying } else { GoalPhase::Idle };
                g.review_failure_reason = None;
                g.review_failure_stage = None;
                Ok(())
            },
            false,
        )?;
        if let Err(error) = self.start_goal_run(lease) {
            self.fault_after_failed_activation(&g, &error);
            return Err(error);
        }
        Ok(g)
    }

    fn ensure_goal_submission_available(&self) -> RuntimeResult<()> {
        if !self.goal_ctx.tool_surface.has("GoalReview") {
            return Err(RuntimeError::Configuration(
                "goal requires GoalReview in the host's resolved tool surface".into(),
            ));
        }
        Ok(())
    }
    pub fn update_goal_limits(
        &self,
        id: &str,
        revision: u64,
        limits: GoalLimits,
    ) -> RuntimeResult<GoalSnapshot> {
        limits.validate().map_err(command_error)?;
        self.goal_control(
            id,
            revision,
            |g| {
                ensure!(
                    g.status.can_adjust_limits(),
                    "completed or cancelled goal limits cannot change"
                );
                if g.status == GoalStatus::Draft && g.draft_duration_secs.is_some() {
                    g.draft_duration_secs =
                        Some(limits.deadline_unix_secs.saturating_sub(goal_now()).max(1));
                }
                g.spec.limits = limits;
                Ok(())
            },
            false,
        )
    }
    /// Explicit host reconciliation after investigating an unknown side effect.
    /// Does not synthesize tool results or automatically resume execution.
    pub fn reconcile_goal_effects(
        &self,
        id: &str,
        revision: u64,
        evidence: String,
    ) -> RuntimeResult<GoalSnapshot> {
        self.goal_control(
            id,
            revision,
            |g| {
                ensure!(
                    matches!(
                        g.status,
                        GoalStatus::Blocked | GoalStatus::Draft | GoalStatus::Failed
                    ) && !g.pending_effects.is_empty(),
                    "no unknown effects to reconcile"
                );
                ensure!(
                    !evidence.trim().is_empty(),
                    "reconciliation requires human evidence"
                );
                g.pending_effects.clear();
                g.reason = Some(format!("human effect reconciliation: {evidence}"));
                Ok(())
            },
            false,
        )
    }
    pub fn clear_goal(&self, id: &str, revision: u64) -> RuntimeResult<GoalSnapshot> {
        self.goal_control(
            id,
            revision,
            |g| {
                ensure!(g.status.is_terminal(), "only ended goals can be cleared");
                g.hidden = true;
                Ok(())
            },
            false,
        )
    }
    pub fn approve_goal(
        &self,
        id: &str,
        revision: u64,
        evidence: Vec<String>,
    ) -> RuntimeResult<GoalSnapshot> {
        // Evidence must be present and non-blank: `vec![]`, `vec![""]` and
        // `vec!["   "]` must all fail, or the recorded basis would be empty.
        if evidence.is_empty() || evidence.iter().any(|item| item.trim().is_empty()) {
            return Err(RuntimeError::Command(
                "human approval requires evidence".into(),
            ));
        }
        self.goal_control(
            id,
            revision,
            |g| {
                ensure!(
                    g.spec.acceptance == GoalAcceptance::Human
                        && g.status == GoalStatus::Blocked
                        && g.phase == GoalPhase::Verifying
                        && g.reviews
                            .last()
                            .is_some_and(|r| r.verdict == GoalReviewVerdict::Satisfied),
                    "no pending human acceptance"
                );
                ensure!(
                    self.validate_review_evidence(g)? == g.evidence_versions,
                    "completion evidence changed since human acceptance was requested"
                );
                if g.spec.checker_id.is_some() {
                    ensure!(
                        self.checker_snapshot_matches(g)?,
                        "configured checker has not passed this candidate"
                    );
                }
                ensure!(
                    goal_now() < g.spec.limits.deadline_unix_secs,
                    "goal deadline expired; explicitly adjust the budget before approval"
                );
                g.status = GoalStatus::Completed;
                g.phase = GoalPhase::Idle;
                g.completion_basis = Some(GoalCompletionBasis::HumanApproved);
                g.reason = Some(evidence.join("\n"));
                Ok(())
            },
            false,
        )
    }
    fn goal_control(
        &self,
        id: &str,
        revision: u64,
        f: impl FnOnce(&mut GoalSnapshot) -> Result<()>,
        interrupt: bool,
    ) -> RuntimeResult<GoalSnapshot> {
        let _guard = self
            .goal_driver
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Err(error) = self.ensure_goal_control_open() {
            // A latched session cannot persist a stop intent either: latch the
            // stop so the run does not keep executing on a command that was
            // refused.
            if interrupt {
                self.stop_run_after_failed_stop_intent(&format!("{error}"));
            }
            return Err(error);
        }
        let g = match self.goal_ctx.goal_store.change_classified(id, revision, f) {
            Ok(goal) => goal,
            Err(GoalChangeError::Rejected(error)) => {
                // A stale revision or an invalid transition rejects the
                // command, not the run: it must not stop a live execution.
                return Err(command_error(error));
            }
            Err(GoalChangeError::Publish(error)) => {
                // The intent could not be stored. A stop command must not read
                // as a saved pause: latch the stop for the current run so it
                // cannot keep scheduling or commit success, and report the
                // refusal instead of a success.
                if interrupt {
                    self.stop_run_after_failed_stop_intent(&format!("{error:#}"));
                }
                return Err(command_error(error));
            }
        };
        if interrupt {
            self.turn_gate.interrupt();
        }
        self.announce_goal(&g);
        Ok(g)
    }

    /// Latch a stop decision after a pause/cancel could not be persisted.
    ///
    /// The command layer cannot publish `Paused`/`Cancelled`, so the run must
    /// not keep scheduling or commit an acceptance result. The current run
    /// moves to `Stopping` with the original error (identity-bound), and the
    /// turn is interrupted; the run's single settlement publishes the stable
    /// `Faulted` state only after whatever holds the turn gate has actually
    /// exited.
    pub(crate) fn stop_run_after_failed_stop_intent(&self, error: &str) {
        let fault = format!("goal stop intent could not be persisted: {error}");
        {
            let mut control = self.goal_driver.lock_control();
            match &control.driver {
                DriverState::Running { run_id, goal_id } => {
                    let (run_id, goal_id) = (*run_id, goal_id.clone());
                    self.goal_driver.set_driver_locked(
                        &mut control,
                        DriverState::Stopping {
                            run_id,
                            goal_id,
                            reason: fault,
                        },
                    );
                }
                // The first failed stop already latched this run and its
                // reason; a second one must not rewrite it.
                DriverState::Stopping { .. } => {}
                _ => {}
            }
        }
        self.turn_gate.interrupt();
    }

    /// A run can only be started from a scheduler, and the contract must never
    /// be published `active` when that is known to be impossible: verify it
    /// before any state write, never after one.
    fn ensure_goal_executor_available(&self) -> RuntimeResult<()> {
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(RuntimeError::Command(
                "goal scheduling requires a running async runtime".into(),
            ));
        }
        Ok(())
    }

    /// Latch a stable fault when an activation wrote its contract but could
    /// not start the run.
    ///
    /// The write cannot be undone; leaving the driver `Idle` would present an
    /// `active` goal that nothing drives. The fault is terminal for this
    /// runtime (reopen to recover) and the caller still receives the error.
    fn fault_after_failed_activation(&self, goal: &GoalSnapshot, error: &RuntimeError) {
        let mut control = self.goal_driver.lock_control();
        if !matches!(control.driver, DriverState::Idle) {
            return;
        }
        let run_id = control.next_run_id;
        control.next_run_id = run_id.saturating_add(1);
        self.goal_driver.set_driver_locked(
            &mut control,
            DriverState::Faulted {
                run_id,
                goal_id: goal.id.clone(),
                error: format!("goal activation could not start a run: {error}"),
            },
        );
    }
    pub(crate) fn announce_goal(&self, goal: &GoalSnapshot) {
        self.event_display
            .emit(crate::runtime::AgentEventKind::GoalUpdated {
                goal: Box::new(goal.clone()),
            });
        if goal.status.is_terminal() {
            self.event_display
                .emit(crate::runtime::AgentEventKind::GoalFinished {
                    goal: Box::new(goal.clone()),
                });
        }
        self.goal_ctx
            .log_raw_event(serde_json::json!({"type":"goal_updated","goal":goal}));
        if let Some(stage) = goal.review_stage {
            let label = match stage {
                GoalReviewStage::Screening => "判断是否需要验收",
                GoalReviewStage::Inspecting => "检查完成证据",
                GoalReviewStage::Adjudicating => "宿主提交结论",
            };
            self.goal_ctx
                .display
                .render_info(&format!("Goal {}: {label}", goal.id));
        }
        self.goal_ctx.display.render_info(&format!(
            "Goal {:?}: {} ({} turns remaining; acceptance {:?}; basis {:?})",
            goal.status,
            goal.reason.as_deref().unwrap_or(&goal.spec.objective),
            goal.remaining_turns(),
            goal.spec.acceptance,
            goal.completion_basis
        ));
    }
    /// Watch whether a goal turn is currently executing.
    pub fn subscribe_goal_turns(&self) -> tokio::sync::watch::Receiver<bool> {
        self.goal_driver.turn_active.subscribe()
    }

    /// Enable goal scheduling. Rust starts with scheduling disabled.
    ///
    /// Arming a driver never starts a run. Hosts that account for execution
    /// capacity start runs through [`Self::create_and_start_goal`] /
    /// [`Self::resume_and_start_goal`], so every run they start carries its
    /// lease; lifecycle hosts without a capacity ledger use
    /// [`Self::start_goal_driver_and_run`] to pick up a restored `active` goal.
    pub fn start_goal_driver(&self) -> RuntimeResult<()> {
        self.ensure_goal_activation_allowed()?;
        self.goal_driver.lock_control().enabled = true;
        Ok(())
    }

    /// Enable scheduling and start a run for the current `active` goal.
    ///
    /// Call after UI subscriptions and input wiring. This is the lifecycle
    /// host's entry: it arms the driver and, if a goal was restored as
    /// `active`, starts exactly one run for it. Hosts that own execution
    /// capacity must not use this: their runs are started by
    /// [`Self::create_and_start_goal`] / [`Self::resume_and_start_goal`].
    pub fn start_goal_driver_and_run(&self) -> RuntimeResult<()> {
        self.start_goal_driver()?;
        self.schedule_goal()
    }

    /// Refuse activation when the driver already stopped for good.
    ///
    /// A latched persistence fault or a closed session is refused too: an
    /// active goal must never be left behind without a driver that can run it.
    fn ensure_goal_activation_allowed(&self) -> RuntimeResult<()> {
        self.ensure_goal_control_open()?;
        let control = self.goal_driver.lock_control();
        match &control.driver {
            DriverState::Closed => Err(RuntimeError::Closed),
            DriverState::Faulted { error, .. } => Err(RuntimeError::Command(format!(
                "goal driver stopped after an unrecoverable failure: {error}"
            ))),
            _ => Ok(()),
        }
    }

    /// Start a run without a host lease (best-effort lifecycle callers).
    pub(crate) fn schedule_goal(&self) -> RuntimeResult<()> {
        self.start_goal_run(None)
    }

    /// A host-managed activation must own the run it starts.
    ///
    /// An existing run would strand the lease while the old run keeps
    /// scheduling, a disabled driver would leave an active goal that nothing
    /// runs, and a missing scheduler would leave it active with no worker:
    /// all are refused before the lease is accepted and before any state is
    /// written.
    fn ensure_host_activation_ready(&self) -> RuntimeResult<()> {
        self.ensure_goal_driver_idle()?;
        if !self.goal_driver.lock_control().enabled {
            return Err(RuntimeError::Configuration(
                "goal driver is not enabled; start it before a host-managed activation".into(),
            ));
        }
        self.ensure_goal_executor_available()?;
        Ok(())
    }

    /// Refuse a host-managed activation while a run still owns the driver.
    fn ensure_goal_driver_idle(&self) -> RuntimeResult<()> {
        match self.goal_driver.state() {
            DriverState::Idle => Ok(()),
            DriverState::Closed => Err(RuntimeError::Closed),
            DriverState::Faulted { error, .. } => Err(RuntimeError::Command(format!(
                "goal driver stopped after an unrecoverable failure: {error}"
            ))),
            DriverState::Running { .. } | DriverState::Stopping { .. } => Err(RuntimeError::Busy {
                active_turn_id: crate::runtime::TurnId::new("goal run in progress"),
            }),
        }
    }

    /// Start one run if the driver is idle, enabled, and has active work.
    ///
    /// Unrecoverable failures are terminal for this runtime: the failed status
    /// may not be publishable (a latched journal), so a restart loop would spin
    /// on a disk state that still reads `active`. `Faulted` therefore refuses
    /// new runs until the session is reopened. A host lease travels with the
    /// admitted run: it lives in the RunGuard and is destroyed when the run
    /// ends or when this call refuses to start one.
    fn start_goal_run(&self, lease: Option<Box<dyn GoalRunLease>>) -> RuntimeResult<()> {
        // Verify the execution environment before mutating control state: a
        // spawn without a runtime would otherwise leave a run that never runs.
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(RuntimeError::Command(
                "goal scheduling requires a running async runtime".into(),
            ));
        }
        let started = {
            let mut control = self.goal_driver.lock_control();
            match &control.driver {
                DriverState::Closed => return Err(RuntimeError::Closed),
                DriverState::Running { .. } | DriverState::Stopping { .. } => {
                    if lease.is_some() {
                        // A host lease is only valid when this call starts the
                        // run; silently dropping it would under-count capacity.
                        return Err(RuntimeError::Command(
                            "a goal run is already scheduled".into(),
                        ));
                    }
                    return Ok(());
                }
                DriverState::Faulted { error, .. } => {
                    return Err(RuntimeError::Command(format!(
                        "goal driver stopped after an unrecoverable failure: {error}"
                    )));
                }
                DriverState::Idle => {}
            }
            if !control.enabled || self.is_closed() {
                if lease.is_some() {
                    return Err(if self.is_closed() {
                        RuntimeError::Closed
                    } else {
                        RuntimeError::Command("goal driver is not enabled".into())
                    });
                }
                return Ok(());
            }
            self.ensure_goal_control_open()?;
            let Some(goal) = self
                .goal_snapshot()
                .filter(|g| g.status == GoalStatus::Active)
            else {
                if lease.is_some() {
                    return Err(RuntimeError::Command(
                        "no active goal is available to run".into(),
                    ));
                }
                return Ok(());
            };
            let run_id = control.next_run_id;
            control.next_run_id = run_id.saturating_add(1);
            let goal_id = goal.id.clone();
            self.goal_driver.set_driver_locked(
                &mut control,
                DriverState::Running {
                    run_id,
                    goal_id: goal_id.clone(),
                },
            );
            Some((run_id, goal_id))
        };
        let Some((run_id, goal_id)) = started else {
            return Ok(());
        };
        let handle = self.clone();
        tokio::spawn(async move {
            handle.run_goal_worker(run_id, goal_id, lease).await;
        });
        Ok(())
    }
}
