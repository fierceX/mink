use super::events::{apply_presentation, bounded_text, update_activity};
use super::turn_guard::turn_timeout_secs;
use super::*;

#[tokio::test]
async fn goal_turn_sink_strips_heavy_fields_and_bounds_subagent_output() {
    use mink::runtime::EventSink;

    let (tx, mut rx) = broadcast::channel(8);
    let sink = GoalTurnSink {
        tx,
        sequence: Arc::new(AtomicU64::new(1)),
        projection: Arc::new(OnceLock::new()),
    };
    let huge = "x".repeat(40 * 1024);
    sink.on_event(AgentEvent {
        turn_id: Some(mink::runtime::TurnId::new("t-1")),
        sequence: 1,
        kind: mink::runtime::AgentEventKind::SubAgentOutput {
            session_id: "sub_1".into(),
            status: "completed".into(),
            label: None,
            stop_reason: None,
            thinking: huge.clone(),
            text: huge,
            in_tokens: 0,
            out_tokens: 0,
        },
    })
    .await
    .unwrap();
    let ev = rx.recv().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&ev.payload).unwrap();
    assert!(parsed["text"].as_str().unwrap().len() < 33 * 1024);
    assert!(parsed["thinking"].as_str().unwrap().len() < 33 * 1024);
}

#[tokio::test]
async fn active_operation_guard_suspends_idle_until_dropped() {
    let dir = std::env::temp_dir().join(format!("mink-op-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let runtime = SessionRuntime::open(AgentOptions::new(&dir, &dir).with_api_key("synthetic"))
        .await
        .unwrap();
    assert!(runtime.idle_for().is_some());
    let guard = runtime.begin_operation();
    assert!(runtime.idle_for().is_none());
    drop(guard);
    assert!(runtime.idle_for().is_some());
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn turn_timeout_defaults_to_two_hours_and_accepts_overrides() {
    assert_eq!(turn_timeout_secs(None), 7_200);
    assert_eq!(turn_timeout_secs(Some("30")), 30);
    assert_eq!(turn_timeout_secs(Some("not-a-number")), 7_200);
}

#[test]
fn bounded_text_keeps_utf8_boundaries_and_marks_the_cut() {
    let text = "汉".repeat(20);
    let bounded = bounded_text(&text, 4);
    assert!(bounded.starts_with("汉"));
    assert!(bounded.contains("truncated"));
    assert!(bounded.len() < text.len());
    assert_eq!(bounded_text("short", 100), "short");
}

fn projection(rows: Vec<serde_json::Value>) -> Projection {
    let row_bytes = rows
        .iter()
        .map(|row| {
            serde_json::to_vec(row)
                .map(|bytes| bytes.len())
                .unwrap_or(0)
        })
        .collect();
    Projection {
        generation: "g".into(),
        watermark: 0,
        rows,
        row_bytes,
        has_older: false,
        progress: vec![],
        current_turn: None,
        phase: "idle",
        last_final: None,
        resources: serde_json::json!({}),
        diagnostics: serde_json::json!({}),
        activity: serde_json::json!({}),
        goal: None,
    }
}

fn turn(n: usize, body_bytes: usize) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({ "seq": n, "role": "user", "content": format!("prompt {n}") }),
        serde_json::json!({
            "seq": n + 1,
            "role": "assistant",
            "content": [{ "type": "text", "text": "x".repeat(body_bytes) }]
        }),
    ]
}

#[test]
fn snapshot_tail_keeps_recent_turns_and_never_splits_the_latest() {
    let small: Vec<_> = (1..=6).flat_map(|n| turn(n * 2, 10)).collect();
    let mut p = projection(small);
    p.trim();
    assert!(p.has_older);
    assert_eq!(p.rows.first().unwrap()["seq"], 6);
    assert_eq!(p.rows.iter().filter(|row| row["role"] == "user").count(), 4);

    let large: Vec<_> = (1..=6).flat_map(|n| turn(n * 2, 100 * 1024)).collect();
    let mut p = projection(large);
    p.trim();
    assert!(p.has_older);
    assert!(p.rows.first().unwrap()["seq"].as_u64().unwrap() > 1);
    assert_eq!(p.rows.last().unwrap()["role"], "assistant");

    let mut rows = turn(2, 300 * 1024);
    rows.extend(turn(4, 300 * 1024));
    let mut p = projection(rows);
    p.trim();
    assert!(p.has_older);
    assert_eq!(
        p.rows.first().unwrap()["seq"],
        4,
        "the newest turn stays whole"
    );
    assert_eq!(p.rows.last().unwrap()["role"], "assistant");
}

#[test]
fn opening_a_long_projection_serializes_only_the_retained_tail() {
    let rows: Vec<_> = (1..=8).flat_map(|n| turn(n * 2, 10)).collect();
    let mut p = projection(rows);
    p.trim_initial();
    assert!(p.has_older);
    assert_eq!(p.rows.iter().filter(|row| row["role"] == "user").count(), 4);
    assert_eq!(p.row_bytes.len(), p.rows.len());
}

#[test]
fn goal_summary_drops_heavy_history_and_keeps_bar_fields() {
    let goal = serde_json::json!({
        "id": "g1",
        "revision": 7,
        "status": "active",
        "phase": "idle",
        "spec": {"objective": "ship it"},
        "iterations": [{"n": 1}, {"n": 2}],
        "reviews": [{"verdict": "partial"}, {"verdict": "satisfied"}],
        "continuations": [{"n": 1}, {"n": 2}],
        "usage_records": [{"tokens": 1}],
        "checker_records": [{"ok": true}],
        "review_report": {"long": "x"},
        "report": {"long": "y"},
        "review_candidate": {"iteration": 1},
        "environment": "host",
        "pending_effects": []
    });
    let summary = goal_summary_value(&goal);
    assert_eq!(summary["id"], "g1");
    assert_eq!(summary["iterations_count"], 2);
    assert_eq!(summary["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(summary["reviews"][0]["verdict"], "satisfied");
    assert_eq!(summary["continuations"].as_array().unwrap().len(), 1);
    for key in [
        "iterations",
        "usage_records",
        "checker_records",
        "review_report",
        "report",
        "environment",
    ] {
        assert!(
            summary.get(key).is_none(),
            "{key} must not be streamed: {summary}"
        );
    }
    assert_eq!(summary["review_candidate"]["iteration"], 1);
}

#[test]
fn activity_tracks_reliable_progress_and_clears_wait_at_boundaries() {
    let mut activity = serde_json::json!({"work_state":"idle","wait_elapsed_secs":null});
    update_activity(&mut activity, &serde_json::json!({"type":"turn_started"}));
    update_activity(
        &mut activity,
        &serde_json::json!({"type":"info","message":"Waiting for model response... elapsed=23s idle=5s"}),
    );
    assert_eq!(activity["work_state"], "waiting");
    assert_eq!(activity["wait_elapsed_secs"], 23);
    // Formal history commits and stats must not erase current activity.
    update_activity(
        &mut activity,
        &serde_json::json!({"type":"conversation_committed"}),
    );
    assert_eq!(activity["wait_elapsed_secs"], 23);
    for (event, work) in [
        (serde_json::json!({"type":"thinking"}), "thinking"),
        (serde_json::json!({"type":"text"}), "generating"),
        (serde_json::json!({"type":"tool_call"}), "tool"),
        (serde_json::json!({"type":"tool_result"}), "waiting"),
        (
            serde_json::json!({"type":"sub_agent_status","session_id":"child","status":"running"}),
            "sub-agent",
        ),
        (
            serde_json::json!({"type":"sub_agent_output","session_id":"child"}),
            "waiting",
        ),
        (
            serde_json::json!({"type":"info","message":"Compressing..."}),
            "compacting",
        ),
        (
            serde_json::json!({"type":"turn_final","outcome":{"error":"cancelled"}}),
            "error",
        ),
        (
            serde_json::json!({"type":"turn_final","outcome":{"error":null}}),
            "idle",
        ),
    ] {
        update_activity(&mut activity, &event);
        assert_eq!(activity["work_state"], work);
        assert!(activity["wait_elapsed_secs"].is_null());
    }
}

#[test]
fn activity_keeps_remaining_children_running_and_interruption_idle() {
    let mut activity = serde_json::json!({"work_state":"idle","wait_elapsed_secs":null});
    for id in ["first", "second"] {
        update_activity(
            &mut activity,
            &serde_json::json!({"type":"sub_agent_status","session_id":id,"status":"launched"}),
        );
    }
    update_activity(
        &mut activity,
        &serde_json::json!({"type":"sub_agent_output","session_id":"first","status":"ok"}),
    );
    assert_eq!(activity["work_state"], "sub-agent");
    assert_eq!(activity["active_sub_agents"], serde_json::json!(["second"]));
    update_activity(
        &mut activity,
        &serde_json::json!({"type":"turn_final","outcome":{"status":"interrupted","error":"interrupted"}}),
    );
    assert_eq!(activity["work_state"], "idle");
    assert_eq!(activity["active_sub_agents"], serde_json::json!([]));
}

fn session_info() -> SessionInfo {
    let path = std::path::PathBuf::from("/tmp/mink-server-runtime-test");
    SessionInfo {
        session_id: "session".into(),
        session_ref: "session".into(),
        is_new: false,
        home: path.clone(),
        cwd: path.clone(),
        events_path: path.join("events.jsonl"),
        conversation_path: path.join("conversation.jsonl"),
        artifacts_dir: path.join("artifacts"),
        summary_path: path.join("summary.md"),
        usage_path: path.join("usage.jsonl"),
        plan_path: path.join("plan.md"),
        plan_draft_path: path.join("plan.draft"),
        todos_path: path.join("todos.json"),
    }
}

#[test]
fn forced_terminal_is_published_once_after_closed() {
    let (tx, mut rx) = broadcast::channel(8);
    let sequence = AtomicU64::new(1);
    let state = Arc::new(Mutex::new(RuntimeState {
        runtime: None,
        phase: RuntimePhase::Closing,
        turn_task: None,
        last_idle: Instant::now(),
        forced_terminal: Some(ForcedTerminal {
            reason: "turn timed out".into(),
            saw_stop: false,
            saw_final: false,
            turn_id: "turn".into(),
            session: Box::new(session_info()),
        }),
    }));

    finalize_shutdown(&state, &tx, &sequence, None, Some("shutdown error"));
    assert_eq!(state.lock().unwrap().phase, RuntimePhase::Closed);
    let stop: serde_json::Value = serde_json::from_str(&rx.try_recv().unwrap().payload).unwrap();
    let final_event: serde_json::Value =
        serde_json::from_str(&rx.try_recv().unwrap().payload).unwrap();
    assert_eq!(stop["type"], "stop");
    assert_eq!(final_event["type"], "turn_final");
    assert_eq!(final_event["outcome"]["status"], "failed");
    assert!(
        final_event["outcome"]["error"]
            .as_str()
            .unwrap()
            .contains("shutdown error")
    );

    finalize_shutdown(&state, &tx, &sequence, None, None);
    assert!(rx.try_recv().is_err());
}
/// Regression for the audit's lock-order inversion: interrupt/shutdown
/// hold the state lock and then publish (projection), so snapshot/detail
/// must never hold the projection while waiting for state. The goal is
/// rendered from the mirrored projection instead of a live state read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_and_detail_do_not_wait_for_the_state_lock() {
    let dir = std::env::temp_dir().join(format!("mink-server-runtime-lock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let runtime = Arc::new(
        SessionRuntime::open(AgentOptions::new(&dir, &dir).with_api_key("synthetic"))
            .await
            .unwrap(),
    );
    // Simulate interrupt/shutdown owning the state lock.
    let state = runtime.state.lock().unwrap();
    let worker_runtime = runtime.clone();
    let worker = std::thread::spawn(move || {
        let (snapshot, _rx) = worker_runtime.snapshot_subscription();
        let detail = worker_runtime.detail();
        (snapshot, detail)
    });
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while !worker.is_finished() && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let finished = worker.is_finished();
    drop(state);
    assert!(
        finished,
        "snapshot/detail must not wait on the state lock held by interrupt/shutdown"
    );
    let (snapshot, detail) = worker.join().unwrap();
    assert_eq!(snapshot["type"], "session_snapshot");
    assert!(detail.get("goal").is_some());
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_mirror_rides_the_snapshot_watermark() {
    let dir = std::env::temp_dir().join(format!(
        "mink-server-runtime-goal-mirror-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let runtime = SessionRuntime::open(AgentOptions::new(&dir, &dir).with_api_key("synthetic"))
        .await
        .unwrap();
    let goal = mink::runtime::GoalSnapshot {
        version: 1,
        draft_duration_secs: None,
        needs_confirmation: false,
        review_candidate: None,
        review_generation: 0,
        checker_evidence_versions: Vec::new(),
        reviews: Vec::new(),
        checker_passed_candidate: None,
        checker_records: Vec::new(),
        confirmed_after_iteration: 0,
        draft_ready: true,
        id: "goal-mirror".into(),
        revision: 1,
        contract_revision: 1,
        spec: mink::runtime::GoalSpec::new("mirror the goal"),
        status: mink::runtime::GoalStatus::Paused,
        phase: mink::runtime::GoalPhase::Idle,
        reason: None,
        review_failure_reason: None,
        review_failure_stage: None,
        review_stage: None,
        review_requires_full: false,
        review_escalation_reason: None,
        review_report: None,
        continuations: Vec::new(),
        continuation_fingerprint: None,
        continuation_rounds: 0,
        completion_basis: None,
        iterations: vec![],
        report: None,
        report_repair_used: false,
        usage_records: vec![],
        usage: Default::default(),
        pending_effects: vec![],
        failure_fingerprint: None,
        repeated_failures: 0,
        blocked_rounds: 0,
        blocker_rounds: 0,
        blocker_fingerprint: None,
        evidence_versions: vec![],
        acceptance_conversation_seq: 0,
        environment: "test".into(),
        hidden: false,
    };
    publish_goal(
        &runtime.event_tx,
        &runtime.stream_sequence,
        &runtime.projection,
        Some(goal.clone()),
    );
    let (snapshot, _rx) = runtime.snapshot_subscription();
    let watermark = runtime.projection.lock().unwrap().watermark;
    assert_eq!(snapshot["stream_sequence"], watermark);
    assert_eq!(snapshot["goal"]["id"], "goal-mirror");
    assert_eq!(runtime.detail()["goal"]["id"], "goal-mirror");
    // A cleared goal is normalized to null in the mirror, the snapshot and
    // the published event alike.
    let mut events = runtime.event_tx.subscribe();
    let mut hidden = goal.clone();
    hidden.hidden = true;
    publish_goal(
        &runtime.event_tx,
        &runtime.stream_sequence,
        &runtime.projection,
        Some(hidden),
    );
    let mut goal_event = None;
    while let Ok(message) = events.try_recv() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.payload)
            && value["type"] == "goal_updated"
        {
            goal_event = Some(value);
        }
    }
    let goal_event = goal_event.expect("goal_updated event");
    assert!(
        goal_event["goal"].is_null(),
        "a cleared goal leaked into the event: {goal_event}"
    );
    let (snapshot, _rx) = runtime.snapshot_subscription();
    assert!(snapshot["goal"].is_null());
    assert!(runtime.detail()["goal"].is_null());
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The audit's second direction: interrupt takes state and then waits for
/// the projection, so a snapshot must be able to finish under the same
/// contention without ever waiting for state. Both operations are bounded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_and_interrupt_finish_under_projection_contention() {
    let dir = std::env::temp_dir().join(format!(
        "mink-server-runtime-contention-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let runtime = Arc::new(
        SessionRuntime::open(AgentOptions::new(&dir, &dir).with_api_key("synthetic"))
            .await
            .unwrap(),
    );
    // The interrupt path must actually publish, so mark the session as
    // running before holding the projection it will wait for. No state
    // lock may be taken while the projection is held, or this test would
    // recreate the inversion it guards against.
    runtime.state.lock().unwrap().phase = RuntimePhase::Running;
    let projection = runtime.projection.lock().unwrap();
    let snapshot_runtime = runtime.clone();
    let snapshot = std::thread::spawn(move || snapshot_runtime.snapshot_subscription().0);
    let interrupt_runtime = runtime.clone();
    let interrupt = std::thread::spawn(move || interrupt_runtime.interrupt());
    // Both are serialised behind this release; the old snapshot path would
    // deadlock here, holding the projection while waiting for the state
    // lock interrupt took first.
    std::thread::sleep(std::time::Duration::from_millis(50));
    drop(projection);
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while !(snapshot.is_finished() && interrupt.is_finished()) && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        snapshot.is_finished(),
        "snapshot must finish with the interrupt"
    );
    assert!(
        interrupt.is_finished(),
        "interrupt must finish with the snapshot"
    );
    let snapshot = snapshot.join().unwrap();
    interrupt.join().unwrap();
    assert_eq!(snapshot["type"], "session_snapshot");
    assert_eq!(runtime.phase(), RuntimePhase::Cancelling);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resource_projection_applies_todo_deltas_without_losing_completed_items() {
    let mut resources = serde_json::json!({"plan":{"plan":null,"draft":null},"todo":{"revision":1,"items":[{"id":"done","content":"old","status":"completed"},{"id":"work","content":"new","status":"in_progress"}]}});
    apply_presentation(
        &mut resources,
        &serde_json::json!({"kind":"todo","data":{"revision":2,"items":[],"changes":[{"change":"completed","id":"work"}]}}),
    );
    assert_eq!(resources["todo"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(resources["todo"]["items"][1]["status"], "completed");
    apply_presentation(
        &mut resources,
        &serde_json::json!({"kind":"todo","data":{"revision":1,"items":[],"changes":[{"change":"removed","id":"done"}]}}),
    );
    assert_eq!(resources["todo"]["items"].as_array().unwrap().len(), 2);
    apply_presentation(
        &mut resources,
        &serde_json::json!({"kind":"plan","data":{"transition":"confirmed","content":"plan"}}),
    );
    assert_eq!(resources["plan"]["plan"], "plan");
    assert!(resources["plan"]["draft"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_publish_and_publish_goal_preserve_monotonic_stream_sequence() {
    let dir = std::env::temp_dir().join(format!(
        "mink-server-runtime-seq-order-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let runtime = Arc::new(
        SessionRuntime::open(AgentOptions::new(&dir, &dir).with_api_key("synthetic"))
            .await
            .unwrap(),
    );
    let base_goal = runtime
        .goal_handle()
        .unwrap()
        .goals()
        .draft(
            mink::runtime::GoalSpec::new("ordered publish regression"),
            60,
        )
        .unwrap();
    let (tx, mut rx) = broadcast::channel(1024);

    let mut handles = Vec::new();
    for worker in 0..4u64 {
        let rt = runtime.clone();
        let tx = tx.clone();
        let template = base_goal.clone();
        handles.push(std::thread::spawn(move || {
            for iter in 0..100u64 {
                if (worker + iter) % 2 == 0 {
                    let mut goal = template.clone();
                    goal.revision = iter + 1;
                    publish_goal(&tx, &rt.stream_sequence, &rt.projection, Some(goal));
                } else {
                    publish(
                        &tx,
                        &rt.stream_sequence,
                        &rt.projection,
                        serde_json::json!({
                            "type": "phase_updated",
                            "phase": if iter % 4 == 1 { "running" } else { "idle" }
                        }),
                    );
                }
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }

    let mut prev: Option<u64> = None;
    let mut count = 0usize;
    while let Ok(event) = rx.try_recv() {
        let parsed: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
        let seq = parsed["stream_sequence"].as_u64().unwrap();
        if let Some(prev_seq) = prev {
            assert!(
                seq > prev_seq,
                "broadcast sequence inverted or duplicated: prev={prev_seq}, seq={seq}, event={parsed}"
            );
        }
        prev = Some(seq);
        count += 1;
    }
    assert_eq!(count, 400);
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
