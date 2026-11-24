use super::*;

/// Read the goal, preferring the live runtime over the persisted file so a
/// session the server is driving reports its real state instead of a stale disk
/// read. Mutations go through the same runtime handle, so the core contract —
/// lease ownership, revision checks and acceptance rules — stays authoritative.
pub(super) async fn get_goal(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    if let Ok(Some(runtime)) = state.registry.active_runtime(&id, query.project.as_deref())
        && let Some(goal) = runtime.goal_snapshot()
    {
        // A cleared (hidden) goal reads as null here exactly like it does in
        // the session snapshot, the goal_updated event and the disk fallback:
        // one normalization rule for every read surface.
        let goal = (!goal.hidden).then_some(goal);
        let goal = match (goal, query.full) {
            (Some(goal), Some(true)) => json!(goal),
            (Some(goal), _) => crate::session::runtime::goal_summary_value(&json!(goal)),
            (None, _) => serde_json::Value::Null,
        };
        return ApiResponse::ok(
            json!({ "goal": goal, "driven": true, "verifier": state.registry.verifier_configured() }),
        );
    }
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let text = match resource_text(&dir, "goal.json").await {
        Ok(Some(text)) => text,
        Ok(None) => {
            return ApiResponse::ok(
                json!({ "goal": serde_json::Value::Null, "driven": false, "verifier": state.registry.verifier_configured() }),
            );
        }
        Err(response) => return response,
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => {
            // `goal.json` stores {version, current, history}; the workbench only
            // needs the live snapshot. A cleared (hidden) goal is normalized to
            // null so the persisted path matches the live read.
            let goal = value
                .get("current")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let goal = if goal.get("hidden").and_then(serde_json::Value::as_bool) == Some(true) {
                serde_json::Value::Null
            } else if query.full == Some(true) {
                goal
            } else {
                crate::session::runtime::goal_summary_value(&goal)
            };
            ApiResponse::ok(
                json!({ "goal": goal, "driven": false, "verifier": state.registry.verifier_configured() }),
            )
        }
        Err(e) => ApiResponse::err(500, format!("goal.json parse error: {e}")),
    }
}

#[derive(serde::Deserialize)]
pub(super) struct CreateGoalBody {
    #[serde(default)]
    protocol_version: Option<u32>,
    #[serde(default)]
    generate: Option<bool>,
    #[serde(default)]
    draft_spec: Option<mink::runtime::GoalDraftSpec>,
    objective: String,
    /// `agent_reviewed` (default), `verifier` or `human`.
    #[serde(default)]
    acceptance: String,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    max_minutes: Option<u64>,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    constraints: Vec<String>,
}

/// Create a draft on an open session; explicit confirmation activates execution.
///
/// The browser is a host here, not the model: limits and acceptance mode are
/// validated by the core contract, and the model still cannot change them.
pub(super) async fn create_goal(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
    body: Option<axum::Json<CreateGoalBody>>,
) -> ApiResponse {
    let Some(body) = body else {
        return ApiResponse::err(400, "goal body is required");
    };
    let runtime = match state.registry.active_runtime(&id, query.project.as_deref()) {
        Ok(Some(runtime)) => runtime,
        Ok(None) => {
            return ApiResponse::err(409, "session is not open; open it before creating a goal");
        }
        Err(error) => return registry_error(error),
    };
    let handle = match runtime.goal_handle() {
        Some(handle) => handle,
        None => return ApiResponse::err(409, "session runtime is closing"),
    };
    if body.protocol_version != Some(2) {
        return ApiResponse::err(
            409,
            "goal protocol upgraded; protocol_version 2 and draft confirmation are required",
        );
    }
    let objective = body.objective.trim();
    if objective.is_empty() {
        return ApiResponse::err(400, "goal objective is empty");
    }
    if objective.len() > 128 * 1024
        || body.constraints.len() > 64
        || body.acceptance_criteria.len() > 64
        || body.constraints.iter().any(|c| c.len() > 16 * 1024)
        || body.acceptance_criteria.iter().any(|c| c.len() > 16 * 1024)
    {
        return ApiResponse::err(400, "goal specification exceeds size limits");
    }
    let mut spec = mink::runtime::GoalSpec::new(objective);
    if !body.acceptance_criteria.is_empty() {
        spec.acceptance_criteria = body.acceptance_criteria.clone();
        spec.criteria = body
            .acceptance_criteria
            .iter()
            .enumerate()
            .map(|(i, outcome)| mink::runtime::GoalCriterion {
                id: format!("criterion-{}", i + 1),
                subgoal_id: "subgoal-1".into(),
                outcome: outcome.clone(),
                required_evidence: "Concrete files or recorded verification results".into(),
            })
            .collect();
    }
    spec.constraints = body.constraints.clone();
    spec.acceptance = match body.acceptance.as_str() {
        "human" => mink::runtime::GoalAcceptance::Human,
        "verifier" => match handle.goal_verifier_ids().into_iter().next() {
            Some(id) => mink::runtime::GoalAcceptance::Verifier { id },
            None => return ApiResponse::err(409, "no goal verifier is configured"),
        },
        "agent_reviewed" | "" => mink::runtime::GoalAcceptance::AgentReviewed,
        other => return ApiResponse::err(400, format!("unknown acceptance mode {other}")),
    };
    if let Some(draft) = &body.draft_spec {
        spec.objective = draft.objective.clone();
        spec.scope = draft.scope.clone();
        spec.constraints = draft.constraints.clone();
        spec.subgoals = draft.subgoals.clone();
        spec.criteria = draft.criteria.clone();
        spec.suggested_actions = draft.suggested_actions.clone();
        spec.acceptance_criteria = spec.criteria.iter().map(|c| c.outcome.clone()).collect();
    }
    spec.checker_id = handle.goal_verifier_ids().into_iter().next();
    let mut limits = spec.limits.clone();
    if let Some(turns) = body.max_turns {
        limits.max_turns = turns;
    }
    if let Some(minutes) = body.max_minutes {
        limits.deadline_unix_secs = now_unix().saturating_add(minutes.saturating_mul(60));
    }
    spec.limits = limits;
    if let Err(error) = spec.validate() {
        return ApiResponse::err(400, format!("{error:#}"));
    }
    // Arm the driver before creating: a goal created while scheduling is
    // disabled would sit idle until the next explicit start. Arming never
    // starts a run by itself — the leased activation below is the single
    // owner of the run — so a refused create cannot leave an unleased run
    // behind. The slot is taken first — goal turns never pass through
    // `submit_input`, so without it a goal would run entirely outside
    // `max_running`. Commands are serialised per session so a failure can
    // release exactly the slot this attempt added, even under concurrent
    // creations.
    let _command = match state
        .registry
        .lock_goal_commands(&id, query.project.as_deref())
        .await
    {
        Ok(guard) => guard,
        Err(error) => return registry_error(error),
    };
    // An existing unfinished goal is refused before any capacity is taken and
    // before the driver is armed, so a refused create never starts a goal the
    // request was not about.
    if handle
        .goal_snapshot()
        .is_some_and(|goal| !goal.status.is_terminal())
    {
        return ApiResponse::err(409, "session already has an unfinished goal");
    }
    let lease = match state
        .registry
        .reserve_goal_run(&id, query.project.as_deref())
        .await
    {
        Ok(lease) => lease,
        Err(error) => return registry_error(error),
    };
    let duration_secs = body.max_minutes.unwrap_or(60).saturating_mul(60);
    let proposed = if body.generate == Some(false) {
        handle.draft_goal(spec, duration_secs)
    } else {
        handle.request_goal_draft(spec, duration_secs)
    };
    let draft = match proposed {
        Ok(draft) => draft,
        Err(error) => return ApiResponse::err(409, error.to_string()),
    };
    if body.generate == Some(false) {
        return ApiResponse::ok(json!({"goal":draft}));
    }
    drop(_command);
    let _op = runtime.begin_operation();
    let task = tokio::spawn(async move {
        let _lease = lease;
        handle
            .generate_goal_draft_at(&draft.id, draft.revision, "")
            .await
    });
    let timeout_secs = crate::session::runtime::draft_generation_timeout_secs();
    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), task).await {
        Ok(Ok(Ok(draft))) => ApiResponse::ok(json!({"goal":draft})),
        Ok(Ok(Err(error))) => ApiResponse::err(
            409,
            format!("draft generation failed; proposal preserved: {error}"),
        ),
        Ok(Err(error)) => ApiResponse::err(
            409,
            format!("draft generation task failed; proposal preserved: {error}"),
        ),
        Err(_) => ApiResponse::err(409, "draft generation timed out; proposal preserved"),
    }
}

#[derive(serde::Deserialize)]
pub(super) struct GoalActionBody {
    #[serde(default)]
    feedback: Option<String>,
    /// Exact goal the caller believes it is acting on.
    #[serde(default)]
    goal_id: Option<String>,
    #[serde(default)]
    revision: Option<u64>,
    /// Human evidence for `approve` and `reconcile`.
    #[serde(default)]
    evidence: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    max_minutes: Option<u64>,
    /// Three-state budget patch: absent keeps the current limit, a number sets
    /// it, an explicit `null` clears it. `Option<u64>` alone cannot express the
    /// difference — serde maps both a missing field and `null` to `None`.
    #[serde(default, deserialize_with = "deserialize_nullable_u64")]
    soft_token_limit: Option<Option<u64>>,
}

/// Deserialize an absent/`null`/number field into the three-state patch above.
pub(super) fn deserialize_nullable_u64<'de, D>(
    deserializer: D,
) -> Result<Option<Option<u64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    pub(super) struct NullableU64;

    impl<'de> serde::de::Visitor<'de> for NullableU64 {
        type Value = Option<Option<u64>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("null or a non-negative integer")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_some<D2>(self, value: D2) -> Result<Self::Value, D2::Error>
        where
            D2: serde::Deserializer<'de>,
        {
            serde::Deserialize::deserialize(value).map(|limit| Some(Some(limit)))
        }
    }

    // The outer `Option` is the field's own absence, handled by `default`; this
    // visitor is only reached when the field is present.
    deserializer.deserialize_option(NullableU64)
}

pub(super) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pause, resume, cancel, clear, re-budget, approve or reconcile a goal.
///
/// Every mutation is a compare-and-set on the caller's revision, so a stale
/// workbench cannot undo a change another surface already made.
pub(super) async fn goal_action(
    State(state): State<Arc<ApiState>>,
    AxumPath((id, action)): AxumPath<(String, String)>,
    Query(query): Query<ProjectQuery>,
    body: Option<axum::Json<GoalActionBody>>,
) -> ApiResponse {
    let runtime = match state.registry.active_runtime(&id, query.project.as_deref()) {
        Ok(Some(runtime)) => runtime,
        Ok(None) => return ApiResponse::err(409, "session is not open"),
        Err(error) => return registry_error(error),
    };
    let handle = match runtime.goal_handle() {
        Some(handle) => handle,
        None => return ApiResponse::err(409, "session runtime is closing"),
    };
    let body = match body {
        Some(axum::Json(body)) => body,
        None => GoalActionBody {
            feedback: None,
            goal_id: None,
            revision: None,
            evidence: None,
            max_turns: None,
            max_minutes: None,
            soft_token_limit: None,
        },
    };
    let current = match handle.goal_snapshot() {
        Some(goal) => goal,
        None => return ApiResponse::err(404, "session has no goal"),
    };
    let Some(revision) = body.revision else {
        return ApiResponse::err(400, "revision is required for goal control");
    };
    // The client's expected identity is mandatory. Revisions restart at 1 for
    // every goal, so a stale request for a replaced goal could otherwise match
    // the replacement's revision and cancel it.
    match body.goal_id.as_deref() {
        None => return ApiResponse::err(400, "goal_id is required for goal control"),
        Some(expected) if expected != current.id => {
            return ApiResponse::err(409, "goal was replaced; reread it before acting");
        }
        Some(_) => {}
    }
    let result = match action.as_str() {
        "refine" => {
            let _command = match state
                .registry
                .lock_goal_commands(&id, query.project.as_deref())
                .await
            {
                Ok(guard) => guard,
                Err(error) => return registry_error(error),
            };
            let current = match handle.goal_snapshot() {
                Some(goal) => goal,
                None => return ApiResponse::err(404, "session has no goal"),
            };
            if current.status != mink::runtime::GoalStatus::Draft || current.revision != revision {
                return ApiResponse::err(409, "stale draft; reread before refining");
            }
            let lease = match state
                .registry
                .reserve_goal_run(&id, query.project.as_deref())
                .await
            {
                Ok(lease) => lease,
                Err(error) => return registry_error(error),
            };
            drop(_command);
            let _op = runtime.begin_operation();
            let feedback = body.feedback.unwrap_or_default();
            let task = tokio::spawn(async move {
                let _lease = lease;
                handle
                    .generate_goal_draft_at(&current.id, revision, &feedback)
                    .await
            });
            let timeout_secs = crate::session::runtime::draft_generation_timeout_secs();
            return match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), task)
                .await
            {
                Ok(Ok(Ok(goal))) => ApiResponse::ok(json!({"goal":goal})),
                Ok(Ok(Err(error))) => {
                    ApiResponse::err(409, format!("draft refinement failed: {error}"))
                }
                Ok(Err(error)) => {
                    ApiResponse::err(409, format!("draft refinement task failed: {error}"))
                }
                Err(_) => ApiResponse::err(409, "draft refinement timed out"),
            };
        }
        "pause" => {
            state
                .registry
                .mark_goal_draining(&id, query.project.as_deref());
            handle.pause_goal(&current.id, revision)
        }
        "confirm" | "resume" => {
            // A reopened session starts with scheduling disabled, so an
            // explicit resume has to arm the driver or the goal sits active
            // and never runs. Arming alone starts no run: the leased
            // activation below is the single owner of the run, so a refused
            // resume cannot issue provider requests or leak capacity. Capacity
            // is checked before it can start, and a refused resume releases
            // only a slot this attempt added.
            let _command = match state
                .registry
                .lock_goal_commands(&id, query.project.as_deref())
                .await
            {
                Ok(guard) => guard,
                Err(error) => return registry_error(error),
            };
            let lease = match state
                .registry
                .reserve_goal_run(&id, query.project.as_deref())
                .await
            {
                Ok(lease) => lease,
                Err(error) => return registry_error(error),
            };
            if let Err(error) = handle.start_goal_driver() {
                // The lease is dropped here: the driver could not be armed.
                return ApiResponse::err(409, format!("{error}"));
            }
            if action == "confirm" {
                handle.confirm_and_start_goal(&current.id, revision, Box::new(lease))
            } else {
                handle.resume_and_start_goal(&current.id, revision, Box::new(lease))
            }
        }
        "cancel" => {
            state
                .registry
                .mark_goal_draining(&id, query.project.as_deref());
            handle.cancel_goal(&current.id, revision)
        }
        "clear" => handle.clear_goal(&current.id, revision),
        "approve" => handle.approve_goal(
            &current.id,
            revision,
            vec![body.evidence.unwrap_or_default()],
        ),
        "reconcile" => {
            handle.reconcile_goal_effects(&current.id, revision, body.evidence.unwrap_or_default())
        }
        "budget" => {
            let mut limits = current.spec.limits.clone();
            if let Some(turns) = body.max_turns {
                limits.max_turns = turns;
            }
            if let Some(minutes) = body.max_minutes {
                limits.deadline_unix_secs = now_unix().saturating_add(minutes.saturating_mul(60));
            }
            if let Some(limit) = body.soft_token_limit {
                limits.soft_token_limit = limit;
            }
            handle.update_goal_limits(&current.id, revision, limits)
        }
        other => return ApiResponse::err(404, format!("unknown goal action {other}")),
    };
    match result {
        Ok(goal) => ApiResponse::ok(json!({
            "goal": crate::session::runtime::goal_summary_value(&json!(goal)),
        })),
        Err(error) => ApiResponse::err(409, format!("{error}")),
    }
}
