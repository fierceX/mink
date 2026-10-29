//! Goal driver: iteration scheduling, the drive loop and run settlement.

use super::*;

impl AgentRuntimeHandle {
    /// One run: drive to a stop, then settle the lifecycle exactly once.
    pub(crate) async fn run_goal_worker(
        &self,
        run_id: u64,
        goal_id: String,
        lease: Option<Box<dyn GoalRunLease>>,
    ) {
        let guard = RunGuard {
            handle: self.clone(),
            run_id,
            goal_id: goal_id.clone(),
            lease,
            finished: false,
        };
        let fault = match self.drive_goal(run_id).await {
            // A stop intent that could not be persisted latched this run in
            // `Stopping`: the run ends, but it must settle as the stable fault
            // carrying that reason, not as a clean finish.
            Ok(()) => self.latched_stop_fault(run_id),
            Err(error) => {
                // At most one attempt to persist `failed` for the same
                // identity; if even that cannot be published the run still
                // stops, with both causes reported.
                let (publish_error, replaced) =
                    match self.goal_ctx.goal_store.transition_if_current(
                        &goal_id,
                        GoalTransition::FailRun {
                            reason: format!("{error:#}"),
                        },
                    ) {
                        Ok(Some(goal)) => {
                            self.announce_goal(&goal);
                            (None, false)
                        }
                        // The goal was replaced while this run failed: the error
                        // belongs to the old identity and must not poison the
                        // replacement's run.
                        Ok(None) => (None, true),
                        Err(publish_error) => (Some(format!("{publish_error:#}")), false),
                    };
                let fault = match publish_error {
                    Some(publish_error) => format!(
                        "goal run failed: {error:#}; persisting the failed status also failed: {publish_error}"
                    ),
                    None => format!("goal run failed: {error:#}"),
                };
                self.goal_ctx
                    .display
                    .render_error(&format!("Goal runner: {fault}"));
                if replaced {
                    None
                } else {
                    // A failed stop intent that already latched this run keeps
                    // its reason: that is the stable cause, the run error is
                    // secondary.
                    Some(match self.latched_stop_fault(run_id) {
                        Some(stop) => format!("{stop}; run error: {error:#}"),
                        None => fault,
                    })
                }
            }
        };
        // The run may have ended the goal: publish the terminal facts once, at
        // the tail, so the model sees the outcome without any request prefix
        // being rewritten. A failed informational append is reported, never
        // turned into a lifecycle change.
        if let Err(error) = self
            .goal_ctx
            .goal_store
            .append_ended_row(&self.goal_ctx.store, self.goal_projection_chars())
            .await
        {
            self.goal_ctx.display.render_error(&format!(
                "Goal runner: terminal facts not recorded: {error:#}"
            ));
        }
        if let Some(reason) = fault {
            // Execution work (an interrupted tool or checker) may still be
            // exiting. Report the stop while it finishes, and only then settle
            // the fault with the run's single completion path: the fault is
            // stable only once the run owns nothing anymore.
            if self.active_turn_id().is_some() {
                {
                    let mut control = self.goal_driver.lock_control();
                    if matches!(
                        &control.driver,
                        DriverState::Running { run_id: current, .. } if *current == run_id
                    ) {
                        self.goal_driver.set_driver_locked(
                            &mut control,
                            DriverState::Stopping {
                                run_id,
                                goal_id: goal_id.clone(),
                                reason: reason.clone(),
                            },
                        );
                    }
                }
                self.turn_gate.wait_idle().await;
            }
            guard.finish(Some(reason));
        } else {
            guard.finish(None);
        }
    }

    /// Whether a new goal iteration may be admitted right now.
    ///
    /// Read under the admission lock by the iteration admission path: once a
    /// stop decision latches `Stopping`, no new execution may start, only the
    /// execution already running may finish and settle the stable fault.
    pub(crate) fn goal_iteration_allowed(&self) -> bool {
        matches!(self.goal_driver.state(), DriverState::Running { .. })
    }

    /// Whether `run_id` is still the run this runtime schedules work for.
    ///
    /// The stop decision and any acceptance commit share the admission
    /// boundary; both sides use this predicate inside it, so a stop latched in
    /// memory can never coexist with a committed success.
    pub(crate) fn run_scheduled(&self, run_id: u64) -> bool {
        matches!(
            self.goal_driver.state(),
            DriverState::Running { run_id: current, .. } if current == run_id
        )
    }

    /// The stop reason a failed pause/cancel latched for this run, if any.
    ///
    /// Read once at settlement: the reason must survive the run's normal
    /// return (never be rewritten to a clean `Idle`) and only becomes the
    /// stable `Faulted` after the execution it owns actually exits.
    fn latched_stop_fault(&self, run_id: u64) -> Option<String> {
        let control = self.goal_driver.lock_control();
        match &control.driver {
            DriverState::Stopping {
                run_id: current,
                reason,
                ..
            } if *current == run_id => Some(reason.clone()),
            _ => None,
        }
    }

    /// Settle a finished run by identity: a late cleanup can never clear a
    /// newer run, and only a clean finish may chain into the next run.
    pub(crate) fn finish_goal_run(&self, run_id: u64, goal_id: &str, fault: Option<String>) {
        let failed = fault.is_some();
        let changed = {
            let mut control = self.goal_driver.lock_control();
            let matches_run = matches!(
                &control.driver,
                DriverState::Running { run_id: current, .. }
                    | DriverState::Stopping { run_id: current, .. } if *current == run_id
            );
            if !matches_run {
                false
            } else {
                let next = match fault {
                    Some(error) => DriverState::Faulted {
                        run_id,
                        goal_id: goal_id.to_string(),
                        error,
                    },
                    None => DriverState::Idle,
                };
                self.goal_driver.set_driver_locked(&mut control, next);
                true
            }
        };
        if changed && !failed {
            // Work may remain (a replacement goal, or a resume racing the
            // finish). A clean finish chains into it; an error never restarts.
            if let Err(error) = self.schedule_goal() {
                self.goal_ctx
                    .display
                    .render_error(&format!("Goal runner: {error:#}"));
            }
        }
    }

    pub async fn wait_goal(&self) -> RuntimeResult<GoalOutcome> {
        let mut changes = self.goal_driver.changes.subscribe();
        loop {
            match changes.borrow_and_update().clone() {
                DriverState::Idle => {
                    return self
                        .goal_snapshot()
                        .ok_or_else(|| RuntimeError::Command("no goal".into()));
                }
                DriverState::Closed => return Err(RuntimeError::Closed),
                DriverState::Faulted { error, .. } => {
                    return Err(RuntimeError::Command(format!(
                        "goal driver faulted: {error}"
                    )));
                }
                DriverState::Running { .. } | DriverState::Stopping { .. } => {}
            }
            if changes.changed().await.is_err() {
                return Err(RuntimeError::Closed);
            }
        }
    }
    async fn drive_goal(&self, run_id: u64) -> Result<()> {
        #[cfg(test)]
        self.goal_driver
            .drive_attempts
            .fetch_add(1, Ordering::SeqCst);
        while let Some(goal) = self
            .goal_snapshot()
            .filter(|g| g.status == GoalStatus::Active)
        {
            // A failed stop intent latches the run into `Stopping`: no further
            // iteration may be admitted, only the execution already running
            // may finish and settle the stable fault.
            if !self.run_scheduled(run_id) {
                break;
            }
            let checker_id = match &goal.spec.acceptance {
                GoalAcceptance::Verifier { id } => Some(id),
                _ => goal.spec.checker_id.as_ref(),
            };
            if let Some(id) = checker_id
                && !self.goal_driver.verifiers.iter().any(|v| v.id() == id)
            {
                let Some(updated) = self.goal_ctx.goal_store.transition_if_current(
                    &goal.id,
                    GoalTransition::MarkVerifierUnavailable {
                        verifier_id: id.clone(),
                    },
                )?
                else {
                    return Ok(());
                };
                self.announce_goal(&updated);
                break;
            }
            if self.goal_ctx.cancel.is_cancelled() || self.is_closed() {
                break;
            }
            if goal.phase == GoalPhase::Verifying
                && goal_now() < goal.spec.limits.deadline_unix_secs
            {
                self.accept_goal(run_id, goal).await?;
                continue;
            }
            if let Some(reason) = goal.budget_reason() {
                let Some(updated) = self.goal_ctx.goal_store.transition_if_current(
                    &goal.id,
                    GoalTransition::ExhaustBudget {
                        reason: reason.into(),
                    },
                )?
                else {
                    return Ok(());
                };
                self.announce_goal(&updated);
                break;
            }
            let stream =
                match self.stream_goal_iteration(self.goal_ctx.store.committed_len().await?) {
                    Ok(stream) => stream,
                    Err(RuntimeError::Closed) => break,
                    Err(error) => {
                        if self
                            .goal_snapshot()
                            .is_some_and(|g| g.status != GoalStatus::Active)
                        {
                            break;
                        }
                        // A stop latched while the iteration was being admitted:
                        // no new work may start, only the stable fault may settle.
                        if !self.run_scheduled(run_id) {
                            break;
                        }
                        return Err(error.into());
                    }
                };

            self.goal_driver.turn_active.send_replace(true);
            // Activity must be cleared on every exit path. A sink error or a
            // failed outcome returns early with `?`, which used to skip the
            // clear and left subscribers believing a goal turn was still
            // running after the driver had already stopped.
            let _turn = TurnActivityGuard {
                active: self.goal_driver.turn_active.clone(),
            };
            let mut stream = stream;
            let outcome = async {
                while let Some(event) = stream.recv().await {
                    if let Some(sink) = &self.goal_driver.sink {
                        sink.on_event(event)
                            .await
                            .map_err(|e| RuntimeError::Command(format!("goal event sink: {e}")))?;
                    }
                }
                stream.outcome().await
            };
            tokio::pin!(outcome);
            let settled_goal_id = goal.id.clone();
            let _result = tokio::select! {
                result = &mut outcome => result?,
                _ = self.wait_goal_deadline() => {
                    if let Some(budget) = self.goal_ctx.goal_store.transition_if_current(
                        &goal.id,
                        GoalTransition::ExhaustBudget {
                            reason: "goal deadline reached during execution".into(),
                        },
                    )? {
                        self.announce_goal(&budget);
                    }
                    self.turn_gate.interrupt();
                    outcome.await?
                }
            };
            // The orchestrator settled this iteration by identity at the
            // persistence boundary; the driver only reads the outcome and
            // applies the report-driven next action.
            let Some(g) = self.goal_ctx.goal_store.snapshot() else {
                return Ok(());
            };
            if g.id != settled_goal_id {
                // The goal was replaced while this round settled; its usage and
                // decisions belong to the goal that started the round.
                return Ok(());
            }
            if g.status != GoalStatus::Active || self.is_closed() {
                self.announce_goal(&g);
                break;
            }
            if g.phase == GoalPhase::Verifying {
                self.accept_goal(run_id, g).await?;
            }
        }
        Ok(())
    }
    /// Wait until the goal's current deadline, re-reading it whenever the host
    /// changes the limits. Review gets a bounded per-attempt cap, applied to
    /// whichever deadline is in force.
    pub(crate) async fn wait_verification_deadline(&self, goal: &GoalSnapshot) {
        // Two independent reasons to stop: a fixed per-attempt cap measured
        // from the start of this acceptance, and the goal's absolute deadline,
        // which the host may shorten at any time. Neither may reset the other,
        // so the cap is a monotonic deadline rather than a per-tick countdown.
        let attempt_deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(super::GOAL_REVIEW_TIMEOUT_SECS);
        let mut updates = self.subscribe_goal();
        loop {
            let absolute = updates
                .borrow_and_update()
                .as_ref()
                .map(|current| current.spec.limits.deadline_unix_secs)
                .unwrap_or(goal.spec.limits.deadline_unix_secs);
            let until_absolute =
                std::time::Duration::from_secs(absolute.saturating_sub(goal_now()));
            if until_absolute.is_zero() {
                return;
            }
            let now = tokio::time::Instant::now();
            if now >= attempt_deadline {
                return;
            }
            let next = (attempt_deadline.min(now + until_absolute)).saturating_duration_since(now);
            tokio::select! {
                _ = tokio::time::sleep(next) => {},
                result = updates.changed() => if result.is_err() { return; },
            }
        }
    }

    async fn wait_goal_deadline(&self) {
        let mut updates = self.subscribe_goal();
        loop {
            let deadline = updates
                .borrow_and_update()
                .as_ref()
                .map(|g| g.spec.limits.deadline_unix_secs);
            let Some(deadline) = deadline else {
                return;
            };
            if goal_now() >= deadline {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(deadline.saturating_sub(goal_now()))) => {},
                result = updates.changed() => if result.is_err() {return;},
            }
        }
    }
    pub(crate) fn goal_projection_chars(&self) -> usize {
        goal_projection_chars(self.goal_ctx.config.max_context_tokens)
    }
}
