use super::super::controls::control_error;
use super::*;

#[test]
pub(super) fn control_error_uses_runtime_error_variants() {
    assert_eq!(
        control_error(&mink::runtime::RuntimeError::ExternallyControlled { owner: "goal" }),
        "当前目标执行中，先暂停目标再操作"
    );
    assert_eq!(
        control_error(&mink::runtime::RuntimeError::Busy {
            active_turn_id: mink::runtime::TurnId::new("turn-1"),
        }),
        "本轮尚未结束，请稍后重试"
    );
}

#[tokio::test]
pub(super) async fn workspace_check_accepts_only_existing_directories() {
    let (app, home, cwd) = creation_router();
    let uri = format!(
        "/api/workspace?path={}",
        urlencoding_simple(&cwd.display().to_string())
    );
    let (status, body) = req(&app, Method::GET, &uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["name"], "workspace");
    assert!(
        body["data"]["project_key"]
            .as_str()
            .is_some_and(|key| !key.is_empty())
    );

    let missing = home.join("missing");
    let uri = format!(
        "/api/workspace?path={}",
        urlencoding_simple(&missing.display().to_string())
    );
    let (status, body) = req(&app, Method::GET, &uri).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn first_prompt_is_bounded_before_creation_and_in_the_session_list() {
    let (app, home, cwd) = creation_router();
    let too_long = "x".repeat(128 * 1024 + 1);
    let (status, body) = post_json(
        &app,
        "/api/sessions",
        serde_json::json!({
            "cwd": cwd.display().to_string(),
            "draft_id": "draft-too-long",
            "first_prompt": too_long,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (_, listed) = req(&app, Method::GET, "/api/sessions").await;
    assert!(listed["data"].as_array().unwrap().is_empty(), "{listed}");

    let (status, created) = post_json(
        &app,
        "/api/sessions",
        serde_json::json!({
            "cwd": cwd.display().to_string(),
            "draft_id": "draft-long-list",
            "first_prompt": "y".repeat(600),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let (_, listed) = req(&app, Method::GET, "/api/sessions").await;
    let prompt = listed["data"][0]["first_prompt"].as_str().unwrap();
    assert!(prompt.ends_with('…'), "{prompt}");
    assert!(prompt.len() <= 512 + '…'.len_utf8(), "{prompt}");
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
pub(super) async fn conversation_pagination_and_seq() {
    let app = test_router();
    // tail：返回最后 2 行，注入行号 seq
    let (s, body) = req(
        &app,
        Method::GET,
        "/api/sessions/test-session/conversation?limit=2&tail=true",
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let rows = body["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["seq"], serde_json::json!(2));
    assert_eq!(rows[1]["content"], serde_json::json!("three"));
    // before_seq：取 seq 之前的行
    let (_, body) = req(
        &app,
        Method::GET,
        "/api/sessions/test-session/conversation?limit=1&before_seq=3",
    )
    .await;
    let rows = body["data"].as_array().unwrap();
    assert_eq!(rows[0]["seq"], serde_json::json!(2));
    // 不存在的会话 → 404
    let (s, _) = req(&app, Method::GET, "/api/sessions/nope/conversation").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
pub(super) async fn files_path_escape_rejected() {
    let app = test_router();
    let (s, body) = req(
        &app,
        Method::GET,
        "/api/sessions/test-session/files?path=../../../etc",
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(body["message"].as_str().unwrap_or("").contains("escape"));
}

#[tokio::test]
pub(super) async fn artifact_name_filtered() {
    let app = test_router();
    let (s, _body) = req(
        &app,
        Method::GET,
        "/api/sessions/test-session/artifacts/..%2F..%2Fpasswd",
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // 正常 artifact 可读
    let (s, body) = req(
        &app,
        Method::GET,
        "/api/sessions/test-session/artifacts/abc",
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["data"]["content"], serde_json::json!("artifact body"));
}

#[tokio::test]
pub(super) async fn health_and_plan() {
    let app = test_router();
    let (s, body) = req(&app, Method::GET, "/health").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["data"]["status"], serde_json::json!("ok"));
    let (s, body) = req(&app, Method::GET, "/api/sessions/test-session/plan").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["data"]["plan"], serde_json::Value::Null);
}

/// A resource file that exists but cannot be read is an error, not an empty
/// state: the workbench must be able to tell "nothing here" from
/// "unreadable", or a broken session looks like an empty one.
#[tokio::test]
pub(super) async fn unreadable_resources_are_not_reported_as_absent() {
    let (app, dir) = test_router_with_session_dir();
    let uri = |suffix: &str| format!("/api/sessions/test-session{suffix}");

    // Missing files keep their existing absent representation.
    let (status, body) = req(&app, Method::GET, &uri("/plan")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"]["plan"].is_null(), "{body}");
    assert!(body["data"]["draft"].is_null(), "{body}");
    let (status, body) = req(&app, Method::GET, &uri("/todo")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"]["todos"].is_null(), "{body}");
    let (status, body) = req(&app, Method::GET, &uri("/goal")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["data"]["goal"].is_null(), "{body}");
    let (status, body) = req(&app, Method::GET, &uri("/artifacts")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["artifacts"], serde_json::json!([]), "{body}");

    // A permission refusal is a read failure. A privileged test runner can
    // still read the file, so the assertion is skipped there instead of
    // passing on a false premise.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let draft = dir.join("plan.draft");
        std::fs::write(&draft, "draft body").unwrap();
        std::fs::set_permissions(&draft, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_to_string(&draft).is_err() {
            let (status, body) = req(&app, Method::GET, &uri("/plan")).await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
            assert!(
                body["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("plan.draft"),
                "the refusal must name the resource: {body}"
            );
        } else {
            eprintln!("skipping permission assertion: the test runner can read mode 000");
        }
        std::fs::set_permissions(&draft, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(&draft).unwrap();
    }

    // A directory where a file belongs is an I/O error, not "no plan".
    std::fs::create_dir_all(dir.join("plan.md")).unwrap();
    let (status, body) = req(&app, Method::GET, &uri("/plan")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        body["message"].as_str().unwrap_or("").contains("plan.md"),
        "{body}"
    );

    // Invalid UTF-8 is a read/decoding failure, not an empty resource.
    std::fs::write(dir.join("todos.json"), [0xff, 0xfe, 0xfd]).unwrap();
    let (status, body) = req(&app, Method::GET, &uri("/todo")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("todos.json"),
        "{body}"
    );
    std::fs::write(dir.join("goal.json"), [0xff]).unwrap();
    let (status, body) = req(&app, Method::GET, &uri("/goal")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        body["message"].as_str().unwrap_or("").contains("goal.json"),
        "{body}"
    );

    // A missing artifact stays 404; an unreadable one does not.
    let (status, body) = req(&app, Method::GET, &uri("/artifacts/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    std::fs::create_dir_all(dir.join("artifacts/broken.txt")).unwrap();
    let (status, body) = req(&app, Method::GET, &uri("/artifacts/broken")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        body["message"].as_str().unwrap_or("").contains("broken"),
        "{body}"
    );
    // The read ceiling is unchanged for a readable but oversized artifact.
    std::fs::write(
        dir.join("artifacts/big.txt"),
        vec![b'x'; FILE_RAW_MAX_BYTES as usize + 1],
    )
    .unwrap();
    let (status, body) = req(&app, Method::GET, &uri("/artifacts/big")).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");

    // A corrupted artifact index is still tolerated line by line (the
    // documented index format), but an unreadable index is an error.
    std::fs::write(
        dir.join("artifacts/index.jsonl"),
        "{\"id\":\"abc\"}\nnot-json\n",
    )
    .unwrap();
    let (status, body) = req(&app, Method::GET, &uri("/artifacts")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["artifacts"].as_array().unwrap().len(),
        1,
        "{body}"
    );
}
