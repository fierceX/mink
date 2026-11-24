use super::*;

#[tokio::test]
async fn goal_pause_during_request_stops_new_requests_and_preserves_deadline() {
    use crate::runtime::{GoalSpec, GoalStatus};
    let home = unique_temp_dir("goal-pause-home");
    let cwd = unique_temp_dir("goal-pause-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(PendingEstablishBackend::default());
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let created = handle
        .confirmed_test_goal(GoalSpec::new("do work"))
        .unwrap();
    backend.entered.notified().await;
    let g = handle.goal_snapshot().unwrap();
    handle.pause_goal(&g.id, g.revision).unwrap();
    let paused = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(paused.status, GoalStatus::Paused);
    assert_eq!(
        paused.spec.limits.deadline_unix_secs,
        created.spec.limits.deadline_unix_secs
    );
    assert_eq!(*backend.calls.lock().unwrap(), 1);
    assert!(handle.pause_goal(&g.id, g.revision).is_err());
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn model_reported_block_needs_consecutive_rounds_before_ending_the_goal() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-blocked-home");
    let cwd = unique_temp_dir("goal-blocked-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("proof.txt"),
        "human prerequisite remains unavailable",
    )
    .unwrap();
    let backend = Arc::new(AlwaysBlockedBackend {
        calls: Default::default(),
    });
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let created = handle
        .confirmed_test_goal(GoalSpec::new("stuck task"))
        .unwrap();
    let mut limits = created.spec.limits.clone();
    limits.max_turns = 10;
    handle
        .update_goal_limits(&created.id, created.revision, limits)
        .unwrap();

    let goal = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .expect("driver must terminate")
        .unwrap();

    // A model cannot end the goal by declaring a blocker on its first round:
    // the same condition has to persist across several rounds, otherwise one
    // unlucky turn would discard the remaining budget.
    assert_eq!(goal.status, GoalStatus::Blocked, "{goal:?}");
    assert!(
        goal.blocked_rounds >= 3,
        "blocked after only {} rounds",
        goal.blocked_rounds
    );
    assert!(
        goal.blocker_rounds >= 3,
        "the same blocker must reach the gate: {}",
        goal.blocker_rounds
    );
    assert!(
        goal.iterations.len() >= 3,
        "streak must span distinct rounds, got {}",
        goal.iterations.len()
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// Audit9 F2: different blockers must not be accumulated into one streak. The
/// gate counts consecutive reports of the *same* condition, while
/// `blocked_rounds` keeps counting the consecutive blocked rounds.
#[tokio::test]
async fn audit9_different_blockers_do_not_share_a_streak() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-blocked-changing-home");
    let cwd = unique_temp_dir("goal-blocked-changing-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("proof.txt"),
        "independent prerequisites remain unavailable",
    )
    .unwrap();
    let backend = Arc::new(ScriptedReportBackend {
        calls: Default::default(),
        script: (0..10)
            .map(|n| {
                (
                    "blocked",
                    format!("missing independent prerequisite {n}"),
                    format!("supply prerequisite {n}"),
                )
            })
            .collect(),
    });
    let runtime = AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend))
        .await
        .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let created = handle
        .confirmed_test_goal(GoalSpec::new("stuck task"))
        .unwrap();
    let mut limits = created.spec.limits.clone();
    limits.max_turns = 10;
    handle
        .update_goal_limits(&created.id, created.revision, limits)
        .unwrap();

    let goal = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .expect("driver must terminate")
        .unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&cwd);

    assert_eq!(
        goal.status,
        GoalStatus::BudgetExhausted,
        "different blockers must not end the goal: {goal:?}"
    );
    assert!(
        goal.blocked_rounds >= 3,
        "consecutive blocked reports: {}",
        goal.blocked_rounds
    );
    assert_eq!(
        goal.blocker_rounds, 1,
        "a changed blocker identity must restart the gate streak"
    );
    assert!(
        goal.iterations.len() >= 3,
        "streak must span distinct rounds, got {}",
        goal.iterations.len()
    );
    assert_eq!(
        goal.budget().blocker_rounds,
        1,
        "the budget view shows the same-blocker streak"
    );
    assert_eq!(
        goal.budget().blocked_rounds,
        goal.blocked_rounds,
        "the budget's blocked_rounds mirrors the snapshot's consecutive count"
    );
}

/// Audit9 F2: `Continue` clears the blocker identity and its streak, so the
/// next blocked report starts a fresh count.
#[tokio::test]
async fn audit9_continue_clears_the_blocker_streak() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-blocked-continue-home");
    let cwd = unique_temp_dir("goal-blocked-continue-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("proof.txt"),
        "human prerequisite remains unavailable",
    )
    .unwrap();
    let backend = Arc::new(ScriptedReportBackend {
        calls: Default::default(),
        script: vec![
            ("blocked", "same blocker A".into(), "do A".into()),
            ("blocked", "same blocker A".into(), "do A".into()),
            ("continue", "progress".into(), "keep going".into()),
            ("blocked", "same blocker A".into(), "do A".into()),
        ],
    });
    let runtime = AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend))
        .await
        .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let created = handle
        .confirmed_test_goal(GoalSpec::new("stuck task"))
        .unwrap();
    let mut limits = created.spec.limits.clone();
    limits.max_turns = 4;
    handle
        .update_goal_limits(&created.id, created.revision, limits)
        .unwrap();

    let goal = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .expect("driver must terminate")
        .unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&cwd);

    assert_eq!(goal.status, GoalStatus::BudgetExhausted, "{goal:?}");
    assert_eq!(
        goal.blocker_rounds, 1,
        "Continue must clear the streak identity and count"
    );
    assert_eq!(goal.blocked_rounds, 1);
}

/// Audit9 F2: the same-blocker streak and its identity survive a runtime
/// restart, so recovery cannot silently reset the gate.
#[tokio::test]
async fn audit9_blocker_streak_survives_restart() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-blocked-restore-home");
    let cwd = unique_temp_dir("goal-blocked-restore-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join("proof.txt"),
        "human prerequisite remains unavailable",
    )
    .unwrap();
    let backend = Arc::new(AlwaysBlockedBackend {
        calls: Default::default(),
    });
    let runtime = AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend))
        .await
        .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("stuck task"))
        .unwrap();
    let sid = handle.session_info().session_id.clone();

    // Let the first blocked rounds settle, then restart the runtime while the
    // goal is still active.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let snapshot = handle.goal_snapshot().unwrap();
        if snapshot.blocker_rounds >= 2 || snapshot.status == GoalStatus::Blocked {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "goal never reached two same-blocker rounds: {snapshot:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    runtime.shutdown().await.unwrap();

    let backend = Arc::new(AlwaysBlockedBackend {
        calls: Default::default(),
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_session(SessionPolicy::Resume(sid))
            .with_llm_backend(backend),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let goal = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .expect("driver must terminate")
        .unwrap();
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&cwd);

    assert_eq!(goal.status, GoalStatus::Blocked, "{goal:?}");
    assert!(
        goal.blocker_rounds >= 3,
        "the streak must survive recovery: {}",
        goal.blocker_rounds
    );
    assert!(goal.blocked_rounds >= 3);
    let _ = goal.id;
}

/// Audit9 F1: the other legal ordering — acceptance commits first, a later
/// stop command is a rejected terminal command and must not latch a fault.
#[tokio::test]
async fn audit9_stop_after_commit_is_rejected_without_a_fault() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("audit9-commit-first", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .expect("driver must terminate")
        .unwrap();
    assert_eq!(completed.status, GoalStatus::Completed, "{completed:?}");
    let refused = handle.pause_goal(&completed.id, completed.revision);
    assert!(
        refused.is_err(),
        "a terminal goal cannot be paused: {refused:?}"
    );
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Idle
        ),
        "a rejected command must not latch a fault: {:?}",
        handle.goal_driver.state()
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_empty_surface_rejects_creation_and_rust_start_does_not_schedule() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-empty-home");
    let cwd = unique_temp_dir("goal-empty-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd).with_enabled_tools(Vec::<String>::new()),
    )
    .await
    .unwrap();
    assert!(
        runtime
            .handle()
            .confirmed_test_goal(GoalSpec::new("work"))
            .is_err()
    );
    runtime.shutdown().await.unwrap();
    let (runtime, h, c, backend) = goal_runtime("goal-optin", false, None).await;
    runtime
        .handle()
        .confirmed_test_goal(GoalSpec::new("work"))
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
    let _ = std::fs::remove_dir_all(h);
    let _ = std::fs::remove_dir_all(c);
}

#[tokio::test]
async fn host_lease_lives_for_exactly_one_run() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-lease", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let goal = handle
        .confirmed_test_goal_with_lease(
            GoalSpec::new("produce proof"),
            Box::new(CountingLease::new(&alive)),
        )
        .unwrap();
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the lease must be held while the run owns scheduling"
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.id, goal.id);
    assert_eq!(outcome.status, GoalStatus::Completed);
    for _ in 0..200 {
        if alive.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the lease must be released when the run ends"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn resumed_host_activation_carries_its_lease() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-lease-resume", false, None).await;
    let handle = runtime.handle();
    // Create and pause while the driver is still disabled, then enable it: the
    // resume is the activation that must own a fresh run and lease.
    let goal = handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    let paused = handle.pause_goal(&goal.id, goal.revision).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resumed = handle
        .resume_and_start_goal(
            &paused.id,
            paused.revision,
            Box::new(CountingLease::new(&alive)),
        )
        .unwrap();
    assert_eq!(resumed.status, GoalStatus::Active);
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 1);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.status, GoalStatus::Completed);
    for _ in 0..200 {
        if alive.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn refused_host_activation_destroys_its_lease() {
    use crate::runtime::*;
    // Driver disabled: a host-managed activation must not leave an active goal
    // that nothing runs.
    let (runtime, home, cwd, _) = goal_runtime("goal-lease-refuse", false, None).await;
    let handle = runtime.handle();
    let alive = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let draft = handle
        .draft_goal(GoalSpec::new("not started"), 3600)
        .unwrap();
    let disabled = handle.confirm_and_start_goal(
        &draft.id,
        draft.revision,
        Box::new(CountingLease::new(&alive)),
    );
    assert!(matches!(disabled, Err(RuntimeError::Configuration(_))));
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a refused activation must destroy its lease"
    );
    assert!(
        handle.goal_snapshot() == Some(draft.clone()),
        "a refused confirmation must leave the draft unchanged"
    );
    // A run already owning the driver refuses the activation before the
    // contract is touched, and still destroys the lease.
    handle.start_goal_driver_and_run().unwrap();
    {
        let mut control = handle.goal_driver.lock_control();
        control.driver = crate::runtime::goal::DriverState::Running {
            run_id: 1,
            goal_id: "g".into(),
        };
    }
    let busy = handle.confirm_and_start_goal(
        &draft.id,
        draft.revision,
        Box::new(CountingLease::new(&alive)),
    );
    assert!(matches!(busy, Err(RuntimeError::Busy { .. })));
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 0);
    // A latched session refuses the activation and destroys the lease too.
    {
        let mut control = handle.goal_driver.lock_control();
        control.driver = crate::runtime::goal::DriverState::Idle;
    }
    runtime.handle().goal_ctx.stores.persistence_fault.raise(
        std::path::Path::new("goal.json"),
        "injected publish failure",
    );
    let latched = handle.confirmed_test_goal_with_lease(
        GoalSpec::new("latched"),
        Box::new(CountingLease::new(&alive)),
    );
    assert!(latched.is_err());
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_persistence_fault_stops_scheduling_without_restart() {
    use crate::runtime::*;
    let (runtime, home, cwd, backend) = goal_runtime("goal-fault-stop", true, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();

    // Let the first iteration reach the provider, then latch the session as a
    // real publish failure would.
    for _ in 0..200 {
        if backend.calls.load(std::sync::atomic::Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        backend.calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "goal turn never started"
    );
    runtime.handle().goal_ctx.stores.persistence_fault.raise(
        std::path::Path::new("goal.json"),
        "injected publish failure",
    );

    // An unrecoverable failure must stop scheduling: wait_goal returns the
    // runtime fault it could not persist instead of spinning on a disk state
    // that still reads `active`.
    let waited = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
        .await
        .expect("driver must stop after an unrecoverable failure");
    assert!(
        waited.is_err(),
        "the runtime fault must surface: {waited:?}"
    );
    assert_eq!(
        handle
            .goal_driver
            .drive_attempts
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a failed run must not restart while the goal still reads active"
    );
    // A faulted driver refuses new activation instead of pretending to run.
    assert!(handle.start_goal_driver().is_err());
    assert!(
        handle
            .confirmed_test_goal(GoalSpec::new("second goal"))
            .is_err()
    );
    assert_eq!(
        handle
            .goal_snapshot()
            .map(|current| (current.id, current.status)),
        Some((goal.id.clone(), GoalStatus::Active)),
        "a fault that cannot be persisted must not fake a failed status"
    );

    let shutdown =
        tokio::time::timeout(std::time::Duration::from_secs(20), runtime.shutdown()).await;
    assert!(
        shutdown.is_ok(),
        "shutdown must stay bounded after the driver already stopped"
    );
    assert!(shutdown.unwrap().is_ok());
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// The same fault-stop scenario, but observed from outside the process: a
/// hot restart loop cannot hide behind the test executor's scheduling when
/// the child process itself is bounded. The child asserts the observable
/// attempt count; the parent enforces the wall-clock deadline and kills a
/// child that never stops.
#[test]
fn goal_fault_stop_is_bounded_in_a_child_process() {
    if std::env::var("MINK_GOAL_FAULT_CHILD").is_ok() {
        return; // the child runs `goal_fault_stop_child` instead
    }
    let exe = std::env::current_exe().expect("test binary");
    let mut child = std::process::Command::new(exe)
        .args([
            "--exact",
            "runtime::builder::tests::goal_lifecycle::goal_fault_stop_child",
            "--nocapture",
        ])
        .env("MINK_GOAL_FAULT_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the fault-stop child");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match child.try_wait().expect("poll the fault-stop child") {
            Some(status) => {
                let output = child
                    .wait_with_output()
                    .expect("collect the fault-stop child output");
                assert!(
                    status.success(),
                    "fault-stop child failed: {}\n{}{}",
                    status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(
                    String::from_utf8_lossy(&output.stdout).contains("goal_fault_stop_child_ok"),
                    "the child never reported the scenario: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "fault-stop child never stopped; the driver is likely retrying in a hot loop"
                );
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

#[test]
fn goal_fault_stop_child() {
    if std::env::var("MINK_GOAL_FAULT_CHILD").is_err() {
        return; // parent-only discovery
    }
    use crate::runtime::*;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("child runtime");
    runtime.block_on(async {
        let (runtime, home, cwd, backend) = goal_runtime("goal-fault-child", true, None).await;
        let handle = runtime.handle();
        handle.start_goal_driver_and_run().unwrap();
        handle
            .confirmed_test_goal(GoalSpec::new("produce proof"))
            .unwrap();
        for _ in 0..200 {
            if backend.calls.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        runtime.handle().goal_ctx.stores.persistence_fault.raise(
            std::path::Path::new("goal.json"),
            "injected publish failure",
        );
        let waited = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
            .await
            .expect("the fault must stop the driver");
        assert!(waited.is_err(), "the runtime fault must surface");
        assert_eq!(
            handle
                .goal_driver
                .drive_attempts
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the child observed a driver restart"
        );
        runtime.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(cwd);
        println!("goal_fault_stop_child_ok");
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_driver_fault_is_stable_and_blocks_reactivation() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-sink-fault-home");
    let cwd = unique_temp_dir("goal-sink-fault-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("proof.txt"), "objective evidence").unwrap();
    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_llm_backend(backend.clone())
            .with_goal_event_sink(Arc::new(FailingSink)),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    let goal = handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();

    let waited = tokio::time::timeout(std::time::Duration::from_secs(10), handle.wait_goal())
        .await
        .expect("a sink failure must stop the run");
    let error = waited.expect_err("the driver fault must surface");
    assert!(
        format!("{error}").contains("sink") || format!("{error}").contains("fault"),
        "the fault keeps its cause: {error}"
    );
    assert_eq!(
        handle
            .goal_driver
            .drive_attempts
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a faulted driver must not restart automatically"
    );
    // Whether the turn outcome was publishable or not, the runtime fault is
    // the stable terminal driver state...
    assert!(matches!(
        handle.goal_driver.state(),
        crate::runtime::goal::DriverState::Faulted { .. }
    ));
    // ...and reactivation is refused until the session is reopened, even for a
    // goal whose failed status made it replaceable.
    assert!(handle.start_goal_driver().is_err());
    assert!(
        handle
            .confirmed_test_goal(GoalSpec::new("replacement"))
            .is_err()
    );
    let _ = goal;
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn failed_stop_intent_stops_the_run_and_never_reads_as_saved() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-stop-intent", true, None).await;
    let handle = runtime.handle();
    // A goal exists without a real run; force the driver into Running so the
    // pause path believes it owns an in-flight run.
    let goal = handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    {
        let mut control = handle.goal_driver.lock_control();
        control.enabled = true;
        control.driver = crate::runtime::goal::DriverState::Running {
            run_id: 7,
            goal_id: goal.id.clone(),
        };
    }
    runtime.handle().goal_ctx.stores.persistence_fault.raise(
        std::path::Path::new("goal.json"),
        "injected publish failure",
    );

    let paused = handle.pause_goal(&goal.id, goal.revision);
    assert!(
        paused.is_err(),
        "a stop intent that cannot be saved must fail"
    );
    // The failed intent latches the run in `Stopping` with its reason; the
    // stable fault waits for the run's own settlement. This fake run has no
    // worker, so waiters are not woken yet: a fault must never be published
    // before the execution it interrupts has actually exited.
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Stopping { run_id: 7, reason, .. }
                if reason.contains("stop intent could not be persisted")
        ),
        "the run must stop instead of continuing on an unpersisted stop intent: {:?}",
        handle.goal_driver.state()
    );
    let pending =
        tokio::time::timeout(std::time::Duration::from_millis(200), handle.wait_goal()).await;
    assert!(
        pending.is_err(),
        "the stable fault must not be published before execution exits"
    );
    // The run settlement publishes the stable fault once execution is gone
    // (what `run_goal_worker` does for a real run).
    handle.finish_goal_run(
        7,
        &goal.id,
        Some("goal stop intent could not be persisted".into()),
    );
    let waited = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .expect("waiters must be woken by the stable fault");
    assert!(waited.is_err());
    // Reactivation is refused while the run is faulted.
    assert!(handle.start_goal_driver().is_err());
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn stale_run_finish_cannot_clear_a_newer_run() {
    let (runtime, home, cwd, _) = goal_runtime("goal-stale-run", false, None).await;
    let handle = runtime.handle();
    // A newer run owns the driver with run_id 2.
    {
        let mut control = handle.goal_driver.control.lock().unwrap();
        control.driver = crate::runtime::goal::DriverState::Running {
            run_id: 2,
            goal_id: "goal-b".into(),
        };
    }
    // A late cleanup for the older run must not touch the newer run.
    handle.finish_goal_run(1, "goal-a", None);
    assert!(matches!(
        handle.goal_driver.state(),
        crate::runtime::goal::DriverState::Running { run_id: 2, .. }
    ));
    handle.finish_goal_run(2, "goal-b", None);
    assert!(matches!(
        handle.goal_driver.state(),
        crate::runtime::goal::DriverState::Idle
    ));
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_settlement_is_idempotent_and_rejects_conflicting_facts() {
    use crate::runtime::*;
    use crate::session::goal::{ApplyResult, IterationKey, SettlementFacts};
    let (runtime, home, cwd, _) = goal_runtime("goal-settle", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.status, GoalStatus::Completed);
    let iteration = outcome.iterations.last().expect("settled iteration");
    let key = IterationKey {
        goal_id: outcome.id.clone(),
        iteration: iteration.iteration,
        turn_id: iteration.turn_id.clone(),
    };
    let result = crate::agent::orchestrator::TurnRunResult {
        billing_turn_id: iteration.billing_turn_id.clone().expect("billing id"),
        status: TurnStatus::Ok,
        failure: None,
        tool_call_count: 0,
        tool_error_count: 0,
        error: None,
        usage_records: vec![],
        usage: Default::default(),
        text: String::new(),
        thinking: String::new(),
    };
    let before = handle.goal_snapshot().unwrap();
    let applied = handle
        .goal_ctx
        .stores
        .goal_store
        .settle_iteration(&key, &result, &SettlementFacts::default())
        .unwrap();
    assert_eq!(applied, ApplyResult::AlreadyApplied);
    let after = handle.goal_snapshot().unwrap();
    assert_eq!(
        before.revision, after.revision,
        "an idempotent repeat must not bump the revision"
    );
    assert_eq!(
        before.usage.request_count, after.usage.request_count,
        "an idempotent repeat must not duplicate usage"
    );
    // Conflicting facts for the same identity are an error, not an overwrite.
    let conflicting = crate::agent::orchestrator::TurnRunResult {
        billing_turn_id: "turn-someone-else".into(),
        ..result.clone()
    };
    assert!(
        handle
            .goal_ctx
            .stores
            .goal_store
            .settle_iteration(&key, &conflicting, &SettlementFacts::default())
            .is_err(),
        "conflicting facts must not silently overwrite a settled iteration"
    );
    // An unknown identity is refused instead of settling whatever is current.
    let unknown = IterationKey {
        goal_id: outcome.id.clone(),
        iteration: iteration.iteration,
        turn_id: "turn-that-never-ran".into(),
    };
    assert!(
        handle
            .goal_ctx
            .stores
            .goal_store
            .settle_iteration(&unknown, &result, &SettlementFacts::default())
            .is_err()
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn stale_settlement_fills_history_without_touching_the_replacement() {
    use crate::runtime::*;
    use crate::session::goal::{IterationKey, SettlementFacts};
    let (runtime, home, cwd, _) = goal_runtime("goal-settle-history", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("first goal"))
        .unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    let iteration = first.iterations.last().expect("settled iteration");
    let key = IterationKey {
        goal_id: first.id.clone(),
        iteration: iteration.iteration,
        turn_id: iteration.turn_id.clone(),
    };
    let result = crate::agent::orchestrator::TurnRunResult {
        billing_turn_id: iteration.billing_turn_id.clone().expect("billing id"),
        status: TurnStatus::Ok,
        failure: None,
        tool_call_count: 0,
        tool_error_count: 0,
        error: None,
        usage_records: vec![],
        usage: Default::default(),
        text: String::new(),
        thinking: String::new(),
    };
    // Stop the driver before creating the replacement, so the assertions below
    // describe a stable current goal.
    handle.goal_driver.disable();
    let replacement = handle
        .confirmed_test_goal(GoalSpec::new("replacement goal"))
        .unwrap();
    let before = handle.goal_snapshot().unwrap();
    assert_eq!(before.id, replacement.id);
    // The archived goal's round settles in history; the replacement is untouched.
    handle
        .goal_ctx
        .stores
        .goal_store
        .settle_iteration(&key, &result, &SettlementFacts::default())
        .expect("archive settlement");
    let after = handle.goal_snapshot().unwrap();
    assert_eq!(after.id, replacement.id);
    assert_eq!(after.revision, before.revision);
    assert!(
        after.usage_records.is_empty(),
        "the replacement must not absorb the old goal's usage"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn identity_bound_updates_never_touch_a_replacement() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("goal-update-identity", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver_and_run().unwrap();
    handle
        .confirmed_test_goal(GoalSpec::new("first goal"))
        .unwrap();
    let first = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.status, GoalStatus::Completed);
    // Stop the driver before creating the replacement so the current goal is
    // stable while the stale update is attempted.
    handle.goal_driver.disable();
    let replacement = handle
        .confirmed_test_goal(GoalSpec::new("replacement goal"))
        .unwrap();
    // A decision that belongs to the finished goal must not reach the
    // replacement, which is what every driver write now relies on.
    let applied = handle
        .goal_ctx
        .stores
        .goal_store
        .transition_if_current(
            &first.id,
            crate::session::goal::GoalTransition::FailRun {
                reason: "stale decision".into(),
            },
        )
        .unwrap();
    assert!(applied.is_none(), "a stale update must apply to nothing");
    let current = handle.goal_snapshot().unwrap();
    assert_eq!(current.id, replacement.id);
    assert_eq!(current.status, GoalStatus::Active);
    assert_eq!(current.reason, replacement.reason);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_restore_active_requires_explicit_rust_driver_and_paused_stays_paused() {
    use crate::runtime::*;
    for paused in [false, true] {
        let (runtime, home, cwd, _) = goal_runtime("goal-restore", false, None).await;
        let sid = runtime.session_info().session_id.clone();
        let handle = runtime.handle();
        let goal = handle
            .confirmed_test_goal(GoalSpec::new("produce proof"))
            .unwrap();
        if paused {
            handle.pause_goal(&goal.id, goal.revision).unwrap();
        }
        runtime.shutdown().await.unwrap();
        let backend = Arc::new(GoalReportBackend {
            calls: Default::default(),
            continue_first: false,
        });
        let runtime = AgentRuntime::start(
            AgentOptions::new(&home, &cwd)
                .with_session(SessionPolicy::Resume(sid))
                .with_llm_backend(backend.clone()),
        )
        .await
        .unwrap();
        let handle = runtime.handle();
        assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            handle.goal_snapshot().unwrap().status,
            if paused {
                GoalStatus::Paused
            } else {
                GoalStatus::Active
            }
        );
        // Arming is not starting: a restored active goal must not run until
        // an explicit lifecycle call picks it up.
        handle.start_goal_driver().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            backend.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "start_goal_driver must not start a run"
        );
        assert!(!handle.goal_driver.is_active());
        handle.start_goal_driver_and_run().unwrap();
        let goal = handle.wait_goal().await.unwrap();
        assert_eq!(
            goal.status,
            if paused {
                GoalStatus::Paused
            } else {
                GoalStatus::Completed
            }
        );
        assert_eq!(
            backend.calls.load(std::sync::atomic::Ordering::SeqCst),
            if paused { 0 } else { 3 }
        );
        runtime.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(cwd);
    }
}

/// Audit8 F1: a restored `active` goal is started by the leased activation,
/// and by nothing else. Arming first must issue no provider request and hold
/// no run; `resume_and_start_goal` is then the single owner of the run and of
/// the host lease it carries.
#[tokio::test]
async fn audit8_restored_active_goal_starts_only_through_a_leased_resume() {
    use crate::runtime::*;
    // Create while scheduling is disabled, then close the runtime: the disk
    // keeps the contract as `active` with no run behind it.
    let (runtime, home, cwd, backend) = goal_runtime("audit8-resume", false, None).await;
    let sid = runtime.session_info().session_id.clone();
    let handle = runtime.handle();
    let goal = handle
        .confirmed_test_goal(GoalSpec::new("produce proof"))
        .unwrap();
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();

    let backend = Arc::new(GoalReportBackend {
        calls: Default::default(),
        continue_first: false,
    });
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_session(SessionPolicy::Resume(sid))
            .with_llm_backend(backend.clone()),
    )
    .await
    .unwrap();
    let handle = runtime.handle();
    let restored = handle.goal_snapshot().unwrap();
    assert_eq!(restored.status, GoalStatus::Active);
    handle.start_goal_driver().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        backend.calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "reopen + arm must not issue a provider request"
    );
    assert!(!handle.goal_driver.is_active());

    let alive = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let resumed = handle
        .resume_and_start_goal(
            &goal.id,
            restored.revision,
            Box::new(CountingLease::new(&alive)),
        )
        .unwrap();
    assert_eq!(resumed.status, GoalStatus::Active);
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 1);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.status, GoalStatus::Completed);
    for _ in 0..200 {
        if alive.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the resumed run's lease is released when the run ends"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// Audit8 F1 (re-audit): a pause whose intent cannot be *published* (a plain
/// pre-publication failure, no global latch) must still stop the run, keep
/// execution ownership until the checker exits, and never let the released
/// checker commit an accepted completion.
#[cfg(unix)]
#[tokio::test]
async fn audit8_prepublication_stop_failure_stops_the_run() {
    use crate::runtime::*;
    use std::os::unix::fs::PermissionsExt;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("audit8-stop-readonly", false, Some(verifier.clone())).await;
    let session_dir = runtime
        .session_info()
        .conversation_path
        .parent()
        .unwrap()
        .to_path_buf();
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let mut spec = GoalSpec::new("produce proof");
    spec.acceptance = GoalAcceptance::Verifier {
        id: "held-check-v1".into(),
    };
    handle.confirmed_test_goal(spec).unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        verifier.entered.notified(),
    )
    .await
    .unwrap();
    // The revision moved while the round settled: the stop command must carry
    // the live revision, or a stale-revision rejection (not a publication
    // failure) would be the thing under test.
    let goal = handle.goal_snapshot().unwrap();
    // A publication failure that does not latch the session: the stop intent
    // cannot be stored, but the session is not globally faulted.
    let original = std::fs::metadata(&session_dir).unwrap().permissions();
    std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let stopped = handle.pause_goal(&goal.id, goal.revision);
    std::fs::set_permissions(&session_dir, original).unwrap();
    assert!(stopped.is_err(), "{stopped:?}");
    assert!(
        handle.goal_ctx.stores.persistence_fault.check().is_ok(),
        "the failure is a plain publication failure, not a latched fault"
    );
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Stopping { reason, .. }
                if reason.contains("stop intent could not be persisted")
        ),
        "the run must be latched for stopping: {:?}",
        handle.goal_driver.state()
    );
    // The checker still owns the turn gate: the fault is not stable yet.
    let early =
        tokio::time::timeout(std::time::Duration::from_millis(200), handle.wait_goal()).await;
    assert!(
        early.is_err(),
        "the fault must wait for the checker to exit"
    );
    verifier.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .expect("the settlement must publish the stable fault");
    assert!(outcome.is_err(), "{outcome:?}");
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Faulted { .. }
        ),
        "the refused pause must leave a stable fault: {:?}",
        handle.goal_driver.state()
    );
    let current = handle.goal_snapshot().unwrap();
    assert_eq!(
        current.status,
        GoalStatus::Active,
        "a refused pause must not be saved"
    );
    assert_eq!(
        current.completion_basis, None,
        "the released checker must not commit an acceptance after the refused stop"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// Audit8 F2 (re-audit): a host activation that cannot start a run must be
/// refused *before* any state is written, never swallowed after the contract
/// is already durable.
#[tokio::test]
async fn audit8_activation_without_executor_leaves_no_contract() {
    use crate::runtime::*;
    let (runtime, home, cwd, _) = goal_runtime("audit8-no-executor", false, None).await;
    let handle = runtime.handle();
    handle.start_goal_driver().unwrap();
    let alive = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let draft = handle
        .draft_goal(GoalSpec::new("cannot run"), 3600)
        .unwrap();
    let thread_draft = draft.clone();
    let thread_handle = handle.clone();
    let thread_alive = alive.clone();
    let leased = std::thread::spawn(move || {
        thread_handle.confirm_and_start_goal(
            &thread_draft.id,
            thread_draft.revision,
            Box::new(CountingLease::new(&thread_alive)),
        )
    })
    .join()
    .unwrap();
    let error = leased.expect_err("activation without an executor must be refused");
    assert!(
        format!("{error}").contains("async runtime"),
        "the refusal names the missing executor: {error}"
    );
    assert!(
        handle.goal_snapshot() == Some(draft.clone()),
        "the draft must not change when activation has no executor"
    );
    assert_eq!(alive.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Idle
        ),
        "a refused activation must not latch a fault"
    );
    // Lifecycle confirmation also leaves the existing draft unchanged.
    let thread_draft = draft.clone();
    let thread_handle = handle.clone();
    let created = std::thread::spawn(move || {
        thread_handle.confirm_goal(&thread_draft.id, thread_draft.revision)
    })
    .join()
    .unwrap();
    assert!(created.is_err(), "{created:?}");
    assert_eq!(handle.goal_snapshot(), Some(draft));
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// Audit8 F3 (re-audit): a failed stop intent keeps the run in `Stopping`
/// until the execution it interrupts really exits; only then is the fault
/// stable, and the reason survives the settlement.
#[tokio::test]
async fn audit8_failed_stop_waits_for_gate_before_stable_fault() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("audit8-stop-latch", false, Some(verifier.clone())).await;
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
    let goal = handle.goal_snapshot().unwrap();
    runtime.handle().goal_ctx.stores.persistence_fault.raise(
        std::path::Path::new("goal.json"),
        "audit injected publish failure",
    );
    assert!(handle.pause_goal(&goal.id, goal.revision).is_err());
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Stopping { .. }
        ),
        "the fault must not be stable while the checker exits: {:?}",
        handle.goal_driver.state()
    );
    assert!(handle.active_turn_id().is_some());
    let early =
        tokio::time::timeout(std::time::Duration::from_millis(200), handle.wait_goal()).await;
    assert!(
        early.is_err(),
        "a fault is stable only after execution exits"
    );
    verifier.release.notify_one();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .expect("waiters must be woken by the stable fault");
    let error = outcome.expect_err("the failed stop intent must surface as a fault");
    assert!(
        format!("{error}").contains("stop intent could not be persisted"),
        "the stop reason survives: {error}"
    );
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Faulted { .. }
        ),
        "{:?}",
        handle.goal_driver.state()
    );
    for _ in 0..200 {
        if alive.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        alive.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the lease is released once the checker has exited"
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// A stale revision is a rejected command, not a failed stop intent: it must
/// not latch or interrupt the live run.
#[tokio::test]
async fn audit8_stale_stop_revision_does_not_stop_the_run() {
    use crate::runtime::*;
    let verifier = Arc::new(HeldGoalVerifier {
        entered: Default::default(),
        release: Default::default(),
    });
    let (runtime, home, cwd, _) =
        goal_runtime("audit8-stale-stop", false, Some(verifier.clone())).await;
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
    let goal = handle.goal_snapshot().unwrap();
    let stale = handle.pause_goal(&goal.id, goal.revision + 1);
    assert!(stale.is_err(), "{stale:?}");
    assert!(
        matches!(
            handle.goal_driver.state(),
            crate::runtime::goal::DriverState::Running { .. }
        ),
        "a stale command must not latch the run: {:?}",
        handle.goal_driver.state()
    );
    assert!(handle.active_turn_id().is_some());
    // The run is still stoppable with the live revision.
    let cancelled = handle.cancel_goal(&goal.id, goal.revision).unwrap();
    assert_eq!(cancelled.status, GoalStatus::Cancelled);
    verifier.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while handle.active_turn_id().is_some()
            || alive.load(std::sync::atomic::Ordering::SeqCst) > 0
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the cancelled run must exit and release its lease");
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn goal_budget_change_updates_the_inflight_absolute_deadline() {
    use crate::runtime::*;
    let home = unique_temp_dir("goal-budget-live-home");
    let cwd = unique_temp_dir("goal-budget-live-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let backend = Arc::new(PendingEstablishBackend::default());
    let runtime =
        AgentRuntime::start(AgentOptions::new(&home, &cwd).with_llm_backend(backend.clone()))
            .await
            .unwrap();
    let handle = runtime.handle();
    handle.confirmed_test_goal(GoalSpec::new("work")).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    backend.entered.notified().await;
    let goal = handle.goal_snapshot().unwrap();
    let mut limits = goal.spec.limits.clone();
    limits.deadline_unix_secs = crate::session::goal::goal_now() + 1;
    handle
        .update_goal_limits(&goal.id, goal.revision, limits)
        .unwrap();
    let exhausted = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exhausted.status, GoalStatus::BudgetExhausted);
    assert_eq!(*backend.calls.lock().unwrap(), 1);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn subagent_executor_retains_child_and_parent_session_leases() {
    let home = unique_temp_dir("lease-child-home");
    let cwd = unique_temp_dir("lease-child-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let runtime = AgentRuntime::start(AgentOptions::new(&home, &cwd))
        .await
        .unwrap();
    let parent_dir = runtime
        .session_info()
        .conversation_path
        .parent()
        .unwrap()
        .to_path_buf();
    let child = crate::agent::sub_executor::SubAgentExecutor::new(
        runtime.ctx.clone(),
        "sub-lease-test".into(),
        false,
        runtime.ctx.config.clone(),
    )
    .await
    .unwrap();
    // Child context is private to its executor: keeping the executor alive also
    // keeps any child tools' lease chain alive after parent teardown.
    runtime.shutdown().await.unwrap();
    assert!(crate::runtime::SessionLease::acquire_directory(&parent_dir).is_err());
    assert!(
        crate::runtime::SessionLease::acquire_directory(
            &parent_dir.join("subagents/sub-lease-test")
        )
        .is_err()
    );
    drop(child);
    // Dropping the executor releases the child's chain; the release can land on
    // another task, so wait for it with a short deadline instead of racing it
    // (a workspace-wide run schedules enough work to lose that race).
    let release_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match crate::runtime::SessionLease::acquire_directory(&parent_dir) {
            Ok(_lease) => break,
            Err(_) if std::time::Instant::now() < release_deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => panic!("parent lease still held after the executor dropped: {error}"),
        }
    }
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

/// A goal boundary must be typed, not something a host recognises by reading
/// the model-visible sentence.
#[tokio::test]
async fn goal_boundary_messages_are_typed_for_hosts() {
    use crate::runtime::*;
    let (runtime, home, cwd, _backend) = goal_runtime("goal-boundary", false, None).await;
    let handle = runtime.handle();
    let spec = GoalSpec::new("ship the parser");
    handle.confirmed_test_goal(spec).unwrap();
    handle.start_goal_driver_and_run().unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle.wait_goal()).await;
    // The session directory is under the home, not the working directory.
    let directory = handle
        .session_info()
        .conversation_path
        .parent()
        .unwrap()
        .to_path_buf();
    runtime.shutdown().await.unwrap();

    let reader = crate::runtime::session::SessionReader::new(directory);
    let rows = reader.conversation().unwrap();
    let mut kinds = Vec::new();
    for row in rows {
        let message = row.pointer("/_mink/goal_message");
        if let Some(message) = message {
            kinds.push((
                message["kind"].as_str().unwrap_or_default().to_string(),
                row.pointer("/_mink/source")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                message["objective"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                message["iteration"].as_u64().unwrap_or_default(),
            ));
        }
    }
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);

    assert!(
        !kinds.is_empty(),
        "goal rounds must publish a typed boundary"
    );
    assert_eq!(kinds[0].0, "start", "first boundary: {:?}", kinds[0]);
    // The source tag stays stable for every round so existing consumers keep
    // working; start versus continuation is carried by the typed `kind`.
    assert!(
        kinds.iter().all(|entry| entry.1 == "goal_continuation"),
        "{kinds:?}"
    );
    assert_eq!(
        kinds[0].2, "ship the parser",
        "objective travels structured"
    );
    assert_eq!(kinds[0].3, 1, "first boundary is round one");
    for entry in &kinds[1..] {
        assert_eq!(entry.0, "continuation", "later boundaries: {entry:?}");
    }
}
