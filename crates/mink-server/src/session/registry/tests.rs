use super::*;

#[test]
fn lease_errors_are_classified_by_io_kind() {
    let locked = RegistryError::from_lease(
        anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::WouldBlock))
            .context("session is locked by another process"),
    );
    assert!(matches!(locked, RegistryError::Locked(_)));

    let failed = RegistryError::from_lease(
        anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("session is locked by another process"),
    );
    assert!(matches!(failed, RegistryError::Internal(_)));
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mink-server-{label}-{}-{nonce}",
        std::process::id()
    ))
}

#[tokio::test]
async fn backfill_title_keeps_image_only_untitled_and_never_resurrects_a_deletion() {
    let dir = unique_temp_dir("backfill");
    let cwd = dir.join("workspace");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Registry::new(dir.clone(), "flash".into(), 4);
    let created = registry
        .create_draft("draft-backfill", &cwd, None)
        .await
        .unwrap();
    let session_dir = PathBuf::from(&created.path);
    let read = || {
        serde_json::from_str::<SessionMetadata>(
            &std::fs::read_to_string(session_dir.join("session.json")).unwrap(),
        )
        .unwrap()
    };

    // Image-only input writes no placeholder prompt: a later text turn can
    // still become the title.
    assert!(
        !registry
            .backfill_title(&created.id, Some(&created.project_key), "   ")
            .await
            .unwrap()
    );
    assert_eq!(read().first_prompt, None);
    assert!(read().untitled);

    assert!(
        registry
            .backfill_title(
                &created.id,
                Some(&created.project_key),
                "fix the reconnect path"
            )
            .await
            .unwrap()
    );
    let metadata = read();
    assert_eq!(
        metadata.first_prompt.as_deref(),
        Some("fix the reconnect path")
    );
    assert_eq!(metadata.title.as_deref(), Some("fix the reconnect path"));
    assert!(!metadata.untitled);

    registry
        .delete(&created.id, Some(&created.project_key))
        .await
        .unwrap();
    assert!(!session_dir.exists());
    assert!(
        registry
            .backfill_title(&created.id, Some(&created.project_key), "late text")
            .await
            .is_err()
    );
    assert!(
        !session_dir.exists(),
        "a late write must not recreate the session"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn poisoned_state_lock_fails_closed() {
    let dir = unique_temp_dir("poison");
    std::fs::create_dir_all(&dir).unwrap();
    let registry = Registry::new(dir.clone(), "flash".into(), 4);

    // Simulate a panic while the state lock was held.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = registry.active.lock().expect("state lock");
        panic!("inject poison");
    }));

    let error = match registry.lock_active_state() {
        Ok(_) => panic!("poisoned state lock must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("poisoned"), "{error}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn lease_is_exclusive_reusable_and_ignores_stale_file_contents() {
    let dir = unique_temp_dir("lease");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(LOCK_FILE);
    std::fs::write(&path, "").unwrap();
    let lease = SessionLease::acquire(path.clone()).unwrap();
    assert!(SessionLease::acquire(path.clone()).is_err());
    drop(lease);
    SessionLease::acquire(path.clone()).unwrap();
    assert!(path.is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_open_publishes_one_runtime_and_one_lease() {
    let root = unique_temp_dir("concurrent-open");
    let home = root.join("home");
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Arc::new(Registry::new(home, "flash".to_string(), 4));
    let created = registry.create("race", &cwd).await.unwrap();

    let mut tasks = Vec::new();
    for _ in 0..32 {
        let registry = registry.clone();
        let id = created.id.clone();
        let project = created.project_key.clone();
        tasks.push(tokio::spawn(async move {
            registry.open(&id, Some(&project)).await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }

    assert_eq!(registry.active.lock().unwrap().len(), 1);
    let session_dir = registry
        .session_dir(&created.id, Some(&created.project_key))
        .await
        .unwrap();
    assert!(session_dir.join(LOCK_FILE).is_file());

    registry
        .close(&created.id, Some(&created.project_key))
        .await
        .unwrap();
    assert!(session_dir.join(LOCK_FILE).exists());
    SessionLease::acquire(session_dir.join(LOCK_FILE)).unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_reuses_one_project_alias() {
    let root = unique_temp_dir("concurrent-create");
    let home = root.join("home");
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Arc::new(Registry::new(home, "flash".to_string(), 4));

    let mut tasks = Vec::new();
    for _ in 0..32 {
        let registry = registry.clone();
        let cwd = cwd.clone();
        tasks.push(tokio::spawn(async move {
            registry.create("same alias", &cwd).await
        }));
    }
    let mut ids = std::collections::BTreeSet::new();
    for task in tasks {
        ids.insert(task.await.unwrap().unwrap().id);
    }

    assert_eq!(ids.len(), 1);
    assert_eq!(registry.list().await.unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

/// The pool is the single owner of goal capacity: a permit occupies the
/// slot until it is dropped, a competitor is refused, and the same session
/// cannot hold two permits at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_run_permits_are_exclusive_and_release_on_drop() {
    let root = unique_temp_dir("goal-reservation");
    let home = root.join("home");
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let registry = Registry::new(home, "flash".to_string(), 1);
    let first = registry.create("first", &cwd).await.unwrap();
    let second = registry.create("second", &cwd).await.unwrap();
    registry
        .open(&first.id, Some(&first.project_key))
        .await
        .unwrap();
    registry
        .open(&second.id, Some(&second.project_key))
        .await
        .unwrap();

    // A creation attempt takes the single slot; a competitor is refused.
    let attempt = registry
        .reserve_goal_run(&first.id, Some(&first.project_key))
        .await
        .unwrap();
    assert!(matches!(
        registry
            .reserve_goal_run(&second.id, Some(&second.project_key))
            .await,
        Err(RegistryError::Capacity(_))
    ));
    // The same session cannot hold a second permit either.
    assert!(matches!(
        registry
            .reserve_goal_run(&first.id, Some(&first.project_key))
            .await,
        Err(RegistryError::Busy(_))
    ));
    // Dropping the failed attempt's lease frees exactly its own slot.
    drop(attempt);
    let claimed = registry
        .reserve_goal_run(&second.id, Some(&second.project_key))
        .await
        .unwrap();
    drop(claimed);

    // A stale permit from a closed instance must not release the reopened
    // instance's slot.
    let stale = registry
        .reserve_goal_run(&first.id, Some(&first.project_key))
        .await
        .unwrap();
    registry
        .close(&first.id, Some(&first.project_key))
        .await
        .unwrap();
    drop(stale);
    registry
        .open(&first.id, Some(&first.project_key))
        .await
        .unwrap();
    let reopened = registry
        .reserve_goal_run(&first.id, Some(&first.project_key))
        .await
        .unwrap();
    assert!(matches!(
        registry
            .reserve_goal_run(&second.id, Some(&second.project_key))
            .await,
        Err(RegistryError::Capacity(_))
    ));
    drop(reopened);
    let freed = registry
        .reserve_goal_run(&second.id, Some(&second.project_key))
        .await
        .unwrap();
    drop(freed);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn delete_respects_a_lease_owned_by_another_registry() {
    let root = unique_temp_dir("cross-registry-delete");
    let home = root.join("home");
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let owner = Registry::new(home.clone(), "flash".to_string(), 4);
    let deleter = Registry::new(home, "flash".to_string(), 4);
    let created = owner.create("leased", &cwd).await.unwrap();
    owner
        .open(&created.id, Some(&created.project_key))
        .await
        .unwrap();

    // The second Registry has an empty local active map, so this assertion
    // specifically verifies the OS-backed lease rather than the in-memory
    // operation lock. A locked delete must leave every session file intact.
    let error = deleter
        .delete(&created.id, Some(&created.project_key))
        .await
        .unwrap_err();
    assert!(matches!(error, RegistryError::Locked(_)));
    let session_dir = owner
        .session_dir(&created.id, Some(&created.project_key))
        .await
        .unwrap();
    assert!(session_dir.join("session.json").is_file());

    // Once the owner shuts down and drops its lease, the same independent
    // Registry can acquire the deletion lease and remove the directory.
    owner
        .close(&created.id, Some(&created.project_key))
        .await
        .unwrap();
    deleter
        .delete(&created.id, Some(&created.project_key))
        .await
        .unwrap();
    assert!(!session_dir.exists());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn catalog_and_resolution_exclude_child_sessions_by_directory_and_metadata() {
    let root = unique_temp_dir("child-catalog");
    let home = root.join("home");
    let project = home.join(".mink/projects/project-key");
    let fixtures = [
        ("user", Some("user"), None),
        ("sub_missing", None, None),
        ("sub_corrupt", None, None),
        ("sub_inherited", Some("inherited-id"), None),
        ("replan_legacy", Some("recovery-id"), None),
        ("renamed-sub", Some("sub_metadata"), None),
        ("renamed-replan", Some("replan_metadata"), None),
        ("custom-child", Some("custom-child"), Some("user")),
        ("broken-user", None, None),
    ];
    for (directory, id, parent) in fixtures {
        let dir = project.join(directory);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(id) = id {
            let metadata = SessionMetadata {
                id: id.into(),
                parent: parent.map(str::to_owned),
                // A user alias is not an internal session identity.
                alias: Some("sub_user_alias".into()),
                ..Default::default()
            };
            std::fs::write(
                dir.join("session.json"),
                serde_json::to_vec(&metadata).unwrap(),
            )
            .unwrap();
        } else if directory.contains("corrupt") || directory == "broken-user" {
            std::fs::write(dir.join("session.json"), "{not-json").unwrap();
        }
    }
    std::fs::create_dir_all(project.join("user/subagents/sub_nested")).unwrap();

    let registry = Registry::new(home, "flash".into(), 1);
    let mut ids = registry
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect::<Vec<_>>();
    ids.sort();
    assert_eq!(ids, ["broken-user", "user"]);
    assert_eq!(
        registry
            .session_dir("user", Some("project-key"))
            .await
            .unwrap(),
        project.join("user")
    );
    for (_, id, _) in fixtures.into_iter().skip(1).take(7) {
        if let Some(id) = id {
            assert!(matches!(
                registry.session_dir(id, Some("project-key")).await,
                Err(RegistryError::NotFound(_))
            ));
        }
    }
    assert!(project.join("sub_missing").is_dir());
    assert_eq!(
        std::fs::read_to_string(project.join("sub_corrupt/session.json")).unwrap(),
        "{not-json"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn list_marks_malformed_metadata_without_overwriting_it() {
    let root = unique_temp_dir("corrupt-list");
    let home = root.join("home");
    let session_dir = home
        .join(".mink")
        .join("projects")
        .join("project-key")
        .join("broken");
    std::fs::create_dir_all(&session_dir).unwrap();
    let metadata_path = session_dir.join("session.json");
    std::fs::write(&metadata_path, "{not-json").unwrap();

    let registry = Registry::new(home, "flash".to_string(), 1);
    let sessions = registry.list().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "broken");
    assert!(sessions[0].corrupt);
    assert_eq!(std::fs::read_to_string(metadata_path).unwrap(), "{not-json");

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn list_skips_corrupt_usage_jsonl_without_failing_whole_list() {
    let root = unique_temp_dir("corrupt-usage-list");
    let home = root.join("home");
    let session_dir = home
        .join(".mink")
        .join("projects")
        .join("project-key")
        .join("ok");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("usage.jsonl"), "not-json\n").unwrap();

    let registry = Registry::new(home, "flash".to_string(), 1);
    let sessions = registry.list().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "ok");
    assert_eq!(sessions[0].tokens_in, 0);
    assert_eq!(sessions[0].tokens_out, 0);

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn list_degrades_when_usage_path_is_io_corrupt() {
    let root = unique_temp_dir("io-corrupt-usage-list");
    let home = root.join("home");
    let session_dir = home
        .join(".mink")
        .join("projects")
        .join("project-key")
        .join("ok");
    std::fs::create_dir_all(&session_dir).unwrap();
    // usage.jsonl 被替换成同名目录：read_to_string 会报 "Is a directory"。
    std::fs::create_dir_all(session_dir.join("usage.jsonl")).unwrap();

    let registry = Registry::new(home, "flash".to_string(), 1);
    let sessions = registry.list().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "ok");
    assert_eq!(sessions[0].tokens_in, 0);
    assert_eq!(sessions[0].tokens_out, 0);

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn build_options_applies_project_minkrc_and_env_overrides() {
    let _guard = crate::session::TEST_ENV_LOCK.lock().await;
    use crate::session::agent_config;
    use std::sync::{Arc, Mutex};

    struct CapturingBackend {
        seen: Arc<Mutex<Vec<(String, String, String)>>>, // (model, api_key, api_url)
    }

    #[async_trait::async_trait]
    impl mink::runtime::LlmBackend for CapturingBackend {
        fn name(&self) -> &str {
            "server-config-capture"
        }

        async fn stream(
            &self,
            request: mink::runtime::LlmRequest,
        ) -> anyhow::Result<mink::runtime::LlmResponseStream> {
            self.seen.lock().unwrap().push((
                request.model.clone(),
                request.api_key.clone(),
                request.api_url.clone(),
            ));
            Ok(mink::runtime::LlmResponseStream {
                events: Box::pin(futures::stream::iter(vec![
                    Ok(mink::runtime::LlmEvent::Text(mink::runtime::LlmTextEvent {
                        content: "ok".into(),
                    })),
                    Ok(mink::runtime::LlmEvent::Stop(mink::runtime::LlmStopEvent {
                        reason: "end_turn".into(),
                    })),
                ])),
                attempt_count: 1,
            })
        }
    }

    let root = unique_temp_dir("build-options");
    let home = root.join("home");
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    // Project-level .minkrc（与 CLI 相同的分组格式）。
    std::fs::write(
        cwd.join(".minkrc"),
        "[provider]\nmodel = \"project-model\"\napi_key = \"project-key\"\nbase_url = \"https://project.invalid/v1\"\n",
    )
    .unwrap();

    // 保留/恢复真实环境变量，避免测试互相污染。
    let env_keys = ["MODEL", "DEEPSEEK_API_KEY", "DEEPSEEK_BASE_URL"];
    let saved: Vec<Option<String>> = env_keys.iter().map(|k| std::env::var(k).ok()).collect();
    for key in env_keys {
        unsafe { std::env::remove_var(key) };
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let user_layer = agent_config::parse_layer(
        "[provider]\nmodel = \"user-model\"\napi_key = \"user-key\"\n",
        "test",
    )
    .unwrap();
    let mut registry = Registry::new(home.clone(), "server-default".to_string(), 4);
    registry.agent_layer = user_layer;
    registry.llm_backend = Some(Arc::new(CapturingBackend { seen: seen.clone() }));

    let options = registry.build_options("cfg-test", &cwd).unwrap();
    let runtime = mink::runtime::AgentRuntime::start(options).await.unwrap();
    runtime.run_turn("hi").await.unwrap();
    runtime.shutdown().await.unwrap();

    let captured = seen.lock().unwrap().clone();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].0, "project-model",
        "project .minkrc model must override the user layer and server default"
    );
    assert_eq!(captured[0].1, "project-key");
    assert_eq!(
        captured[0].2, "https://project.invalid/v1/chat/completions",
        "runtime appends the chat completions path to base_url"
    );

    // 环境变量覆盖文件层（server 文档化优先级：env 最高）。
    unsafe { std::env::set_var("MODEL", "env-model") };
    unsafe { std::env::set_var("DEEPSEEK_API_KEY", "env-key") };
    let options = registry.build_options("cfg-test", &cwd).unwrap();
    let runtime = mink::runtime::AgentRuntime::start(options).await.unwrap();
    runtime.run_turn("hi").await.unwrap();
    runtime.shutdown().await.unwrap();
    let captured = seen.lock().unwrap().clone();
    assert_eq!(captured[1].0, "env-model");
    assert_eq!(captured[1].1, "env-key");

    for (key, value) in env_keys.iter().zip(saved) {
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn invalid_project_minkrc_prevents_runtime_options_from_being_built() {
    let root = unique_temp_dir("invalid-project-config");
    let home = root.join("home");
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join(".minkrc"),
        "[tools]\nenabled_tools = []\nunknown_key = true\n",
    )
    .unwrap();
    let registry = Registry::new(home, "flash".into(), 1);
    let error = match registry.build_options("config-test", &cwd) {
        Ok(_) => panic!("invalid project config must be rejected"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("unknown_key"));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn build_options_freezes_the_configured_image_capability() {
    let _guard = crate::session::TEST_ENV_LOCK.lock().await;
    // Env outranks the file layers (documented server precedence): start
    // from a clean environment so the project `.minkrc` is what is read.
    let saved_image = std::env::var("MINK_IMAGE_INPUT").ok();
    let saved_models = std::env::var("MINK_VISION_MODELS").ok();
    unsafe { std::env::remove_var("MINK_IMAGE_INPUT") };
    unsafe { std::env::remove_var("MINK_VISION_MODELS") };

    struct NoopBackend;

    #[async_trait::async_trait]
    impl mink::runtime::LlmBackend for NoopBackend {
        fn name(&self) -> &str {
            "server-image-capture"
        }

        async fn stream(
            &self,
            _request: mink::runtime::LlmRequest,
        ) -> anyhow::Result<mink::runtime::LlmResponseStream> {
            unreachable!("this test never runs a turn")
        }
    }

    #[derive(serde::Deserialize)]
    struct Snapshot {
        image_input: mink::runtime::ImageInputCapability,
    }

    fn read_snapshot(session_path: &str) -> Snapshot {
        let raw = std::fs::read_to_string(
            std::path::Path::new(session_path).join("model-capabilities.json"),
        )
        .expect("model-capabilities.json");
        serde_json::from_str(&raw).expect("snapshot parses")
    }

    let root = unique_temp_dir("image-caps");
    let home = root.join("home");
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join(".minkrc"),
        "[provider]\nmodel = \"flash\"\nimage_input = \"on\"\n",
    )
    .unwrap();
    let mut registry = Registry::new(home, "flash".to_string(), 4);
    registry.llm_backend = Some(Arc::new(NoopBackend));

    // `image_input = "on"` must freeze the same image capability the
    // CLI/TUI freezes, or sessions stop being openable across frontends.
    let created = registry.create("vision-cfg", &cwd).await.unwrap();
    match read_snapshot(&created.path).image_input {
        mink::runtime::ImageInputCapability::OpenAiChatImageUrl(limits) => {
            let default = mink::runtime::OpenAiChatImageUrlLimits::default();
            assert_eq!(limits.detail, default.detail);
            assert_eq!(
                limits.max_images_per_request,
                default.max_images_per_request
            );
        }
        mink::runtime::ImageInputCapability::Unsupported => {
            panic!("image_input = \"on\" must freeze an image-capable session snapshot")
        }
    }

    // `[provider.image]` overrides apply on top of the explicit override.
    let override_cwd = root.join("project-override");
    std::fs::create_dir_all(&override_cwd).unwrap();
    std::fs::write(
        override_cwd.join(".minkrc"),
        "[provider]\nimage_input = \"on\"\n\n[provider.image]\nmax_images_per_request = 3\n",
    )
    .unwrap();
    let created = registry
        .create("vision-override", &override_cwd)
        .await
        .unwrap();
    match read_snapshot(&created.path).image_input {
        mink::runtime::ImageInputCapability::OpenAiChatImageUrl(limits) => {
            assert_eq!(limits.max_images_per_request, 3);
        }
        mink::runtime::ImageInputCapability::Unsupported => {
            panic!("[provider.image] must apply on top of image_input = \"on\"")
        }
    }

    match saved_image {
        Some(value) => unsafe { std::env::set_var("MINK_IMAGE_INPUT", value) },
        None => unsafe { std::env::remove_var("MINK_IMAGE_INPUT") },
    }
    match saved_models {
        Some(value) => unsafe { std::env::set_var("MINK_VISION_MODELS", value) },
        None => unsafe { std::env::remove_var("MINK_VISION_MODELS") },
    }
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(test)]
mod poison_public_tests {
    use super::*;

    fn test_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mink-server-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn poisoned_state_lock_does_not_panic_public_entries() {
        let dir = test_dir("poison-public");
        std::fs::create_dir_all(&dir).unwrap();
        let registry = Registry::new(dir.clone(), "flash".into(), 4);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.active.lock().expect("state lock");
            panic!("inject poison");
        }));

        assert!(registry.list().await.is_err(), "list must fail closed");
        assert!(
            registry.active_runtime("missing", None).is_err(),
            "active_runtime must fail closed"
        );
        assert!(
            registry.shutdown_all().await.is_err(),
            "shutdown_all must fail closed"
        );
        // Pure-read helpers recover the poisoned lock instead of panicking.
        assert!(registry.idle_session_ids(Duration::from_secs(0)).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn summary_offers_the_first_prompt_as_a_label_fallback() {
        // A session without a title still has the request that opened it; the
        // list uses that rather than showing a raw id.
        let dir =
            std::env::temp_dir().join(format!("mink-registry-first-prompt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let metadata = SessionMetadata {
            id: "session-under-test".into(),
            alias: None,
            title: None,
            created_at: String::new(),
            updated_at: String::new(),
            cwd: "/tmp".into(),
            parent: None,
            first_prompt: Some("把这个会话的标题补上".into()),
            untitled: false,
            summary: None,
            subagent: None,
        };
        let summary = summary_from_metadata(Some(metadata), None, &dir, (0, 0, 0, 0));
        assert_eq!(summary.title, None);
        assert_eq!(
            summary.first_prompt.as_deref(),
            Some("把这个会话的标题补上")
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
