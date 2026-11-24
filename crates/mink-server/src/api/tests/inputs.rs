use super::*;

#[tokio::test]
pub(super) async fn draft_id_is_idempotent_and_first_prompt_names_the_session() {
    let (app, home, cwd) = creation_router();
    let body = serde_json::json!({
        "cwd": cwd.display().to_string(),
        "draft_id": "draft-idem",
        "first_prompt": "  fix the reconnect path\nmore detail",
    });
    let (status, first) = post_json(&app, "/api/sessions", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = post_json(&app, "/api/sessions", body).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(first["data"]["id"], second["data"]["id"]);
    assert_eq!(first["data"]["title"], "fix the reconnect path", "{first}");
    assert_eq!(
        first["data"]["first_prompt"],
        "fix the reconnect path\nmore detail"
    );
    assert_eq!(first["data"]["untitled"], false, "{first}");
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn sessions_without_a_draft_or_name_are_new_and_untitled() {
    let (app, home, cwd) = creation_router();
    let body = serde_json::json!({ "cwd": cwd.display().to_string() });
    let (status, first) = post_json(&app, "/api/sessions", body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = post_json(&app, "/api/sessions", body).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_ne!(first["data"]["id"], second["data"]["id"]);
    assert_eq!(first["data"]["title"], serde_json::Value::Null, "{first}");
    assert_eq!(first["data"]["untitled"], true, "{first}");
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn turn_input_validation() {
    let app = test_router();
    // 空 body → axum JSON 提取器拒绝（415）
    let (s, _) = req(&app, Method::POST, "/api/sessions/test-session/turn").await;
    assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // 带 JSON body 但 input 为空 → 400
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/sessions/test-session/turn")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"input":""}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
pub(super) async fn turn_request_id_retry_does_not_start_a_second_turn() {
    let home = std::env::temp_dir().join(format!(
        "mink-server-turn-idem-{}-{}",
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
    let created = registry.create("turn-idem", &cwd).await.unwrap();
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

    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!(
                        "/api/sessions/{}/turn?project={project}",
                        created.id
                    ))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"input":"hello once","request_id":"stable-turn-1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // Reusing the id with different content is a conflict, not an internal
    // error and not a second turn.
    let conflict = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/sessions/{}/turn?project={project}",
                    created.id
                ))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"input":"different words","request_id":"stable-turn-1"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);

    // The mock turn finishes quickly; the retry must have reused the first
    // receipt instead of appending the same user input twice.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let dir = home
        .join(".mink")
        .join("projects")
        .join(&project)
        .join(&created.id);
    let conversation = std::fs::read_to_string(dir.join("conversation.jsonl")).unwrap();
    assert_eq!(
        conversation.matches("hello once").count(),
        1,
        "{conversation}"
    );
    registry.close(&created.id, Some(&project)).await.unwrap();
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn input_receipts_snapshot_and_legacy_history_are_consistent() {
    let home = std::env::temp_dir().join(format!(
        "mink-input-api-{}-{}",
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
    let created = registry.create("inputs", &cwd).await.unwrap();
    let base = format!("/api/sessions/{}", created.id);
    let project = created.project_key;
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let app = router(Arc::new(ApiState {
        registry: registry.clone(),
        cwd,
        shutdown,
    }));
    assert_eq!(
        req(
            &app,
            Method::POST,
            &format!("{base}/open?project={project}")
        )
        .await
        .0,
        StatusCode::OK
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{base}/stream?project={project}&snapshot=true"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut stream = response.into_body();
    let initial = tokio::time::timeout(std::time::Duration::from_secs(3), stream.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    let initial = String::from_utf8(initial.to_vec()).unwrap();
    let snapshot: serde_json::Value =
        serde_json::from_str(initial.trim().strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(snapshot["type"], "session_snapshot");
    assert!(snapshot["resources"]["todo"]["items"].is_array());
    assert!(snapshot["generation"].is_string());
    let payload =
        r#"{"request_id":"stable","text":"task","target_turn_id":null,"attachment_ids":[]}"#;
    let submit = |payload: &str| {
        Request::builder()
            .method(Method::POST)
            .uri(format!("{base}/inputs?project={project}"))
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap()
    };
    assert_eq!(
        app.clone().oneshot(submit(payload)).await.unwrap().status(),
        StatusCode::OK
    );
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while let Some(frame) = stream.frame().await {
            let data = frame.unwrap().into_data().unwrap();
            let text = String::from_utf8(data.to_vec()).unwrap();
            let Some(json) = text.trim().strip_prefix("data: ") else {
                continue;
            };
            let event: serde_json::Value = serde_json::from_str(json).unwrap();
            assert_eq!(event["generation"], snapshot["generation"]);
            if event["type"] == "turn_final" {
                break;
            }
        }
    })
    .await
    .unwrap();
    let (_, receipt) = req(
        &app,
        Method::GET,
        &format!("{base}/inputs?project={project}&request_id=stable"),
    )
    .await;
    assert_eq!(receipt["data"][0]["status"], "applied");
    assert_eq!(
        app.clone().oneshot(submit(payload)).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone()
            .oneshot(submit(&payload.replace("task", "different")))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let (_, outstanding) = req(
        &app,
        Method::GET,
        &format!("{base}/inputs?project={project}"),
    )
    .await;
    assert_eq!(outstanding["data"], json!([]));
    let (_, history) = req(
        &app,
        Method::GET,
        &format!("{base}/conversation?project={project}&turns=true&limit=1"),
    )
    .await;
    assert_eq!(history["data"][0]["_mink"]["input_id"], "stable");
    assert_eq!(history["data"].as_array().unwrap().len(), 2);
    drop(stream);
    registry.shutdown_all().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn host_and_origin_middleware_blocks_rebinding_and_csrf() {
    let app = test_router();

    // Loopback Host allowed + security headers + no-store + API 404 JSON
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/version")
                .header("host", "127.0.0.1:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(res.headers().get("referrer-policy").unwrap(), "no-referrer");
    assert_eq!(res.headers().get("x-frame-options").unwrap(), "DENY");
    assert_eq!(res.headers().get("cache-control").unwrap(), "no-store");

    let api_404 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/does-not-exist")
                .header("host", "localhost:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(api_404.status(), StatusCode::NOT_FOUND);

    // DNS rebinding Host rejected
    let rebind = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/version")
                .header("host", "evil.example:8765")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rebind.status(), StatusCode::FORBIDDEN);

    // Cross-origin POST rejected
    let csrf_origin = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/sessions/test-session/open")
                .header("host", "127.0.0.1:8765")
                .header("origin", "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(csrf_origin.status(), StatusCode::FORBIDDEN);

    // Sec-Fetch-Site: cross-site rejected on mutations
    let csrf_fetch_site = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/sessions/test-session/compact")
                .header("host", "127.0.0.1:8765")
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(csrf_fetch_site.status(), StatusCode::FORBIDDEN);
}

#[cfg(unix)]
#[tokio::test]
pub(super) async fn artifact_and_files_reject_symlinks_and_oversized_inputs() {
    let (app, sess_dir) = test_router_with_session_dir();
    let outside = sess_dir.parent().unwrap().join("secret.txt");
    std::fs::write(&outside, "top secret").unwrap();
    std::os::unix::fs::symlink(&outside, sess_dir.join("artifacts/leak.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, sess_dir.join("leak_in_cwd.txt")).unwrap();

    let (status, _) = req(
        &app,
        Method::GET,
        "/api/sessions/test-session/artifacts/leak",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = req(
        &app,
        Method::GET,
        "/api/sessions/test-session/files?path=leak_in_cwd.txt&raw=true",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = req(
        &app,
        Method::GET,
        "/api/sessions/test-session/artifacts/.hidden",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Oversized input payload rejected with 400 after opening
    let home = std::env::temp_dir().join(format!("mink-open-limit-{}", std::process::id()));
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let reg = Arc::new(Registry::with_llm_backend(
        home.clone(),
        "flash".to_string(),
        4,
        Arc::new(MockRuntimeBackend),
    ));
    let created = reg.create("open-limit", &cwd).await.unwrap();
    let project = created.project_key.clone();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let open_app = router(Arc::new(ApiState {
        registry: reg.clone(),
        cwd,
        shutdown,
    }));
    let (open_status, open_body) = req(
        &open_app,
        Method::POST,
        &format!("/api/sessions/{}/open?project={project}", created.id),
    )
    .await;
    assert_eq!(open_status, StatusCode::OK, "{open_body}");
    let huge_text = "x".repeat(128 * 1024 + 1);
    let huge_res = open_app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/sessions/{}/inputs?project={project}",
                    created.id
                ))
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "request_id": "req-huge",
                        "text": huge_text,
                        "attachment_ids": [],
                        "target_turn_id": null,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(huge_res.status(), StatusCode::BAD_REQUEST);
    reg.shutdown_all().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_file(outside);
}
