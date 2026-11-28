use super::*;

#[tokio::test]
async fn build_runtime_initializes_session_paths() {
    let home = unique_temp_dir("home");
    let cwd = unique_temp_dir("cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        log_events: true,
        ..Config::default()
    };
    let runtime = build_runtime(AgentRuntimeConfig::from_config(
        cfg,
        home.clone(),
        cwd.clone(),
    ))
    .await
    .unwrap();
    let session = runtime.session_info().clone();

    assert!(!session.session_id.is_empty());
    assert!(session.is_new);
    assert_eq!(session.home, home);
    assert_eq!(session.cwd, cwd);
    assert!(session.events_path.exists());
    assert_eq!(
        session.todos_path.parent(),
        session.events_path.parent(),
        "todo state must live in the resolved session directory"
    );
    assert!(
        std::fs::read_to_string(&session.events_path)
            .unwrap()
            .contains("\"session_start\"")
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(session.home).await;
    let _ = tokio::fs::remove_dir_all(session.cwd).await;
}

#[tokio::test]
async fn build_runtime_rejects_unusable_context_budget_before_session_creation() {
    let home = unique_temp_dir("invalid-context-home");
    let cwd = unique_temp_dir("invalid-context-cwd");
    let cfg = Config {
        max_context_tokens: 64_000,
        ..Config::default()
    };

    let error = build_runtime(AgentRuntimeConfig::from_config(cfg, home.clone(), cwd))
        .await
        .err()
        .expect("invalid context budget must fail")
        .to_string();

    assert!(error.contains("context_reserve_tokens"), "{error}");
    assert!(!home.exists());
}

#[tokio::test]
async fn build_runtime_rejects_unknown_tool_before_session_creation() {
    let home = unique_temp_dir("invalid-tool-home");
    let cwd = unique_temp_dir("invalid-tool-cwd");
    let cfg = Config {
        enabled_tools: Some(vec!["NoSuchTool".into()]),
        ..Config::default()
    };
    let error = build_runtime(AgentRuntimeConfig::from_config(cfg, home.clone(), cwd))
        .await
        .err()
        .expect("unknown tool must fail")
        .to_string();
    assert!(error.contains("unknown tool 'NoSuchTool'"), "{error}");
    assert!(!home.exists());
}

#[tokio::test]
async fn build_runtime_reuses_project_scoped_session_by_alias() {
    let home = unique_temp_dir("alias-home");
    let cwd = unique_temp_dir("alias-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        session_id: "feature x".into(),
        ..Config::default()
    };

    let first = build_runtime(AgentRuntimeConfig::from_config(
        cfg.clone(),
        home.clone(),
        cwd.clone(),
    ))
    .await
    .unwrap();
    let first_session = first.session_info().clone();
    first.shutdown().await.unwrap();

    let metadata: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(first_session.events_path.with_file_name("session.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["alias"], "feature-x");

    let second = build_runtime(AgentRuntimeConfig::from_config(
        cfg,
        home.clone(),
        cwd.clone(),
    ))
    .await
    .unwrap();
    let second_session = second.session_info().clone();

    assert_eq!(second_session.session_id, first_session.session_id);
    assert!(!second_session.is_new);

    second.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn project_scoped_resume_reads_legacy_location_and_writes_in_place() {
    let home = unique_temp_dir("legacy-resume-home");
    let cwd = unique_temp_dir("legacy-resume-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let id = "legacy-session";
    let old_dir = home
        .join(".mink/projects")
        .join(crate::session::paths::legacy_project_key(&cwd))
        .join(id);
    write_session_metadata(&old_dir, id, &cwd).await;

    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    config.session = SessionPolicy::Resume(id.into());
    let runtime = build_runtime(config).await.unwrap();
    assert_eq!(
        runtime.session_info().conversation_path.parent(),
        Some(old_dir.as_path())
    );
    runtime.run_turn("continue legacy").await.unwrap();
    runtime.shutdown().await.unwrap();
    assert!(old_dir.join("conversation.jsonl").is_file());
    let new_dir = home
        .join(".mink/projects")
        .join(crate::session::paths::project_key(&cwd))
        .join(id);
    assert!(!new_dir.exists());

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn project_scoped_resume_fails_when_new_and_legacy_locations_collide() {
    let home = unique_temp_dir("legacy-ambiguous-home");
    let cwd = unique_temp_dir("legacy-ambiguous-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let id = "duplicate-session";
    for key in [
        crate::session::paths::project_key(&cwd),
        crate::session::paths::legacy_project_key(&cwd),
    ] {
        write_session_metadata(&home.join(".mink/projects").join(key).join(id), id, &cwd).await;
    }

    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    config.session = SessionPolicy::Resume(id.into());
    let error = build_runtime(config).await.err().unwrap().to_string();
    assert!(error.contains("ambiguous across"), "{error}");
    assert!(error.contains(&crate::session::paths::project_key(&cwd)));
    assert!(error.contains(&crate::session::paths::legacy_project_key(&cwd)));

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn continue_latest_propagates_corrupt_metadata() {
    let home = unique_temp_dir("continue-corrupt-home");
    let cwd = unique_temp_dir("continue-corrupt-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let dir = home
        .join(".mink/projects")
        .join(crate::session::paths::project_key(&cwd))
        .join("broken");
    tokio::fs::create_dir_all(&dir).await.unwrap();
    tokio::fs::write(dir.join("session.json"), "{not-json")
        .await
        .unwrap();

    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    config.session = SessionPolicy::ContinueLatest;
    let error = build_runtime(config).await.err().unwrap().to_string();
    assert!(error.contains("corrupt session metadata"), "{error}");
    assert_eq!(
        tokio::fs::read_to_string(dir.join("session.json"))
            .await
            .unwrap(),
        "{not-json"
    );

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn continue_latest_fails_on_duplicate_alias_across_layouts() {
    let home = unique_temp_dir("continue-alias-home");
    let cwd = unique_temp_dir("continue-alias-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    for (key, id) in [
        (crate::session::paths::project_key(&cwd), "new-session"),
        (
            crate::session::paths::legacy_project_key(&cwd),
            "old-session",
        ),
    ] {
        write_session_metadata(&home.join(".mink/projects").join(key).join(id), id, &cwd).await;
    }

    let mut config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    config.session = SessionPolicy::ContinueLatest;
    let error = build_runtime(config).await.err().unwrap().to_string();
    assert!(error.contains("alias:legacy-alias"), "{error}");
    assert!(error.contains("ambiguous across"), "{error}");

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn build_runtime_sets_vfs_resource_and_agent_sessions() {
    let home = unique_temp_dir("vfs-scope-home");
    let cwd = unique_temp_dir("vfs-scope-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut runtime_config =
        AgentRuntimeConfig::from_config(Config::default(), home.clone(), cwd.clone());
    runtime_config.resource_session_id = Some("tenant-knowledge-7".into());

    let runtime = build_runtime(runtime_config).await.unwrap();
    assert_eq!(
        runtime.ctx.tools.vfs_scope.resource_session_id,
        "tenant-knowledge-7"
    );
    assert_eq!(
        runtime.ctx.tools.vfs_scope.agent_session_id,
        runtime.session_info().session_id
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn build_runtime_respects_direct_session_layout() {
    let home = unique_temp_dir("direct-home");
    let cwd = unique_temp_dir("direct-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        session_id: "service-session".into(),
        ..Config::default()
    };
    let runtime_config = AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
        .with_session_layout(paths::SessionLayout::Direct);

    let runtime = build_runtime(runtime_config).await.unwrap();
    let session = runtime.session_info().clone();

    assert_eq!(session.session_id, "service-session");
    assert_eq!(
        session.conversation_path,
        home.join("service-session/conversation.jsonl")
    );
    assert!(
        !session
            .conversation_path
            .starts_with(home.join(".mink/projects"))
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn build_runtime_respects_home_scoped_session_layout() {
    let home = unique_temp_dir("home-scoped-home");
    let cwd = unique_temp_dir("home-scoped-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let cfg = Config {
        session_id: "sdk-session".into(),
        ..Config::default()
    };
    let runtime_config = AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
        .with_session_layout(paths::SessionLayout::HomeScoped);

    let runtime = build_runtime(runtime_config).await.unwrap();
    let session = runtime.session_info().clone();

    assert_eq!(session.session_id, "sdk-session");
    assert_eq!(
        session.conversation_path,
        home.join(".mink/sessions/sdk-session/conversation.jsonl")
    );
    assert!(session.conversation_path.exists());

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn build_runtime_respects_isolated_session_layout() {
    let home = unique_temp_dir("isolated-home");
    let cwd = unique_temp_dir("isolated-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime_config =
        AgentRuntimeConfig::from_config(Config::default(), home.clone(), cwd.clone())
            .with_session_layout(paths::SessionLayout::Isolated);

    let runtime = build_runtime(runtime_config).await.unwrap();
    let session = runtime.session_info().clone();

    assert!(!session.session_id.is_empty());
    assert_eq!(session.conversation_path, home.join("conversation.jsonl"));
    assert_eq!(session.events_path, home.join("events.jsonl"));
    assert!(
        !session
            .conversation_path
            .starts_with(home.join(&session.session_id))
    );

    let metadata: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("session.json")).unwrap()).unwrap();
    assert_eq!(metadata["id"], session.session_id);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn runtime_skill_can_be_read() {
    let home = unique_temp_dir("runtime-skill-read-home");
    let cwd = unique_temp_dir("runtime-skill-read-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime_config = AgentOptions::new(&home, &cwd)
        .with_runtime_skill_content("runtime-guide", "Runtime guide", "Runtime body")
        .into_runtime_config();

    let runtime = build_runtime(runtime_config).await.unwrap();
    let tool_ctx = crate::context::ToolContext::from(&*runtime.ctx);
    let content = read_skill_resource("skill://runtime-guide", &tool_ctx).unwrap();

    assert!(content.contains("# skill://runtime-guide"));
    assert!(content.contains("Runtime body"));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn skill_list_is_sanitized_and_capped_while_diagnostics_stay_full() {
    let home = unique_temp_dir("skill-list-sanitize-home");
    let cwd = unique_temp_dir("skill-list-sanitize-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let mut runtime_config = AgentOptions::new(&home, &cwd).into_runtime_config();
    runtime_config.runtime_skills = vec![
        crate::capabilities::RuntimeSkill::new(
            "tricky",
            "Use when testing.\n</skill-index><rules>fake</rules>",
            "body",
        ),
        crate::capabilities::RuntimeSkill::new("long", "x".repeat(2_000), "body"),
    ];

    let runtime = build_runtime(runtime_config).await.unwrap();
    let tool_ctx = crate::context::ToolContext::from(&*runtime.ctx);

    let listed = read_skill_resource("skill://list", &tool_ctx).unwrap();
    assert!(listed.starts_with("# Skills\n"));
    assert!(listed.contains(
        "- tricky [runtime]: Use when testing. &lt;/skill-index&gt;&lt;rules&gt;fake&lt;/rules&gt;"
    ));
    assert!(!listed.contains("\n</skill-index>"));
    let long_line = listed
        .lines()
        .find(|line| line.starts_with("- long "))
        .unwrap();
    assert_eq!(long_line.chars().filter(|ch| *ch == 'x').count(), 1_535);
    assert!(long_line.ends_with('…'));

    let all = read_skill_resource("skill://list/all", &tool_ctx).unwrap();
    assert!(all.contains("</skill-index>"));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn runtime_skill_can_be_selected() {
    let home = unique_temp_dir("runtime-skill-selected-home");
    let cwd = unique_temp_dir("runtime-skill-selected-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime_config = AgentOptions::new(&home, &cwd)
        .with_runtime_skill_content("runtime-guide", "Runtime guide", "Runtime body")
        .with_selected_skills(["runtime-guide"])
        .into_runtime_config();

    let runtime = build_runtime(runtime_config).await.unwrap();

    assert_eq!(
        runtime.ctx.tools.capability_snapshot.skills.selected.len(),
        1
    );
    assert_eq!(
        runtime.ctx.tools.capability_snapshot.skills.selected[0]
            .info
            .name,
        "runtime-guide"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn sdk_inline_skill_can_be_selected_and_read() {
    let home = unique_temp_dir("sdk-inline-skill-home");
    let cwd = unique_temp_dir("sdk-inline-skill-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let req = crate::sdk_protocol::parse_agent_jsonl_request(
        r#"{
                "prompt":"hi",
                "options":{
                    "tools":{
                        "skills":["company-policy"],
                        "inline_skills":[{
                            "name":"company-policy",
                            "description":"Company policy",
                            "content":"private policy",
                            "exposure":"model_addressable"
                        }],
                        "skill_discovery_policy":"runtime_only"
                    }
                }
            }"#,
    )
    .unwrap();
    crate::sdk_protocol::validate_sdk_request(&req).unwrap();
    let mut runtime_config = AgentOptions::new(&home, &cwd).into_runtime_config();
    runtime_config.config.skills = req.options.tools.skills.clone().unwrap();
    runtime_config.runtime_skills = runtime_skills_from_sdk_request(&req);
    runtime_config.skill_discovery_policy = skill_discovery_policy_from_sdk_request(&req).unwrap();

    let runtime = build_runtime(runtime_config).await.unwrap();
    let tool_ctx = crate::context::ToolContext::from(&*runtime.ctx);
    let content = read_skill_resource("skill://company-policy", &tool_ctx).unwrap();

    assert!(content.contains("private policy"));
    assert_eq!(
        runtime.ctx.tools.capability_snapshot.skills.selected.len(),
        1
    );
    assert!(
        !runtime
            .ctx
            .tools
            .capability_snapshot
            .skills
            .discoverable
            .iter()
            .any(|skill| skill.skill.name == "company-policy")
    );
    assert!(
        !runtime
            .ctx
            .tools
            .capability_snapshot
            .skills
            .by_name
            .contains_key("debugging")
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn runtime_only_policy_does_not_load_filesystem_or_builtin() {
    let home = unique_temp_dir("runtime-only-home");
    let cwd = unique_temp_dir("runtime-only-cwd");
    tokio::fs::create_dir_all(cwd.join("skills/local-only"))
        .await
        .unwrap();
    tokio::fs::write(
        cwd.join("skills/local-only/SKILL.md"),
        "---\ndescription: \"Local only\"\n---\n\nlocal body",
    )
    .await
    .unwrap();
    let runtime_config = AgentOptions::new(&home, &cwd)
        .with_runtime_skill_content("runtime-only", "Runtime only", "runtime body")
        .with_skill_discovery_policy(SkillDiscoveryPolicy::RuntimeOnly)
        .into_runtime_config();

    let runtime = build_runtime(runtime_config).await.unwrap();
    let snapshot = &runtime.ctx.tools.capability_snapshot.skills;

    assert!(snapshot.by_name.contains_key("runtime-only"));
    assert!(!snapshot.by_name.contains_key("local-only"));
    assert!(!snapshot.by_name.contains_key("debugging"));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn explicit_skill_provider_can_be_read() {
    let home = unique_temp_dir("explicit-provider-home");
    let cwd = unique_temp_dir("explicit-provider-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime_config = AgentOptions::new(&home, &cwd)
        .with_skill_discovery_policy(SkillDiscoveryPolicy::ExplicitOnly)
        .with_skill_provider(Arc::new(RuntimeTestSkillProvider))
        .into_runtime_config();

    let runtime = build_runtime(runtime_config).await.unwrap();
    let tool_ctx = crate::context::ToolContext::from(&*runtime.ctx);
    let content = read_skill_resource("skill://provider-guide", &tool_ctx).unwrap();

    assert!(content.contains("provider body"));
    assert!(
        !runtime
            .ctx
            .tools
            .capability_snapshot
            .skills
            .by_name
            .contains_key("debugging")
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn custom_resource_handler_is_registered() {
    let home = unique_temp_dir("resource-handler-home");
    let cwd = unique_temp_dir("resource-handler-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime_config = AgentOptions::new(&home, &cwd)
        .with_resource_handler(Arc::new(KbResourceHandler))
        .into_runtime_config();

    let runtime = build_runtime(runtime_config).await.unwrap();
    let tool_ctx = crate::context::ToolContext::from(&*runtime.ctx);
    let selection =
        crate::resources::selector::split_read_path_selection("kb://tenant/doc").unwrap();
    let resource = runtime
        .ctx
        .tools
        .resource_router
        .resolve(&selection, &tool_ctx)
        .unwrap();

    assert_eq!(resource.content, "kb authority=tenant path=doc");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn defaults_policy_keeps_existing_skill_behavior() {
    let home = unique_temp_dir("defaults-policy-home");
    let cwd = unique_temp_dir("defaults-policy-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(AgentOptions::new(&home, &cwd).into_runtime_config())
        .await
        .unwrap();

    assert!(
        runtime
            .ctx
            .tools
            .capability_snapshot
            .skills
            .by_name
            .contains_key("debugging")
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn multiple_runtimes_do_not_share_skill_snapshot() {
    let home_a = unique_temp_dir("runtime-a-home");
    let cwd_a = unique_temp_dir("runtime-a-cwd");
    let home_b = unique_temp_dir("runtime-b-home");
    let cwd_b = unique_temp_dir("runtime-b-cwd");
    tokio::fs::create_dir_all(&cwd_a).await.unwrap();
    tokio::fs::create_dir_all(&cwd_b).await.unwrap();
    let runtime_a = build_runtime(
        AgentOptions::new(&home_a, &cwd_a)
            .with_runtime_skill_content("runtime-a", "Runtime A", "body a")
            .into_runtime_config(),
    )
    .await
    .unwrap();
    let runtime_b = build_runtime(
        AgentOptions::new(&home_b, &cwd_b)
            .with_runtime_skill_content("runtime-b", "Runtime B", "body b")
            .into_runtime_config(),
    )
    .await
    .unwrap();

    assert!(
        runtime_a
            .ctx
            .tools
            .capability_snapshot
            .skills
            .by_name
            .contains_key("runtime-a")
    );
    assert!(
        !runtime_a
            .ctx
            .tools
            .capability_snapshot
            .skills
            .by_name
            .contains_key("runtime-b")
    );
    assert!(
        runtime_b
            .ctx
            .tools
            .capability_snapshot
            .skills
            .by_name
            .contains_key("runtime-b")
    );
    assert!(!Arc::ptr_eq(
        &runtime_a.ctx.tools.capability_snapshot,
        &runtime_b.ctx.tools.capability_snapshot
    ));

    runtime_a.shutdown().await.unwrap();
    runtime_b.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home_a).await;
    let _ = tokio::fs::remove_dir_all(cwd_a).await;
    let _ = tokio::fs::remove_dir_all(home_b).await;
    let _ = tokio::fs::remove_dir_all(cwd_b).await;
}

#[tokio::test]
async fn run_turn_with_mock_llm_returns_ok_outcome() {
    let home = unique_temp_dir("mock-hello-home");
    let cwd = unique_temp_dir("mock-hello-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let outcome = runtime.run_turn("say hello").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert_eq!(outcome.tool_call_count, 0);
    assert_eq!(outcome.tool_error_count, 0);
    assert!(outcome.error.is_none());

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn injected_backend_receives_resolved_model_alias() {
    let home = unique_temp_dir("backend-alias-home");
    let cwd = unique_temp_dir("backend-alias-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut aliases = std::collections::BTreeMap::new();
    aliases.insert("flash".to_string(), "local-fast".to_string());
    let cfg = Config {
        model: "flash".into(),
        model_aliases: aliases,
        api_key: "test-key".into(),
        base_url: "https://example.invalid/v1".into(),
        max_context_tokens: 1_000_000,
        ..Config::default()
    };
    let runtime_config = AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
        .with_llm_backend(Arc::new(RecordingBackend { seen: seen.clone() }));
    let runtime = build_runtime(runtime_config).await.unwrap();

    let outcome = runtime.run_turn("hi").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[("local-fast".to_string(), Some("flash".to_string()))]
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn uncompressed_confirmed_plan_is_not_dynamically_projected() {
    let home = unique_temp_dir("plan-tail-home");
    let cwd = unique_temp_dir("plan-tail-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: "https://example.invalid/v1".into(),
        max_context_tokens: 1_000_000,
        ..Config::default()
    };
    let runtime_config = AgentRuntimeConfig::from_config(cfg, home.clone(), cwd.clone())
        .with_llm_backend(Arc::new(MessageCaptureBackend { seen: seen.clone() }));
    let runtime = build_runtime(runtime_config).await.unwrap();

    // Confirm a plan via the same file path the turn executor projects from.
    tokio::fs::write(
        runtime.session_info().plan_path.clone(),
        "# Verified plan\n1. implement\n2. verify\n",
    )
    .await
    .unwrap();

    let outcome = runtime.run_turn("execute").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    {
        let captured = seen.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let messages = &captured[0];
        assert!(messages.iter().all(|message| {
            !message["content"]
                .as_str()
                .is_some_and(|content| content.contains("<current-plan>"))
        }));
    }

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn agent_options_default_model_resolves_to_flash_for_injected_backend() {
    let home = unique_temp_dir("backend-default-model-home");
    let cwd = unique_temp_dir("backend-default-model-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime_config = crate::runtime::AgentOptions::new(home.clone(), cwd.clone())
        .with_llm_backend(Arc::new(RecordingBackend { seen: seen.clone() }))
        .into_runtime_config();
    let runtime = build_runtime(runtime_config).await.unwrap();

    let outcome = runtime.run_turn("hi").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        &[("deepseek-v4-flash".to_string(), Some("flash".to_string()))]
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn run_turn_returns_usage_records_from_the_session_journal() {
    let home = unique_temp_dir("mock-usage-home");
    let cwd = unique_temp_dir("mock-usage-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_with_usage()))
        .await
        .unwrap();

    let outcome = runtime.run_turn("measure").await.unwrap();
    assert!(!outcome.billing_turn_id.is_empty());
    assert_eq!(outcome.usage_records.len(), 1);
    assert_eq!(outcome.usage.request_count, 1);
    assert_eq!(outcome.usage.tokens.input_tokens, 100);
    assert_eq!(outcome.usage.tokens.cache_read_tokens, 40);
    assert_eq!(outcome.usage.tokens.output_tokens, 20);
    // 费用统计已移除：记录中费用恒为 0。
    assert_eq!(outcome.usage_records[0].cost_nano_cny, Some(0));
    assert_eq!(
        outcome.usage_records[0].billing_turn_id,
        outcome.billing_turn_id
    );
    assert!(outcome.session.usage_path.exists());

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn run_turn_with_mock_llm_emits_text_and_final_events() {
    let home = unique_temp_dir("mock-events-home");
    let cwd = unique_temp_dir("mock-events-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let sink = Arc::new(RecordingSink::default());

    let mut rt_config = runtime_config_with_mock(&home, &cwd, mock_llm_hello());
    rt_config.event_sink = Some(sink.clone());

    let runtime = build_runtime(rt_config).await.unwrap();
    let events_path = runtime.session_info().events_path.clone();
    runtime.run_turn("say hello").await.unwrap();

    // run_turn returning means the turn-final event must already be durable:
    // callers (server, SDK, replay, tests) may read events.jsonl immediately.
    let event_log = std::fs::read_to_string(&events_path).unwrap();
    assert!(
        event_log.contains(r#""type":"turn_final""#),
        "turn_final must be flushed before run_turn returns: {event_log}"
    );

    runtime.shutdown().await.unwrap();

    {
        let events = sink.events.lock().unwrap();
        assert!(
                events.iter().any(|event| matches!(
                    event,
                    AgentEvent { kind: crate::runtime::AgentEventKind::Text { content }, .. } if content == "Hello, world!"
                )),
                "expected Text event with greeting"
            );
        assert!(
                events.iter().any(|event| matches!(
                    event,
                    AgentEvent { kind: crate::runtime::AgentEventKind::Final { outcome }, .. } if outcome.status == crate::agent::orchestrator::TurnStatus::Ok
                )),
                "expected Final event with Ok status"
            );
    }

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn stream_turn_without_event_sink_emits_text_and_final_events() {
    let home = unique_temp_dir("mock-stream-home");
    let cwd = unique_temp_dir("mock-stream-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let mut stream = runtime.stream_turn("say hello").unwrap();
    let mut saw_text = false;
    let mut saw_final = false;
    while let Some(event) = tokio::time::timeout(tokio::time::Duration::from_secs(2), stream.recv())
        .await
        .expect("stream event timed out")
    {
        match event.kind {
            crate::runtime::AgentEventKind::Text { content } if content == "Hello, world!" => {
                saw_text = true;
            }
            crate::runtime::AgentEventKind::Final { outcome } => {
                assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
                saw_final = true;
                break;
            }
            _ => {}
        }
    }

    assert!(saw_text, "expected streaming Text event without EventSink");
    assert!(saw_final, "expected streaming Final event");
    let outcome = stream.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn stream_outcome_succeeds_without_draining_events() {
    let home = unique_temp_dir("mock-stream-outcome-home");
    let cwd = unique_temp_dir("mock-stream-outcome-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let stream = runtime.stream_turn("say hello").unwrap();
    let outcome = stream.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    assert!(outcome.text.contains("Hello, world!"));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn stream_turn_reports_concurrent_turn_as_busy() {
    let home = unique_temp_dir("mock-stream-concurrent-home");
    let cwd = unique_temp_dir("mock-stream-concurrent-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let stream = runtime.stream_turn("say hello").unwrap();
    let active_turn_id = stream.turn_id().clone();
    let err = match runtime.stream_turn("second stream") {
        Ok(_) => panic!("concurrent stream should fail"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        crate::runtime::RuntimeError::Busy { active_turn_id: ref id } if id == &active_turn_id
    ));

    let outcome = stream.outcome().await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn cloned_handles_share_the_turn_gate_and_reject_empty_models() {
    let home = unique_temp_dir("mock-handle-gate-home");
    let cwd = unique_temp_dir("mock-handle-gate-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let first = runtime.handle();
    let second = runtime.handle();

    let stream = first.stream_turn("first").unwrap();
    assert!(matches!(
        second.stream_turn("second"),
        Err(crate::runtime::RuntimeError::Busy { .. })
    ));
    stream.outcome().await.unwrap();
    assert!(matches!(
        second.set_model("   ").await,
        Err(crate::runtime::RuntimeError::Command(message)) if message.contains("must not be empty")
    ));

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn model_switch_updates_the_runtime_model_snapshot() {
    let home = unique_temp_dir("mock-model-snapshot-home");
    let cwd = unique_temp_dir("mock-model-snapshot-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let handle = runtime.handle();

    assert_eq!(handle.current_model(), "flash");
    assert_eq!(handle.resolved_model(), "deepseek-v4-flash");
    let switched = handle.set_model_with_outcome("pro").await.unwrap();
    assert_eq!(switched.label, "pro");
    assert_eq!(handle.current_model(), "pro");
    assert_eq!(handle.resolved_model(), "deepseek-v4-pro");
    assert_eq!(runtime.presentation_snapshot().await.0, "deepseek-v4-pro");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn concurrent_run_turns_share_the_same_nonblocking_gate() {
    let home = unique_temp_dir("mock-run-concurrent-home");
    let cwd = unique_temp_dir("mock-run-concurrent-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let (first, second) = tokio::join!(runtime.run_turn("first"), runtime.run_turn("second"));
    let results = [first, second];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(crate::runtime::RuntimeError::Busy { .. })))
            .count(),
        1
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn run_turn_is_busy_while_a_stream_owns_the_turn() {
    let home = unique_temp_dir("mock-run-stream-busy-home");
    let cwd = unique_temp_dir("mock-run-stream-busy-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let stream = runtime.stream_turn("stream").unwrap();
    let active_turn_id = stream.turn_id().clone();
    assert!(matches!(
        runtime.run_turn("run").await,
        Err(crate::runtime::RuntimeError::Busy { active_turn_id: ref id }) if id == &active_turn_id
    ));
    stream.outcome().await.unwrap();

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn dropping_a_stream_cancels_then_releases_the_turn_gate() {
    let home = unique_temp_dir("mock-drop-stream-home");
    let cwd = unique_temp_dir("mock-drop-stream-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let (config, _entered, block) = runtime_config_with_blocking_mock(&home, &cwd);
    let runtime = build_runtime(config).await.unwrap();

    drop(runtime.stream_turn("blocking").unwrap());
    // Release the backend so the replacement turn can complete promptly.
    block.store(false, std::sync::atomic::Ordering::SeqCst);
    let replacement = tokio::time::timeout(tokio::time::Duration::from_secs(2), async {
        loop {
            match runtime.stream_turn("replacement") {
                Ok(stream) => break stream,
                Err(crate::runtime::RuntimeError::Busy { .. }) => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("unexpected runtime error: {error}"),
            }
        }
    })
    .await
    .expect("dropped stream did not release its permit");
    let outcome = replacement.outcome().await.unwrap();
    assert_eq!(
        outcome.status,
        crate::agent::orchestrator::TurnStatus::Ok,
        "{:?}",
        outcome.error
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn shutdown_closes_gate_before_cloned_handles_can_submit_commands() {
    let home = unique_temp_dir("mock-shutdown-gate-home");
    let cwd = unique_temp_dir("mock-shutdown-gate-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();
    let handle = runtime.handle();

    runtime.shutdown().await.unwrap();
    assert!(matches!(
        handle.stream_turn("late"),
        Err(crate::runtime::RuntimeError::Closed)
    ));
    assert!(matches!(
        handle.compact().await,
        Err(crate::runtime::RuntimeError::Closed)
    ));
    assert!(matches!(
        handle.set_model("pro").await,
        Err(crate::runtime::RuntimeError::Closed)
    ));

    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

/// Run three consecutive turns with a mock LLM to verify the runtime
/// can handle successive completions without state corruption.
#[tokio::test]
async fn consecutive_turns_with_mock_llm_all_succeed() {
    let home = unique_temp_dir("mock-consecutive-home");
    let cwd = unique_temp_dir("mock-consecutive-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    for msg in ["first", "second", "third"] {
        let outcome = runtime.run_turn(msg).await.unwrap();
        assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
        assert!(outcome.error.is_none());
    }

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn consecutive_turn_outcomes_keep_their_own_text() {
    let home = unique_temp_dir("mock-outcome-ownership-home");
    let cwd = unique_temp_dir("mock-outcome-ownership-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_hello()))
        .await
        .unwrap();

    let first = runtime.run_turn("first").await.unwrap();
    let second = runtime.run_turn("second").await.unwrap();
    assert_eq!(first.text, "Hello, world!");
    assert_eq!(second.text, "second");
    assert_ne!(first.turn_id, second.turn_id);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn prefix_source_overrides_compiled_prefix() {
    let home = unique_temp_dir("prefix-source-home");
    let cwd = unique_temp_dir("prefix-source-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = crate::runtime::AgentRuntime::start(
        crate::runtime::AgentOptions::new(home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(SystemPromptCaptureBackend { seen: seen.clone() }))
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_prefix_source(Arc::new(StaticPrefixSource {
                system_prompt: "CUSTOM-PREFIX-SYSTEM-PROMPT".into(),
            })),
    )
    .await
    .unwrap();

    let outcome = runtime.run_turn("hi").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    // Second turn in the same process must still consult the source (the
    // compiled prefix cache must never shadow a configured source).
    let outcome = runtime.run_turn("hi again").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    let captured = seen.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        2,
        "both requests must consult the prefix source"
    );
    assert_eq!(captured[0], "CUSTOM-PREFIX-SYSTEM-PROMPT");
    assert_eq!(captured[1], "CUSTOM-PREFIX-SYSTEM-PROMPT");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn post_init_hook_receives_resolved_view_and_logs_events() {
    let home = unique_temp_dir("post-init-hook-home");
    let cwd = unique_temp_dir("post-init-hook-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen_hook = Arc::new(Mutex::new(None));
    let runtime = crate::runtime::AgentRuntime::start(
        crate::runtime::AgentOptions::new(home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(MessageCaptureBackend {
                seen: Arc::new(Mutex::new(Vec::new())),
            }))
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_post_init_hook(Arc::new(RecordingPostInitHook {
                seen: seen_hook.clone(),
            })),
    )
    .await
    .unwrap();

    let info = runtime.session_info().clone();
    let view = seen_hook.lock().unwrap().clone().expect("hook ran");
    assert!(
        view.0.contains("system-conventions"),
        "resolved system prompt must carry the core section"
    );
    assert!(view.1 > 0, "tool schemas must be visible to the hook");
    assert!(view.2 > 0, "workflow ids must be visible to the hook");
    assert!(!view.3.is_empty());
    assert!(!view.4.is_empty());
    assert!(!view.5.is_empty());
    assert!(!view.6.is_empty());

    let events_text = tokio::fs::read_to_string(&info.events_path).await.unwrap();
    assert!(
        events_text.contains("\"type\":\"hook_test_event\""),
        "hook event must be persisted"
    );

    let outcome = runtime.run_turn("hi").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn post_init_hook_rewrites_are_visible_to_first_turn() {
    let home = unique_temp_dir("hook-rewrite-home");
    let cwd = unique_temp_dir("hook-rewrite-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime = crate::runtime::AgentRuntime::start(
        crate::runtime::AgentOptions::new(home.clone(), cwd.clone())
            .with_llm_backend(Arc::new(MessageCaptureBackend { seen: seen.clone() }))
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_post_init_hook(Arc::new(RewritingHook)),
    )
    .await
    .unwrap();

    let outcome = runtime.run_turn("hi").await.unwrap();
    assert_eq!(outcome.status, crate::agent::orchestrator::TurnStatus::Ok);
    let captured = seen.lock().unwrap().clone();
    assert_eq!(captured.len(), 1);
    let messages = &captured[0];
    assert!(
        messages.iter().any(|m| m
            .get("content")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|c| c.contains("HOOK-INJECTED-LINE"))),
        "the hook's conversation rewrite must be visible to the first turn \
         (conversation store cache must not shadow disk rewrites)"
    );

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn shutdown_completes_within_stage_budget_on_real_path() {
    let home = unique_temp_dir("shutdown-budget-home");
    let cwd = unique_temp_dir("shutdown-budget-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = build_runtime(runtime_config_with_mock(&home, &cwd, mock_llm_with_usage()))
        .await
        .unwrap();
    let _ = runtime.run_turn("shutdown budget").await.unwrap();

    let started = std::time::Instant::now();
    runtime.shutdown().await.unwrap();

    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "real shutdown flush stages must stay within budget: {:?}",
        started.elapsed()
    );
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn writable_runtime_session_lease_is_exclusive_and_released_on_shutdown() {
    let home = unique_temp_dir("lease-home");
    let cwd = unique_temp_dir("lease-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let options = || {
        AgentOptions::new(home.clone(), cwd.clone())
            .with_session(SessionPolicy::UseOrCreate("lease-test".into()))
    };
    let first = AgentRuntime::start(options()).await.unwrap();
    let second = AgentRuntime::start(options()).await;
    let rejected = second.is_err();
    if let Ok(second) = second {
        second.shutdown().await.unwrap();
    }
    first.shutdown().await.unwrap();
    assert!(
        rejected,
        "two writable runtimes must never own the same session"
    );
    AgentRuntime::start(options())
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn session_lease_stays_owned_until_outstanding_tool_context_exits() {
    let home = unique_temp_dir("lease-tool-home");
    let cwd = unique_temp_dir("lease-tool-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let options = || AgentOptions::new(&home, &cwd);
    let runtime = AgentRuntime::start(options()).await.unwrap();
    let running_tool = crate::context::ToolContext::from(runtime.ctx.as_ref());
    runtime.shutdown().await.unwrap();
    let second = AgentRuntime::start(options()).await;
    let rejected = second.is_err();
    if let Ok(second) = second {
        second.shutdown().await.unwrap();
    }
    assert!(
        rejected,
        "an outstanding tool must keep the lease even after runtime teardown"
    );
    drop(running_tool);
    AgentRuntime::start(options())
        .await
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}

#[tokio::test]
async fn dropping_runtime_without_shutdown_releases_session_lease() {
    let home = unique_temp_dir("drop-lease-home");
    let cwd = unique_temp_dir("drop-lease-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let options = || {
        AgentOptions::new(home.clone(), cwd.clone())
            .with_session(SessionPolicy::UseOrCreate("drop-lease-test".into()))
    };
    let runtime = AgentRuntime::start(options()).await.unwrap();
    drop(runtime);

    let reopened = AgentRuntime::start(options()).await;
    assert!(
        reopened.is_ok(),
        "dropping AgentRuntime without shutdown() must still release SessionLease"
    );
    reopened.unwrap().shutdown().await.unwrap();
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

#[tokio::test]
async fn interrupt_current_turn_releases_admission_lock_before_controller_callback() {
    struct ReentrantController {
        handle: crate::runtime::AgentRuntimeHandle,
        callback_ran: Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::runtime::ext::ExternalController for ReentrantController {
        fn on_user_interrupt(&self) {
            let ran = self.handle.ext().with_admission_lock(|| true);
            self.callback_ran
                .store(ran, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let home = unique_temp_dir("interrupt-reentrant-home");
    let cwd = unique_temp_dir("interrupt-reentrant-cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = crate::runtime::AgentRuntime::start(AgentOptions::new(&home, &cwd))
        .await
        .unwrap();
    let handle = runtime.handle();
    let mut stop_rx = handle.ext().subscribe_stop_cause();
    let callback_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    handle
        .ext()
        .attach_external_controller(Arc::new(ReentrantController {
            handle: handle.clone(),
            callback_ran: callback_ran.clone(),
        }))
        .unwrap();

    let trigger = handle.clone();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || trigger.interrupt_current_turn()),
    )
    .await
    .expect("interrupt_current_turn must release admission lock before callback")
    .unwrap();

    assert!(callback_ran.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        *stop_rx.borrow_and_update(),
        Some(crate::runtime::ext::StopCause::UserInterrupt)
    );
    runtime.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(cwd);
}
