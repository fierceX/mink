use super::*;

#[tokio::test]
pub(super) async fn goal_endpoint_reports_the_persisted_snapshot_or_null() {
    let (app, dir) = test_router_with_session_dir();
    // No goal.json yet: the workbench marker must read as "no goal".
    let (status, body) = req(&app, Method::GET, "/api/sessions/test-session/goal").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"]["goal"].is_null(),
        "sessions without a goal report null, got {body}"
    );

    // goal.json wraps the snapshot in {version, current, history}; only
    // `current` is mirrored.
    std::fs::write(
        dir.join("goal.json"),
        serde_json::json!({
            "version": 1,
            "current": {
                "id": "goal-1", "status": "active", "phase": "executing",
                "remaining_turns": 12, "hidden": false
            },
            "history": []
        })
        .to_string(),
    )
    .unwrap();
    let (status, body) = req(&app, Method::GET, "/api/sessions/test-session/goal").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["goal"]["id"], "goal-1", "{body}");
    assert_eq!(body["data"]["goal"]["status"], "active", "{body}");
    assert_eq!(body["data"]["goal"]["remaining_turns"], 12, "{body}");
    assert!(
        body["data"]["goal"].get("history").is_none(),
        "history is not mirrored: {body}"
    );

    // A cleared (hidden) goal is absent on the persisted path too: the
    // workbench must read the same "no goal" as a live runtime reports.
    std::fs::write(
        dir.join("goal.json"),
        serde_json::json!({
            "version": 1,
            "current": {
                "id": "goal-1", "status": "cancelled", "phase": "idle",
                "hidden": true
            },
            "history": []
        })
        .to_string(),
    )
    .unwrap();
    let (status, body) = req(&app, Method::GET, "/api/sessions/test-session/goal").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"]["goal"].is_null(),
        "a cleared goal must not leak through the disk fallback: {body}"
    );
}

#[tokio::test]
pub(super) async fn goal_endpoint_normalizes_cleared_goals_to_null() {
    let (app, home, sid) = open_goal_router().await;
    let g = |suffix: &str| format!("/api/sessions/{sid}{suffix}");

    let (status, body) = post_json(
        &app,
        &g("/goal"),
        serde_json::json!({
            "objective": "clear me",
            "protocol_version": 2,
            "generate": false
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["data"]["goal"]["id"].as_str().unwrap().to_string();
    let revision = body["data"]["goal"]["revision"].as_u64().unwrap();

    let (status, body) = post_json(
        &app,
        &g("/goal/cancel"),
        serde_json::json!({"goal_id": id, "revision": revision}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["goal"]["status"], "cancelled", "{body}");
    let revision = body["data"]["goal"]["revision"].as_u64().unwrap();

    let (status, body) = post_json(
        &app,
        &g("/goal/clear"),
        serde_json::json!({"goal_id": id, "revision": revision}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["goal"]["hidden"], true, "{body}");

    // The live runtime still drives the session, but the cleared goal is
    // gone from every read surface.
    let (status, body) = req(&app, Method::GET, &g("/goal")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"]["goal"].is_null(),
        "a cleared goal must not leak through the live read: {body}"
    );
    assert_eq!(body["data"]["driven"], true, "{body}");

    // The authenticated session snapshot agrees with the REST read.
    let (status, snapshot) = req(&app, Method::GET, &g("")).await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    assert!(
        snapshot["data"]["goal"].is_null(),
        "the snapshot must not carry a cleared goal: {snapshot}"
    );
    let _ = std::fs::remove_dir_all(home);
}

/// Budget patches distinguish an omitted `soft_token_limit` (keep the
/// current value) from an explicit `null` (clear it). Both must survive the
/// HTTP layer unchanged, while invalid values and stale revisions leave the
/// authoritative snapshot alone.
#[tokio::test]
pub(super) async fn goal_budget_patches_soft_token_limit_as_missing_set_or_null() {
    let (app, home, sid) = open_goal_router().await;
    let g = |suffix: &str| format!("/api/sessions/{sid}{suffix}");
    let (status, body) = post_json(
        &app,
        &g("/goal"),
        serde_json::json!({
            "objective": "budget tri-state",
            "protocol_version": 2,
            "generate": false,
            "max_turns": 3,
            "max_minutes": 30
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["data"]["goal"]["id"].as_str().unwrap().to_string();
    let mut revision = body["data"]["goal"]["revision"].as_u64().unwrap();

    // A number sets the limit.
    let (status, body) = post_json(
        &app,
        &g("/goal/budget"),
        serde_json::json!({"goal_id": id, "revision": revision, "soft_token_limit": 500}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["goal"]["spec"]["limits"]["soft_token_limit"], 500,
        "{body}"
    );
    revision = body["data"]["goal"]["revision"].as_u64().unwrap();

    // Omitting the field leaves it in place.
    let (status, body) = post_json(
        &app,
        &g("/goal/budget"),
        serde_json::json!({"goal_id": id, "revision": revision, "max_turns": 4}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["goal"]["spec"]["limits"]["soft_token_limit"], 500,
        "{body}"
    );
    revision = body["data"]["goal"]["revision"].as_u64().unwrap();

    // An explicit null clears it.
    let (status, body) = post_json(
        &app,
        &g("/goal/budget"),
        serde_json::json!({"goal_id": id, "revision": revision, "soft_token_limit": null}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["data"]["goal"]["spec"]["limits"]["soft_token_limit"].is_null(),
        "null must clear the limit: {body}"
    );
    revision = body["data"]["goal"]["revision"].as_u64().unwrap();

    // Zero follows the core rule: it is not a valid soft limit.
    let (status, body) = post_json(
        &app,
        &g("/goal/budget"),
        serde_json::json!({"goal_id": id, "revision": revision, "soft_token_limit": 0}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Negative, overflowing and wrongly typed values are refused by the
    // body decoder before any state change.
    for raw in [
        format!("{{\"goal_id\":\"{id}\",\"revision\":{revision},\"soft_token_limit\":-1}}"),
        format!(
            "{{\"goal_id\":\"{id}\",\"revision\":{revision},\"soft_token_limit\":18446744073709551616}}"
        ),
        format!("{{\"goal_id\":\"{id}\",\"revision\":{revision},\"soft_token_limit\":\"500\"}}"),
    ] {
        let (status, text) = post_raw(&app, &g("/goal/budget"), &raw).await;
        assert!(
            status.is_client_error(),
            "invalid soft_token_limit must be refused, got {status}: {text}"
        );
    }

    // A stale revision or a replaced goal is refused without a snapshot
    // change.
    let (status, body) = post_json(
        &app,
        &g("/goal/budget"),
        serde_json::json!({"goal_id": id, "revision": revision - 1, "soft_token_limit": 700}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = post_json(
        &app,
        &g("/goal/budget"),
        serde_json::json!({"goal_id": "other-goal", "revision": revision, "soft_token_limit": 700}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = req(&app, Method::GET, &g("/goal")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["goal"]["revision"], revision, "{body}");
    assert!(
        body["data"]["goal"]["spec"]["limits"]["soft_token_limit"].is_null(),
        "refused patches must not alter the snapshot: {body}"
    );

    // A cancelled goal cannot be re-budgeted.
    let (status, body) = post_json(
        &app,
        &g("/goal/cancel"),
        serde_json::json!({"goal_id": id, "revision": revision}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let revision = body["data"]["goal"]["revision"].as_u64().unwrap();
    let (status, body) = post_json(
        &app,
        &g("/goal/budget"),
        serde_json::json!({"goal_id": id, "revision": revision, "soft_token_limit": 900}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn goal_control_requires_an_open_session() {
    // `test_router` never opens the session, so control must refuse rather
    // than write a goal.json that no runtime would ever drive.
    let (app, _dir) = test_router_with_session_dir();
    let (status, body) = post_json(
        &app,
        "/api/sessions/test-session/goal",
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "ship it" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
pub(super) async fn goal_control_rejects_a_blank_objective_with_a_host_error() {
    let (app, home, sid) = open_goal_router().await;
    let g = |suffix: &str| format!("/api/sessions/{sid}{suffix}");
    let (status, body) = post_json(
        &app,
        &g("/goal"),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "   " }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("objective"),
        "{body}"
    );
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn server_creates_and_controls_a_goal_through_the_runtime() {
    let (app, home, sid) = open_goal_router().await;
    let g = |suffix: &str| format!("/api/sessions/{sid}{suffix}");

    // Creating requires the session to be open; the fixture opened it.
    let (status, body) = post_json(
        &app,
        &g("/goal"),
        serde_json::json!({
            "objective": "ship the parser",
            "protocol_version": 2,
            "generate": false,
            "acceptance": "agent_reviewed",
            "max_turns": 4,
            "acceptance_criteria": ["parses fixtures"],
            "constraints": ["no network"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let goal = &body["data"]["goal"];
    assert_eq!(goal["status"], "draft", "{body}");
    assert_eq!(
        goal["spec"]["limits"]["max_turns"], 4,
        "host-chosen limit is honoured"
    );
    let id = goal["id"].as_str().expect("goal id").to_string();
    let revision = goal["revision"].as_u64().expect("goal revision");
    let (status, confirmed) = post_json(
        &app,
        &g("/goal/confirm"),
        serde_json::json!({"goal_id":id,"revision":revision}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
    let revision = confirmed["data"]["goal"]["revision"].as_u64().unwrap();

    // The live runtime reports the same goal, not a stale disk read.
    let (status, body) = req(&app, Method::GET, &g("/goal")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["driven"], true, "{body}");
    assert_eq!(body["data"]["goal"]["id"], id, "{body}");

    // Every mutation is a compare-and-set on the goal the caller means.
    let (status, body) = post_json(
        &app,
        &g("/goal/pause"),
        serde_json::json!({ "revision": revision + 99, "goal_id": id }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "stale revision must be refused: {body}"
    );

    // A missing goal id is a host error, not a silent default to the
    // server's current goal: replacements restart revisions at 1, so
    // substituting the current id would let a stale request act on it.
    let (status, body) = post_json(
        &app,
        &g("/goal/pause"),
        serde_json::json!({ "revision": revision }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // And a goal id that is not the current one is refused outright.
    let (status, body) = post_json(
        &app,
        &g("/goal/pause"),
        serde_json::json!({ "revision": revision, "goal_id": "goal-someone-else" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // The current revision pauses, and the state is observable afterwards.
    let (status, body) = post_json(
        &app,
        &g("/goal/pause"),
        serde_json::json!({ "revision": revision, "goal_id": id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["goal"]["status"], "paused", "{body}");
    let paused_revision = body["data"]["goal"]["revision"].as_u64().unwrap();

    let (status, body) = post_json(
        &app,
        &g("/goal/resume"),
        serde_json::json!({ "revision": paused_revision, "goal_id": id }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["goal"]["status"], "active", "{body}");

    // Missing revision is a host error, not a silent no-op.
    let (status, body) = post_json(&app, &g("/goal/cancel"), serde_json::json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn goal_rejects_an_unknown_acceptance_mode() {
    let (app, home, sid) = open_goal_router().await;
    let g = |suffix: &str| format!("/api/sessions/{sid}{suffix}");
    let (status, body) = post_json(
        &app,
        &g("/goal"),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "x", "acceptance": "wishful" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn legacy_goal_write_requires_explicit_protocol_upgrade() {
    let (app, home, sid) = open_goal_router().await;
    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{sid}/goal"),
        serde_json::json!({"objective":"legacy request"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("protocol_version 2")
    );
    let _ = std::fs::remove_dir_all(home);
}

/// Audit regression: a goal creation refused while an ordinary turn owns
/// the session reserved a capacity slot and never released it, so the
/// slot leaked even though no goal ever existed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn refused_goal_creation_does_not_leak_a_capacity_slot() {
    let release = Arc::new(tokio::sync::watch::Sender::new(false));
    let (app, home, first, second) = gated_router(1, release.clone()).await;

    // The ordinary turn is the only running turn and holds the slot.
    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{first}/turn"),
        serde_json::json!({ "input": "hold the capacity slot" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_for_running(&app, &first, true).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{first}/goal"),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "busy goal" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Let the turn end, then a second session must be able to claim the
    // released capacity: the refused attempt created no goal, so nothing
    // may stay reserved for it.
    release.send_replace(true);
    wait_for_running(&app, &first, false).await;
    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{second}/goal"),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "second goal", "max_turns": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Stop the driven goal before dropping the fixture home.
    let goal = &body["data"]["goal"];
    let _ = post_json(
        &app,
        &format!("/api/sessions/{second}/goal/cancel"),
        serde_json::json!({ "goal_id": goal["id"], "revision": goal["revision"] }),
    )
    .await;
    let _ = std::fs::remove_dir_all(home);
}

/// A repeat create must not free the slot its unfinished goal still holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn repeat_goal_create_keeps_the_running_goal_slot() {
    let release = Arc::new(tokio::sync::watch::Sender::new(false));
    let (app, home, first, second) = gated_router(1, release.clone()).await;

    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{first}/goal"),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "first goal" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let draft = &body["data"]["goal"];
    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{first}/goal/confirm"),
        serde_json::json!({"goal_id":draft["id"],"revision":draft["revision"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The repeat is refused but its rollback must observe the active goal
    // and leave the slot alone.
    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{first}/goal"),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "replacement must be refused" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{second}/goal"),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "second goal" }),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    release.send_replace(true);
    let _ = std::fs::remove_dir_all(home);
}

/// Audit8 F1: a goal left `active` by a crash must never be driven by the
/// mere act of arming the driver. Reopening issues no provider request;
/// the leased resume is the activation that starts exactly one run, keeps
/// the single capacity slot, and refuses competing work while it runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn restored_active_goal_starts_only_through_a_leased_resume() {
    let home = std::env::temp_dir().join(format!(
        "mink-server-restore-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = home.join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let release = Arc::new(tokio::sync::watch::Sender::new(false));
    let gauge = Arc::new(CapacityGauge::default());
    let registry = Arc::new(Registry::with_llm_backend(
        home.clone(),
        "flash".to_string(),
        1,
        Arc::new(GatedBackend::with_gauge(&release, gauge.clone())),
    ));
    let first = registry.create("restore-a", &cwd).await.unwrap();
    let second = registry.create("restore-b", &cwd).await.unwrap();
    registry
        .open(&first.id, Some(&first.project_key))
        .await
        .unwrap();
    registry
        .open(&second.id, Some(&second.project_key))
        .await
        .unwrap();

    // Create the goal while scheduling is disabled: the contract is on
    // disk as `active` with no run behind it, which is exactly the state a
    // hard-killed server reopens into (recovery leaves it active and
    // idle; nothing arms the driver until an explicit activation).
    let created = {
        let runtime = registry
            .active_runtime(&first.id, Some(&first.project_key))
            .unwrap()
            .unwrap();
        let goals = runtime.goals().unwrap();
        let draft = goals
            .draft(mink_goal::GoalSpec::new("restore then resume"), 3600)
            .unwrap();
        let goal = goals.confirm(&draft.id, draft.revision).unwrap();
        assert_eq!(goal.status, mink_goal::GoalStatus::Active);
        goal
    };
    assert_eq!(
        gauge.calls(),
        0,
        "an armed-only runtime must issue no provider request"
    );
    assert_eq!(registry.capacity.occupied(), 0);

    let (shutdown, _) = tokio::sync::watch::channel(false);
    let app = router(Arc::new(ApiState {
        registry: registry.clone(),
        cwd: cwd.clone(),
        shutdown,
    }));
    let base = format!("/api/sessions/{}", first.id);
    let (status, body) = req(&app, Method::GET, &format!("{base}/goal")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let restored = body["data"]["goal"].clone();
    assert_eq!(restored["status"], "active", "{body}");
    assert_eq!(restored["phase"], "idle", "{body}");
    let goal_id = restored["id"].as_str().unwrap().to_string();
    let revision = restored["revision"].as_u64().unwrap();

    // The leased resume is the activation that owns the run.
    let (status, body) = post_json(
        &app,
        &format!("{base}/goal/resume"),
        serde_json::json!({ "goal_id": goal_id, "revision": revision }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let resumed_revision = body["data"]["goal"]["revision"].as_u64().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while gauge.active() != 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "the resumed run never issued a provider request"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        registry.capacity.occupied(),
        1,
        "the admitted run holds the single slot"
    );
    assert!(
        registry
            .running(&first.id, Some(&first.project_key))
            .unwrap()
    );

    // Competing work under `max_running = 1` is refused, not admitted.
    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{}/goal", second.id),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "competing goal" }),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{}/turn", second.id),
        serde_json::json!({ "input": "competing turn" }),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

    // A repeat resume while the run owns the slot adds no second run.
    let (status, body) = post_json(
        &app,
        &format!("{base}/goal/resume"),
        serde_json::json!({ "goal_id": goal_id, "revision": resumed_revision }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        gauge.calls(),
        1,
        "a refused resume issued a provider request"
    );
    assert_eq!(gauge.active(), 1);
    assert_eq!(
        created.id, goal_id,
        "the resumed goal is the restored contract"
    );

    // Cancel is a stop intent: the in-flight turn still owns the slot
    // until it really exits, and only then is capacity released.
    let (_, body) = req(&app, Method::GET, &format!("{base}/goal")).await;
    let live_revision = body["data"]["goal"]["revision"].as_u64().unwrap();
    let (status, body) = post_json(
        &app,
        &format!("{base}/goal/cancel"),
        serde_json::json!({ "goal_id": goal_id, "revision": live_revision }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        registry.capacity.occupied(),
        1,
        "capacity stays occupied while the cancelled turn exits"
    );
    release.send_replace(true);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while registry.capacity.occupied() != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the cancelled run never released its slot"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        gauge.max_active() <= 1,
        "max_running was exceeded: {}",
        gauge.max_active()
    );
    registry.shutdown_all().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
}

/// A configuration refusal (no GoalReport in the resolved surface) also
/// rolls back the reservation it made before the contract check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn configuration_refused_goal_creation_rolls_back_its_slot() {
    let home = std::env::temp_dir().join(format!(
        "mink-server-goal-config-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = home.join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    // The project layer removes GoalReport, so core refuses the contract.
    std::fs::write(cwd.join(".minkrc"), "[tools]\nenabled_tools = [\"Read\"]\n").unwrap();
    let release = Arc::new(tokio::sync::watch::Sender::new(false));
    let registry = Arc::new(Registry::with_llm_backend(
        home.clone(),
        "flash".to_string(),
        1,
        Arc::new(GatedBackend::new(&release)),
    ));
    let session = registry.create("config", &cwd).await.unwrap();
    registry
        .open(&session.id, Some(&session.project_key))
        .await
        .unwrap();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let app = router(Arc::new(ApiState {
        registry: registry.clone(),
        cwd,
        shutdown,
    }));

    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{}/goal", session.id),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "must be refused" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("GoalRead"),
        "the refusal names the missing capability: {body}"
    );

    // The refused attempt created no goal, so the sole capacity slot must
    // be free: a second session without the restrictive layer can claim it.
    let cwd2 = home.join("workspace2");
    std::fs::create_dir_all(&cwd2).unwrap();
    let other = registry.create("other", &cwd2).await.unwrap();
    registry
        .open(&other.id, Some(&other.project_key))
        .await
        .unwrap();
    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{}/goal", other.id),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "capacity was released", "max_turns": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    release.send_replace(true);
    let _ = std::fs::remove_dir_all(home);
}

/// A persistence failure during goal creation must release the slot too:
/// the goal never existed, so nothing may keep occupying `max_running`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn failed_goal_persistence_rolls_back_its_slot() {
    use std::os::unix::fs::PermissionsExt;

    let release = Arc::new(tokio::sync::watch::Sender::new(false));
    let (app, home, first, second) = gated_router(1, release.clone()).await;
    let projects = home.join(".mink").join("projects");
    let session_dir = std::fs::read_dir(&projects)
        .unwrap()
        .flat_map(|entry| std::fs::read_dir(entry.unwrap().path()).unwrap())
        .map(|entry| entry.unwrap().path())
        .find(|path| path.file_name().is_some_and(|name| name == first.as_str()))
        .expect("session directory");
    let original = std::fs::metadata(&session_dir).unwrap().permissions();
    std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{first}/goal"),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "persistence must fail", "max_turns": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Restore write permission before the fixture cleanup, then prove the
    // slot is free by letting the second session claim it.
    std::fs::set_permissions(&session_dir, original).unwrap();
    let (status, body) = post_json(
        &app,
        &format!("/api/sessions/{second}/goal"),
        serde_json::json!({ "protocol_version":2, "generate":false, "objective": "slot was released", "max_turns": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    release.send_replace(true);
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn resume_guidance_obeys_capacity_and_live_guidance_remains_idempotent() {
    // This test builds its registry from ambient configuration, so it must
    // hold the shared environment lock: otherwise a concurrent config test
    // can leave `MODEL` set to a value that changes the resolved surface
    // and the capacity expectations below no longer describe what runs.
    let _env = crate::session::TEST_ENV_LOCK.lock().await;
    pub(super) struct WaitingBackend {
        pub(super) started: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait::async_trait]
    impl mink::runtime::LlmBackend for WaitingBackend {
        fn name(&self) -> &str {
            "capacity-test"
        }
        async fn stream(
            &self,
            _request: mink::runtime::LlmRequest,
        ) -> anyhow::Result<mink::runtime::LlmResponseStream> {
            self.started
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(mink::runtime::LlmResponseStream {
                events: Box::pin(futures::stream::pending()),
                attempt_count: 1,
            })
        }
    }
    async fn post(
        app: &Router,
        uri: &str,
        data: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(data.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    let home = std::env::temp_dir().join(format!(
        "mink-capacity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let registry = Arc::new(Registry::with_llm_backend(
        home.clone(),
        "mock".into(),
        1,
        Arc::new(WaitingBackend {
            started: backend_started.clone(),
        }),
    ));
    let a = registry.create("a", &cwd).await.unwrap();
    let b = registry.create("b", &cwd).await.unwrap();
    registry.open(&a.id, Some(&a.project_key)).await.unwrap();
    registry.open(&b.id, Some(&b.project_key)).await.unwrap();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let app = router(Arc::new(ApiState {
        registry: registry.clone(),
        cwd,
        shutdown,
    }));
    let a_base = format!("/api/sessions/{}", a.id);
    let a_inputs = format!("{a_base}/inputs?project={}", a.project_key);
    let b_inputs = format!("/api/sessions/{}/inputs?project={}", b.id, b.project_key);
    let task = json!({"request_id":"first", "text":"wait", "target_turn_id":null});
    let (status, first) = post(&app, &a_inputs, task.clone()).await;
    assert_eq!(status, StatusCode::OK);
    // A retry of an already admitted request must not consume another slot.
    let (status, duplicate) = post(&app, &a_inputs, task).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["data"]["input_id"], duplicate["data"]["input_id"]);
    // Wait until the running turn has reached the provider stream. The
    // inbox applies pending guidance at the top of every round, so guidance
    // admitted before the first request started is legitimately applied by
    // that round; "live guidance" means the turn is already waiting on the
    // model when the input arrives.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while !backend_started.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let runtime = registry
        .active_runtime(&a.id, Some(&a.project_key))
        .unwrap()
        .unwrap();
    let guidance = json!({"request_id":"guide", "text":"extra constraint", "target_turn_id":first["data"]["turn_id"]});
    let (status, guide) = post(&app, &a_inputs, guidance.clone()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "live guidance is allowed at capacity"
    );
    assert_eq!(post(&app, &a_inputs, guidance).await.0, StatusCode::OK);
    registry.interrupt(&a.id, Some(&a.project_key)).unwrap();
    let guide_id = guide["data"]["input_id"].as_str().unwrap();
    let receipt = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if !runtime.running()
                && let Some(receipt) = runtime.inbox().entries().into_iter().find(|r| {
                    r.input_id == guide_id && r.status == mink::runtime::InputStatus::Unapplied
                })
            {
                break receipt;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // Reopen to exercise durable input restoration and finish old cleanup.
    registry.close(&a.id, Some(&a.project_key)).await.unwrap();
    registry.open(&a.id, Some(&a.project_key)).await.unwrap();
    assert_eq!(
        post(
            &app,
            &b_inputs,
            json!({"request_id":"second", "text":"wait"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        post(
            &app,
            &a_inputs,
            json!({"request_id":"third", "text":"new task"})
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    let resume_uri = format!(
        "{a_base}/inputs/{guide_id}/resume?project={}",
        a.project_key
    );
    assert_eq!(
        post(&app, &resume_uri, json!({"revision":receipt.revision}))
            .await
            .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert!(
        !registry
            .active_runtime(&a.id, Some(&a.project_key))
            .unwrap()
            .unwrap()
            .running()
    );
    // A normal input resume must also be capacity checked.
    let root_resume = format!(
        "{a_base}/inputs/{}/resume?project={}",
        first["data"]["input_id"].as_str().unwrap(),
        a.project_key
    );
    assert_eq!(
        post(
            &app,
            &root_resume,
            json!({"revision":first["data"]["revision"]})
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS
    );
    registry.close(&b.id, Some(&b.project_key)).await.unwrap();
    let (status, resumed) = post(&app, &resume_uri, json!({"revision":receipt.revision})).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "resume succeeds after capacity is released: {resumed}"
    );
    assert_eq!(resumed["data"]["guidance"], false);
    assert_ne!(resumed["data"]["turn_id"], guide["data"]["turn_id"]);
    registry.shutdown_all().await.unwrap();
    std::fs::remove_dir_all(home).unwrap();
}
