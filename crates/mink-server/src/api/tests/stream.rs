use super::*;

/// The workbench reaches `/model` and `/compact` through the same session
/// runtime the terminal drives, so both must be host endpoints rather than
/// client-only behaviour.
#[tokio::test]
pub(super) async fn server_switches_model_and_compacts_through_the_runtime() {
    let (app, home, sid) = open_goal_router().await;
    let g = |suffix: &str| format!("/api/sessions/{sid}{suffix}");

    // A blank model is a host error, not a silent no-op.
    let (status, body) = post_json(&app, &g("/model"), serde_json::json!({ "model": "   " })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = post_json(&app, &g("/model"), serde_json::json!({ "model": "pro" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["model"], "pro", "{body}");
    assert!(
        body["data"]["label"].is_string(),
        "the switch must resolve an alias to a real label: {body}"
    );

    let (status, body) = post_json(&app, &g("/compact"), serde_json::json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["compacted"], true, "{body}");

    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn closed_runtime_is_reapable_and_leaves_the_active_table() {
    let home = std::env::temp_dir().join(format!(
        "mink-server-reap-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Arc::new(Registry::with_llm_backend(
        home.clone(),
        "mock".into(),
        1,
        Arc::new(MockRuntimeBackend),
    ));
    let created = registry.create("reap", &cwd).await.unwrap();
    let project = created.project_key.clone();
    registry.open(&created.id, Some(&project)).await.unwrap();

    let runtime = registry
        .active_runtime(&created.id, Some(&project))
        .unwrap()
        .expect("opened runtime");
    runtime.shutdown().await.unwrap();
    assert!(
        registry
            .closed_session_ids()
            .iter()
            .any(|(id, _)| id == &created.id),
        "a forced/closed runtime must be reaper-visible"
    );

    // The reaper's action removes the entry (and its broadcast sender).
    registry.close(&created.id, Some(&project)).await.unwrap();
    assert!(
        !registry
            .closed_session_ids()
            .iter()
            .any(|(id, _)| id == &created.id)
    );
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn closing_a_session_emits_closed_to_its_sse_stream() {
    let home = std::env::temp_dir().join(format!(
        "mink-server-sse-close-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Arc::new(Registry::with_llm_backend(
        home.clone(),
        "mock".into(),
        1,
        Arc::new(MockRuntimeBackend),
    ));
    let created = registry.create("sse-close", &cwd).await.unwrap();
    let project = created.project_key.clone();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let state = Arc::new(ApiState {
        registry: registry.clone(),
        cwd,
        shutdown,
    });
    let app = router(state);
    let (status, _) = req(
        &app,
        Method::POST,
        &format!("/api/sessions/{}/open?project={project}", created.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let stream_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/sessions/{}/stream?project={project}&snapshot=true",
                    created.id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stream_response.status(), StatusCode::OK);
    let mut body = stream_response.into_body();

    // The reaper's action: close removes the active entry and shutdown
    // publishes the terminal phase to every subscribed stream.
    registry.close(&created.id, Some(&project)).await.unwrap();
    let saw_closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(frame) = body.frame().await {
            let Ok(data) = frame.unwrap().into_data() else {
                continue;
            };
            for line in String::from_utf8_lossy(&data).lines() {
                let Some(json) = line.strip_prefix("data: ") else {
                    continue;
                };
                let Ok(event) = serde_json::from_str::<serde_json::Value>(json) else {
                    continue;
                };
                if event["type"] == "phase_updated" && event["phase"] == "closed" {
                    return true;
                }
            }
        }
        false
    })
    .await
    .expect("the stream must either deliver closed or end");
    assert!(saw_closed, "close must publish phase_updated closed");
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn mock_runtime_api_streams_authoritative_sse_envelope() {
    let home = std::env::temp_dir().join(format!(
        "mink-server-mock-sse-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Arc::new(Registry::with_llm_backend(
        home.clone(),
        "mock".into(),
        1,
        Arc::new(MockRuntimeBackend),
    ));
    let created = registry.create("mock-sse", &cwd).await.unwrap();
    let project = created.project_key.clone();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let state = Arc::new(ApiState {
        registry: registry.clone(),
        cwd,
        shutdown,
    });
    let app = router(state);

    let (status, _) = req(
        &app,
        Method::POST,
        &format!("/api/sessions/{}/open?project={project}", created.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let stream_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/sessions/{}/stream?project={project}",
                    created.id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stream_response.status(), StatusCode::OK);
    let mut body = stream_response.into_body();

    let turn_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/sessions/{}/turn?project={project}",
                    created.id
                ))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"input":"hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(turn_response.status(), StatusCode::OK);

    let mut types = Vec::new();
    let mut saw_stream_sequence = false;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while let Some(frame) = body.frame().await {
            let frame = frame.unwrap();
            let Ok(data) = frame.into_data() else {
                continue;
            };
            for line in String::from_utf8_lossy(&data).lines() {
                let Some(json) = line.strip_prefix("data: ") else {
                    continue;
                };
                let event: serde_json::Value = serde_json::from_str(json).unwrap();
                saw_stream_sequence |= event["stream_sequence"].is_number();
                if let Some(kind) = event["type"].as_str() {
                    types.push(kind.to_string());
                    if kind == "turn_final" {
                        return;
                    }
                }
            }
        }
    })
    .await
    .expect("mock runtime SSE did not reach turn_final");

    assert!(saw_stream_sequence);
    assert!(types.starts_with(&["turn_started".into()]));
    assert!(types.contains(&"text".into()));
    assert!(types.contains(&"stop".into()));
    assert_eq!(types.last().map(String::as_str), Some("turn_final"));
    registry.close(&created.id, Some(&project)).await.unwrap();
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn sse_stream_ends_when_shutdown_signal_is_sent() {
    let home = std::env::temp_dir().join(format!(
        "mink-server-shutdown-sse-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Arc::new(Registry::with_llm_backend(
        home.clone(),
        "mock".into(),
        1,
        Arc::new(MockRuntimeBackend),
    ));
    let created = registry.create("shutdown-sse", &cwd).await.unwrap();
    let project = created.project_key.clone();
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let state = Arc::new(ApiState {
        registry: registry.clone(),
        cwd,
        shutdown: shutdown_tx.clone(),
    });
    let app = router(state);

    let (status, _) = req(
        &app,
        Method::POST,
        &format!("/api/sessions/{}/open?project={project}", created.id),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let stream_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/sessions/{}/stream?project={project}",
                    created.id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stream_response.status(), StatusCode::OK);
    let mut body = stream_response.into_body();

    let _ = shutdown_tx.send(true);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while let Some(frame) = body.frame().await {
            let _ = frame.unwrap();
        }
    })
    .await
    .expect("SSE stream should close after the shutdown signal is sent");

    registry.close(&created.id, Some(&project)).await.unwrap();
    let _ = std::fs::remove_dir_all(home);
}
