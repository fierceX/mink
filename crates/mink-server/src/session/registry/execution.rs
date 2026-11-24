use super::*;

impl Registry {
    pub fn start_turn(
        &self,
        id: &str,
        project: Option<&str>,
        input: String,
        request_id: Option<String>,
    ) -> RegistryResult<()> {
        self.start_turn_inner(id, project, input, request_id)
    }

    fn start_turn_inner(
        &self,
        id: &str,
        project: Option<&str>,
        input: String,
        request_id: Option<String>,
    ) -> RegistryResult<()> {
        let locator = self.required_active_locator(id, project)?;
        let runtime = {
            let active = self.lock_active_state()?;
            active
                .get(&locator)
                .ok_or_else(|| RegistryError::NotFound(format!("session {id} is not open")))?
                .runtime
                .clone()
        };
        if runtime.closed() {
            return Err(RegistryError::NotFound(format!(
                "session {id} runtime is closed; reopen it"
            )));
        }
        if let Some(request_id) = request_id.as_deref()
            && runtime
                .existing_turn_input(request_id, &input)
                .map_err(|error| RegistryError::Busy(error.to_string()))?
                .is_some()
        {
            return Ok(());
        }
        let instance = self.instance_key(&locator);
        let permit = self
            .capacity
            .try_reserve(instance, CapacityOwner::OrdinaryTurn)
            .map_err(|error| self.capacity_error(id, error))?;
        runtime
            .start_turn_with_permit(input, request_id, Some(permit))
            .map_err(|error| RegistryError::Busy(error.to_string()))
    }

    /// Identity of the session instance a permit belongs to.
    fn instance_key(&self, locator: &SessionLocator) -> SessionInstanceKey {
        let mut generations = self.generations.lock().unwrap_or_else(|e| e.into_inner());
        let generation = *generations.entry(locator.clone()).or_insert(1);
        SessionInstanceKey {
            project_key: locator.project_key.clone(),
            id: locator.id.clone(),
            generation,
        }
    }

    fn capacity_error(&self, id: &str, error: ReserveError) -> RegistryError {
        match error {
            ReserveError::Full { limit, .. } => {
                RegistryError::Capacity(format!("too many running sessions (limit {limit})"))
            }
            ReserveError::Occupied => RegistryError::Busy(format!(
                "session {id} already has a turn or goal run in progress"
            )),
        }
    }

    /// Reserve the single capacity slot a goal run will own.
    ///
    /// The lease is destroyed by core when the activation is refused, so the
    /// caller never rolls a reservation back by hand.
    pub(crate) async fn reserve_goal_run(
        &self,
        id: &str,
        project: Option<&str>,
    ) -> RegistryResult<PermitLease> {
        let locator = self.required_active_locator(id, project)?;
        let instance = self.instance_key(&locator);
        // A previous run of the same instance may still be exiting (a paused
        // turn settles within milliseconds). Wait on Notify for release instead
        // of sleeping in a polling loop.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1_000);
        loop {
            let notified = self.capacity.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self
                .capacity
                .try_reserve(instance.clone(), CapacityOwner::GoalDriver)
            {
                Ok(permit) => return Ok(PermitLease::new(permit)),
                Err(error @ ReserveError::Full { .. }) => {
                    return Err(self.capacity_error(id, error));
                }
                Err(ReserveError::Occupied) => {
                    if tokio::time::timeout_at(deadline, notified).await.is_err() {
                        return Err(self.capacity_error(id, ReserveError::Occupied));
                    }
                }
            }
        }
    }

    /// Record that a stop was requested for this session's running execution.
    ///
    /// The permit itself belongs to the run; the ledger entry is marked
    /// draining so capacity stays occupied until the execution actually
    /// exits.
    pub(crate) fn mark_goal_draining(&self, id: &str, project: Option<&str>) -> bool {
        let Ok(locator) = self.required_active_locator(id, project) else {
            return false;
        };
        let instance = self.instance_key(&locator);
        self.capacity.mark_draining(&instance)
    }

    /// Serialise goal creation/control commands on one session.
    ///
    /// The reservation is only safe to roll back if no other attempt can
    /// create or drop a goal for this session in between: holding this lock
    /// from the reserve to the rollback makes "this attempt created nothing"
    /// a stable fact for the whole attempt.
    pub async fn lock_goal_commands(
        &self,
        id: &str,
        project: Option<&str>,
    ) -> RegistryResult<tokio::sync::OwnedMutexGuard<()>> {
        let locator = self.required_active_locator(id, project)?;
        Ok(self.operation_lock(&locator).lock_owned().await)
    }

    /// Whether a goal checker is configured for this deployment.
    ///
    /// The workbench must not offer checker acceptance the server cannot
    /// honour, and the capability is what lets it decide.
    pub fn submit_input(
        &self,
        id: &str,
        project: Option<&str>,
        input: mink::runtime::HumanInput,
        resume: Option<(&str, u64)>,
    ) -> RegistryResult<mink::runtime::InputReceipt> {
        let locator = self.required_active_locator(id, project)?;
        let runtime = {
            let active = self.lock_active_state()?;
            active
                .get(&locator)
                .ok_or_else(|| RegistryError::NotFound("session is not open".into()))?
                .runtime
                .clone()
        };
        let existing = if resume.is_none() {
            runtime
                .inbox()
                .existing(&input)
                .map_err(|e| RegistryError::Busy(e.to_string()))?
        } else {
            None
        };
        if let Some(receipt) = existing {
            return Ok(receipt);
        }
        // Resuming an unapplied input starts a new turn even when its original
        // input was guidance targeting an older turn. Only live guidance can
        // share an already admitted running session's slot, and it does not
        // take a second one.
        let permit = if resume.is_some() || input.target_turn_id.is_none() {
            let instance = self.instance_key(&locator);
            Some(
                self.capacity
                    .try_reserve(instance, CapacityOwner::OrdinaryTurn)
                    .map_err(|error| self.capacity_error(id, error))?,
            )
        } else {
            None
        };
        runtime
            .submit(input, resume, permit)
            .map_err(|e| RegistryError::Busy(e.to_string()))
    }
    pub fn interrupt(&self, id: &str, project: Option<&str>) -> RegistryResult<()> {
        self.interrupt_inner(id, project)
    }

    fn interrupt_inner(&self, id: &str, project: Option<&str>) -> RegistryResult<()> {
        let locator = self.required_active_locator(id, project)?;
        let active = self.lock_active_state()?;
        let session = active
            .get(&locator)
            .ok_or_else(|| RegistryError::NotFound(format!("session {id} is not open")))?;
        session.runtime.interrupt();
        Ok(())
    }
}
