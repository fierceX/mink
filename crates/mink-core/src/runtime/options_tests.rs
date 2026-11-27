use super::*;

#[test]
fn options_convert_to_runtime_config_without_losing_config_fields() {
    let runtime_config = AgentOptions::new("/tmp/mink-home", "/tmp/project")
        .with_model("pro")
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1")
        .with_session(SessionPolicy::UseOrCreate("work".to_string()))
        .with_max_tokens(123)
        .with_max_turns(7)
        .with_max_context_tokens(456)
        .with_context_compact_pct(70)
        .with_context_reserve_tokens(71)
        .with_context_compact_tail_tokens(72)
        .with_context_compact_max_output_tokens(73)
        .with_context_compact_input_reduction(true)
        .with_tool_timeout_secs(10)
        .with_tool_timeout_max_secs(1_800)
        .with_sub_agent_timeout_secs(11)
        .with_llm_timeouts(12, 13, 14)
        .with_tool_result_max_bytes(15)
        .with_file_write_max_bytes(16)
        .with_search_limits(17, 18)
        .with_output_format(OutputFormat::StreamJson)
        .with_verbose(true)
        .with_log_events(false)
        .with_openai_reasoning_effort("high")
        .with_openai_include_usage(false)
        .with_openai_token_param(TokenParamKind::MaxCompletionTokens)
        .with_openai_tool_choice("auto")
        .with_openai_extra_body(BTreeMap::from([(
            "enable_thinking".to_string(),
            serde_json::json!(true),
        )]))
        .with_runtime_skill_content("runtime-rust", "Runtime Rust", "runtime body")
        .with_skill_discovery_policy(SkillDiscoveryPolicy::RuntimeOnly)
        .with_selected_skills(["runtime-rust"])
        .with_mission_content("mission")
        .with_enabled_tools(vec!["Read".to_string(), "Bash".to_string()])
        .with_tool_approval_mode(ToolApprovalMode::Write)
        .with_llm_recovery(crate::config::LlmRecoveryPolicy {
            format_window_size: 5,
            format_max_errors: 2,
            request_max_retries: 1,
            request_timeout_secs: Some(30),
        })
        .with_first_prompt("hello")
        .into_runtime_config();

    assert_eq!(runtime_config.home, PathBuf::from("/tmp/mink-home"));
    assert_eq!(runtime_config.cwd, PathBuf::from("/tmp/project"));
    assert_eq!(runtime_config.session_layout, SessionLayout::Isolated);
    assert_eq!(
        runtime_config.skill_discovery_policy,
        SkillDiscoveryPolicy::RuntimeOnly
    );
    assert_eq!(runtime_config.runtime_skills.len(), 1);
    assert_eq!(runtime_config.runtime_skills[0].name, "runtime-rust");
    assert!(matches!(
        runtime_config.session,
        SessionPolicy::UseOrCreate(ref value) if value == "work"
    ));
    assert_eq!(runtime_config.first_prompt.as_deref(), Some("hello"));

    let cfg = runtime_config.config;
    assert_eq!(cfg.model, "pro");
    assert_eq!(cfg.api_key, "test-key");
    assert_eq!(cfg.base_url, "https://example.invalid/v1");
    assert_eq!(cfg.max_tokens, 123);
    assert_eq!(cfg.max_turns, 7);
    assert_eq!(cfg.max_context_tokens, 456);
    assert_eq!(cfg.context_compact_pct, 70);
    assert_eq!(cfg.context_reserve_tokens, 71);
    assert_eq!(cfg.context_compact_tail_tokens, 72);
    assert_eq!(cfg.context_compact_max_output_tokens, 73);
    assert!(cfg.context_compact_input_reduction);
    assert_eq!(cfg.tool_timeout_secs, 10);
    assert_eq!(cfg.tool_timeout_max_secs, 1_800);
    assert_eq!(cfg.sub_agent_timeout_secs, 11);
    assert_eq!(cfg.llm_first_event_timeout_secs, 12);
    assert_eq!(cfg.llm_idle_timeout_secs, 13);
    assert_eq!(cfg.llm_wait_heartbeat_secs, 14);
    assert_eq!(cfg.tool_result_max_bytes, 15);
    assert_eq!(cfg.file_write_max_bytes, 16);
    assert_eq!(cfg.max_search_files, 17);
    assert_eq!(cfg.max_search_results, 18);
    assert_eq!(cfg.output_format, OutputFormat::StreamJson);
    assert!(cfg.verbose);
    assert!(!cfg.log_events);
    assert_eq!(cfg.openai_reasoning_effort.as_deref(), Some("high"));
    assert!(!cfg.openai_include_usage);
    assert_eq!(cfg.openai_token_param, TokenParamKind::MaxCompletionTokens);
    assert_eq!(cfg.openai_tool_choice, Some(serde_json::json!("auto")));
    assert_eq!(
        cfg.openai_extra_body["enable_thinking"],
        serde_json::json!(true)
    );
    assert_eq!(cfg.skills, vec!["runtime-rust"]);
    assert_eq!(cfg.mission_content.as_deref(), Some("mission"));
    assert_eq!(
        cfg.enabled_tools,
        Some(vec!["Read".to_string(), "Bash".to_string()])
    );
    assert_eq!(cfg.tool_approval_mode, ToolApprovalMode::Write);
    assert_eq!(
        cfg.llm_recovery,
        crate::config::LlmRecoveryPolicy {
            format_window_size: 5,
            format_max_errors: 2,
            request_max_retries: 1,
            request_timeout_secs: Some(30),
        }
    );
    // per-tool approval 的 options 便捷方法已删除（零外部调用），
    // 嵌入方经 Config/CLI [tools.approval] 配置。
}

#[test]
fn options_llm_recovery_defaults_are_the_single_runtime_defaults() {
    // An untouched AgentOptions carries the shared Rust defaults (the
    // other entry points override only the fields they set).
    let runtime_config = AgentOptions::new("/tmp/mink-home", "/tmp/project").into_runtime_config();
    let policy = runtime_config.config.llm_recovery;
    assert_eq!(policy, crate::config::LlmRecoveryPolicy::default());
    assert_eq!(policy.format_window_size, 10);
    assert_eq!(policy.format_max_errors, 3);
    assert_eq!(policy.request_max_retries, 3);
    assert_eq!(policy.request_timeout_secs, None);
}

#[test]
fn options_use_typed_runtime_controls() {
    let runtime_config = AgentOptions::new("/tmp/mink-home", "/tmp/project")
        .with_first_prompt("metadata")
        .with_session(SessionPolicy::UseOrCreate("typed-session".into()))
        .into_runtime_config();

    assert_eq!(runtime_config.first_prompt.as_deref(), Some("metadata"));
    assert!(matches!(
        runtime_config.session,
        SessionPolicy::UseOrCreate(ref value) if value == "typed-session"
    ));
}

#[test]
fn options_explicit_first_prompt_is_metadata_only() {
    let runtime_config = AgentOptions::new("/tmp/mink-home", "/tmp/project")
        .with_first_prompt("metadata only")
        .into_runtime_config();

    assert!(runtime_config.config.prompt.is_empty());
    assert_eq!(
        runtime_config.first_prompt.as_deref(),
        Some("metadata only")
    );
}

#[test]
fn options_preserve_session_selection_semantics() {
    let runtime_config = AgentOptions::new("/tmp/mink-home", "/tmp/project")
        .with_session(SessionPolicy::UseOrCreate("existing-or-alias".into()))
        .into_runtime_config();
    assert!(matches!(
        runtime_config.session,
        SessionPolicy::UseOrCreate(ref value) if value == "existing-or-alias"
    ));

    let runtime_config = AgentOptions::new("/tmp/mink-home", "/tmp/project")
        .with_session(SessionPolicy::ContinueLatest)
        .into_runtime_config();
    assert!(matches!(
        runtime_config.session,
        SessionPolicy::ContinueLatest
    ));
}

#[test]
fn options_explicit_session_is_authoritative() {
    let runtime_config = AgentOptions::new("/tmp/mink-home", "/tmp/project")
        .with_session(SessionPolicy::New)
        .into_runtime_config();

    assert!(matches!(runtime_config.session, SessionPolicy::New));
}

#[test]
fn options_can_override_session_layout() {
    let runtime_config = AgentOptions::new("/tmp/mink-home", "/tmp/project")
        .with_home_scoped_sessions()
        .into_runtime_config();
    assert_eq!(runtime_config.session_layout, SessionLayout::HomeScoped);

    let runtime_config = AgentOptions::new("/tmp/mink-home", "/tmp/project")
        .with_project_scoped_sessions()
        .into_runtime_config();
    assert_eq!(runtime_config.session_layout, SessionLayout::ProjectScoped);

    let runtime_config = AgentOptions::new("/tmp/mink-home", "/tmp/project")
        .with_direct_sessions()
        .into_runtime_config();
    assert_eq!(runtime_config.session_layout, SessionLayout::Direct);
}

#[test]
fn options_individual_ptc_builders_and_validation_support_embedded_hosts() {
    struct NoopCustomTool;
    #[async_trait::async_trait]
    impl crate::runtime::AgentTool for NoopCustomTool {
        fn definition(&self) -> crate::runtime::ToolDefinition {
            crate::runtime::ToolDefinition::new(
                "DomainQuery",
                "Query domain records.",
                serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            )
            .with_ptc_exposure(crate::runtime::ToolPtcExposure::ProgrammaticOnly)
        }

        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: crate::runtime::ToolExecutionContext,
        ) -> std::result::Result<crate::runtime::ToolOutput, crate::runtime::ToolError> {
            Ok(crate::runtime::ToolOutput::text("ok"))
        }
    }

    struct NoopPtcRuntime;
    #[async_trait::async_trait]
    impl crate::ptc::PtcRuntime for NoopPtcRuntime {
        fn kind(&self) -> crate::ptc::PtcBackendKind {
            crate::ptc::PtcBackendKind::WasiPython
        }
        fn resolve(
            &self,
            req: crate::ptc::PtcRunRequest,
        ) -> anyhow::Result<crate::ptc::PtcRunSpec> {
            crate::ptc::PtcConfig::default().resolve_run_spec(req)
        }
        async fn run(
            &self,
            _spec: crate::ptc::PtcRunSpec,
            _dispatcher: Arc<dyn crate::ptc::PtcBindingDispatcher>,
        ) -> anyhow::Result<crate::ptc::PtcRunResult> {
            Ok(crate::ptc::PtcRunResult {
                output: Vec::new(),
                output_truncated: false,
                error: None,
            })
        }
    }

    let options = AgentOptions::new("/tmp/mink-home", "/tmp/project")
        .with_ptc_mode(crate::ptc::PtcMode::Tool)
        .with_ptc_backend(crate::ptc::PtcBackendKind::WasiPython)
        .with_ptc_python_executable("/usr/bin/python3")
        .with_ptc_timeout_secs(90)
        .with_ptc_max_timeout_secs(900)
        .with_ptc_max_parallel_sub_calls(3)
        .with_ptc_max_sub_calls_per_minute(180)
        .with_ptc_max_output_bytes(65_536)
        .with_ptc_max_message_bytes(32_768)
        .with_ptc_max_pending_calls(16)
        .with_ptc_stop_grace_ms(500)
        .with_ptc_runtime(Arc::new(NoopPtcRuntime))
        .with_tool(Arc::new(NoopCustomTool))
        .with_enabled_tools(["DomainQuery".to_string()]);

    options
        .validate_runtime_config()
        .expect("custom tool in enabled_tools and custom PtcRuntime must validate cleanly");

    let runtime_config = options.into_runtime_config();
    let ptc = &runtime_config.config.ptc;
    assert_eq!(ptc.mode, crate::ptc::PtcMode::Tool);
    assert_eq!(ptc.backend, crate::ptc::PtcBackendKind::WasiPython);
    assert_eq!(ptc.python_executable.as_deref(), Some("/usr/bin/python3"));
    assert_eq!(ptc.timeout_secs, 90);
    assert_eq!(ptc.max_timeout_secs, 900);
    assert_eq!(ptc.max_parallel_sub_calls, 3);
    assert_eq!(ptc.max_sub_calls_per_minute, 180);
    assert_eq!(ptc.max_output_bytes, 65_536);
    assert_eq!(ptc.max_message_bytes, 32_768);
    assert_eq!(ptc.max_pending_calls, 16);
    assert_eq!(ptc.stop_grace_ms, 500);
    assert!(runtime_config.ptc_runtime.is_some());
}
