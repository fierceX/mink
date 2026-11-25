use super::*;

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn merge_overrides_present_fields_and_keeps_absent_ones() {
        let base = AgentConfig {
            model: Some("base-model".into()),
            enabled_tools: Some(vec!["Read".into()]),
            ..Default::default()
        };
        let over = AgentConfig {
            model: Some("over-model".into()),
            ..Default::default()
        };

        let merged = merge(base, over);

        assert_eq!(merged.model.as_deref(), Some("over-model"));
        assert_eq!(merged.enabled_tools, Some(vec!["Read".into()]));
    }

    #[test]
    fn merge_container_semantics_are_explicit() {
        let base = AgentConfig {
            enabled_tools: Some(vec!["Read".into()]),
            model_aliases: [("a".to_string(), "base".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let over = AgentConfig {
            // An explicit empty list replaces the base selection (it does not
            // mean "keep base"), matching CLI layering.
            enabled_tools: Some(Vec::new()),
            model_aliases: [("b".to_string(), "over".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let merged = merge(base, over);

        assert_eq!(merged.enabled_tools, Some(Vec::new()));
        assert_eq!(
            merged.model_aliases.get("a").map(String::as_str),
            Some("base")
        );
        assert_eq!(
            merged.model_aliases.get("b").map(String::as_str),
            Some("over")
        );
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    fn parse_layer(text: &str, origin: &str) -> AgentConfig {
        super::parse_layer(text, origin).expect("valid test configuration")
    }

    #[test]
    fn grouped_config_parses_all_sections() {
        let layer = parse_layer(
            r#"
[provider]
model = "pro"
api_key = "sk-test"
base_url = "https://example.invalid/v1"
openai_reasoning_effort = "low"
openai_include_usage = false
openai_token_param = "max_completion_tokens"
model_aliases = { fast = "flash" }
openai_extra_body = { temperature = 0.2 }

[generation]
max_tokens = 4096
max_turns = 8
llm_first_event_timeout = 30

[context]
max_context = "120k"
context_compact_pct = 80
context_reserve_tokens = 2048

[tools]
enabled_tools = ["Read", "Edit", "Bash"]
approval_mode = "write"
tool_timeout = 120
sub_agent_max_depth = 2
skills = ["python"]
max_search_files = 100

[tools.edit]
mode = "replace"
fuzzy_threshold = 0.9

[signal]
policy = "evidence"

[sandbox]
enabled = true
allow_network = false

[sandbox_python]
timeout = 60
"#,
            "test",
        );
        assert_eq!(layer.model.as_deref(), Some("pro"));
        assert_eq!(layer.api_key.as_deref(), Some("sk-test"));
        assert_eq!(
            layer.base_url.as_deref(),
            Some("https://example.invalid/v1")
        );
        assert_eq!(layer.openai_reasoning_effort.as_deref(), Some("low"));
        assert_eq!(layer.openai_include_usage, Some(false));
        assert_eq!(
            layer.openai_token_param,
            Some(TokenParamKind::MaxCompletionTokens)
        );
        assert_eq!(
            layer.model_aliases.get("fast").map(String::as_str),
            Some("flash")
        );
        assert_eq!(
            layer
                .openai_extra_body
                .get("temperature")
                .and_then(Value::as_f64),
            Some(0.2)
        );
        assert_eq!(layer.max_tokens, Some(4096));
        assert_eq!(layer.max_turns, Some(8));
        assert_eq!(layer.llm_first_event_timeout_secs, Some(30));
        assert_eq!(layer.max_context_tokens, Some(120_000));
        assert_eq!(layer.context_compact_pct, Some(80));
        assert_eq!(layer.context_reserve_tokens, Some(2048));
        assert_eq!(
            layer.enabled_tools.as_deref(),
            Some(&["Read".to_string(), "Edit".to_string(), "Bash".to_string()][..])
        );
        assert_eq!(layer.approval_mode, Some(ToolApprovalMode::Write));
        assert_eq!(layer.tool_timeout_secs, Some(120));
        assert_eq!(layer.sub_agent_max_depth, Some(2));
        assert_eq!(layer.skills.as_deref(), Some(&["python".to_string()][..]));
        assert_eq!(layer.edit_mode, Some(EditMode::Replace));
        assert_eq!(layer.edit_fuzzy_threshold, Some(0.9));
        assert_eq!(layer.signal_policy, Some(SignalPolicy::Evidence));
        assert_eq!(layer.sandbox.enabled, Some(true));
        assert_eq!(layer.sandbox.allow_network, Some(false));
        assert_eq!(layer.sandbox_python.timeout, Some(60));
    }

    #[test]
    fn invalid_subagent_limits_are_preserved_for_runtime_validation() {
        let layer = parse_layer(
            "[tools]\nsub_agent_max_depth = 4\nsub_agent_timeout = 0\n",
            "test",
        );
        assert_eq!(layer.sub_agent_max_depth, Some(4));
        assert_eq!(layer.sub_agent_timeout_secs, Some(0));
    }

    #[test]
    fn invalid_subagent_limits_are_rejected_by_runtime_validation() {
        let cfg = AgentConfig {
            sub_agent_max_depth: Some(4),
            sub_agent_timeout_secs: Some(0),
            ..AgentConfig::default()
        };
        let options = apply_to(
            AgentOptions::new("/tmp/mink-server-config", "/tmp/mink-server-config"),
            &cfg,
        );
        assert!(options.validate_runtime_config().is_err());
    }

    #[test]
    fn flat_keys_are_rejected_like_the_cli() {
        let error = super::parse_layer("model = \"pro\"\n", "test").unwrap_err();
        assert!(error.to_string().contains("model"));
    }

    #[test]
    fn invalid_layer_is_rejected_instead_of_dropping_tool_restrictions() {
        let error = super::parse_layer("[tools]\nenabled_tools = []\nunknown_key = true\n", "test")
            .unwrap_err();
        assert!(error.to_string().contains("unknown_key"));
    }

    #[test]
    fn layer_loader_ignores_only_missing_files() {
        let root = std::env::temp_dir().join(format!("mink-agent-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let missing = root.join("missing.minkrc");
        assert!(load_layer(&missing).unwrap().model.is_none());
        assert!(
            load_layer(&root).is_err(),
            "reading a directory must not look like a missing config"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_image_keys_do_not_reject_the_whole_file() {
        // Regression: the multimodal `[provider]` keys were missing from this
        // replica, so `deny_unknown_fields` rejected every `.minkrc` that used
        // them as a whole and the server silently fell back to defaults.
        let layer = parse_layer(
            r#"
[provider]
model = "pro"
image_input = "on"
vision_models = ["custom-vision"]

[provider.image]
detail = "low"
max_images_per_request = 3
"#,
            "test",
        );
        assert_eq!(layer.model.as_deref(), Some("pro"));
        assert!(matches!(
            layer.image_input,
            Some(ImageInputCapability::OpenAiChatImageUrl(_))
        ));
        assert_eq!(
            layer.vision_models.as_deref(),
            Some(&["custom-vision".to_string()][..])
        );
        assert_eq!(
            layer.image_limits.as_ref().and_then(|limits| limits.detail),
            Some(ImageDetail::Low)
        );
    }

    #[test]
    fn image_config_parses_sizes_and_warns_off_invalid_values() {
        let layer = parse_layer(
            r#"
[provider.image]
max_image_bytes_per_request = "32M"
max_image_bytes = "8M"
max_dimension = 4096
max_pixels = "4M"
"#,
            "test",
        );
        let limits = layer.image_limits.expect("image limits");
        assert_eq!(limits.max_image_bytes_per_request, Some(32_000_000));
        assert_eq!(limits.max_image_bytes, Some(8_000_000));
        assert_eq!(limits.max_dimension, Some(4096));
        assert_eq!(limits.max_pixels, Some(4_000_000));
        assert_eq!(limits.max_images_per_request, None);

        // Invalid values warn and drop only the affected item; the rest of
        // the file keeps loading (same stance as the CLI).
        let layer = parse_layer(
            "[provider]\nmodel = \"pro\"\nimage_input = \"maybe\"\n\n[provider.image]\ndetail = \"ultra\"\n",
            "test",
        );
        assert_eq!(layer.model.as_deref(), Some("pro"));
        assert_eq!(layer.image_input, None);
        assert_eq!(layer.image_limits, None);

        let layer = parse_layer("[provider]\nimage_input = \"off\"\n", "test");
        assert_eq!(layer.image_input, Some(ImageInputCapability::Unsupported));
    }

    #[test]
    fn image_sections_merge_like_the_cli() {
        // `image_input` / `vision_models` are whole-value overrides; the
        // `[provider.image]` table merges per field across layers.
        let user = parse_layer(
            "[provider]\nimage_input = \"on\"\n\n[provider.image]\nmax_images_per_request = 10\n",
            "user",
        );
        let project = parse_layer(
            "[provider]\nimage_input = \"off\"\n\n[provider.image]\ndetail = \"low\"\n",
            "project",
        );
        let merged = merge(user, project);
        assert_eq!(merged.image_input, Some(ImageInputCapability::Unsupported));
        let limits = merged.image_limits.expect("merged image limits");
        assert_eq!(limits.max_images_per_request, Some(10));
        assert_eq!(limits.detail, Some(ImageDetail::Low));
    }

    #[test]
    fn image_env_overrides_are_applied_on_top() {
        let _guard = crate::session::TEST_ENV_LOCK.blocking_lock();
        let mut cfg = parse_layer("[provider]\nimage_input = \"off\"\n", "test");
        unsafe { std::env::set_var("MINK_IMAGE_INPUT", "on") };
        unsafe { std::env::set_var("MINK_VISION_MODELS", "a, b ,") };
        apply_env_overrides(&mut cfg);
        unsafe { std::env::remove_var("MINK_IMAGE_INPUT") };
        unsafe { std::env::remove_var("MINK_VISION_MODELS") };
        assert!(matches!(
            cfg.image_input,
            Some(ImageInputCapability::OpenAiChatImageUrl(_))
        ));
        assert_eq!(
            cfg.vision_models.as_deref(),
            Some(&["a".to_string(), "b".to_string()][..])
        );
    }

    #[test]
    fn removed_plan_projection_tail_is_rejected() {
        let error = toml::from_str::<MinkConfigFile>("[context]\nplan_projection_tail = false\n")
            .unwrap_err();
        assert!(error.to_string().contains("plan_projection_tail"));
    }

    #[test]
    fn project_layer_overrides_user_layer() {
        let user = parse_layer(
            "[provider]\nmodel = \"user-model\"\napi_key = \"user-key\"\n\n[tools]\ntool_timeout = 10\n",
            "user",
        );
        let project = parse_layer(
            "[provider]\nmodel = \"project-model\"\n\n[tools]\ntool_timeout = 20\n",
            "project",
        );
        let merged = merge(user, project);
        assert_eq!(merged.model.as_deref(), Some("project-model"));
        assert_eq!(
            merged.api_key.as_deref(),
            Some("user-key"),
            "absent project fields keep user values"
        );
        assert_eq!(merged.tool_timeout_secs, Some(20));
        // Section fields merge per-field too.
        let user = parse_layer("[sandbox]\nenabled = true\n", "user");
        let project = parse_layer("[sandbox]\nallow_network = false\n", "project");
        let merged = merge(user, project);
        assert_eq!(merged.sandbox.enabled, Some(true));
        assert_eq!(merged.sandbox.allow_network, Some(false));
    }

    #[test]
    fn env_overrides_are_applied_on_top() {
        let _guard = crate::session::TEST_ENV_LOCK.blocking_lock();
        let mut cfg = parse_layer(
            "[provider]\nmodel = \"file-model\"\napi_key = \"file-key\"\n",
            "test",
        );
        unsafe { std::env::set_var("MODEL", "env-model") };
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "env-key") };
        unsafe { std::env::set_var("DEEPSEEK_BASE_URL", "https://env.invalid/v1") };
        apply_env_overrides(&mut cfg);
        unsafe { std::env::remove_var("MODEL") };
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
        unsafe { std::env::remove_var("DEEPSEEK_BASE_URL") };
        assert_eq!(cfg.model.as_deref(), Some("env-model"));
        assert_eq!(cfg.api_key.as_deref(), Some("env-key"));
        assert_eq!(cfg.base_url.as_deref(), Some("https://env.invalid/v1"));
    }

    #[test]
    fn provider_http_timeout_merges_per_field_and_parses_file_layer() {
        // server 的 provider http_timeout 合并语义：项目层优先、缺省保留上层，
        // 0 是合法值（不设总超时），不能被当成 None。
        let user = parse_layer("[provider]\nhttp_timeout_secs = 0\n", "user");
        let project = parse_layer("[provider]\nhttp_timeout_secs = 1200\n", "project");
        assert_eq!(user.http_timeout_secs, Some(0));
        let merged = merge(user, project);
        assert_eq!(merged.http_timeout_secs, Some(1200), "project layer wins");

        let user_only = parse_layer("[provider]\nhttp_timeout_secs = 0\n", "user");
        let merged_absent = merge(user_only, parse_layer("", "project"));
        assert_eq!(
            merged_absent.http_timeout_secs,
            Some(0),
            "an absent project field keeps the user value"
        );

        // 文件层解析：Some(0) 必须保留为 Some(0)（0 = 不设总超时），
        // 未配置时为 None（由 Rust 默认 7200 接管）。
        let file: MinkConfigFile = toml::from_str("[provider]\nhttp_timeout_secs = 0\n").unwrap();
        assert_eq!(from_file(&file).http_timeout_secs, Some(0));
        let none: MinkConfigFile = toml::from_str("[provider]\nmodel = \"flash\"\n").unwrap();
        assert_eq!(from_file(&none).http_timeout_secs, None);
    }

    #[test]
    fn size_suffixes_match_cli_semantics() {
        assert_eq!(parse_size_bytes("120k").unwrap(), 120_000);
        assert_eq!(parse_size_bytes("2m").unwrap(), 2_000_000);
        assert_eq!(parse_size_bytes("1g").unwrap(), 1_000_000_000);
        assert_eq!(parse_size_bytes("4096").unwrap(), 4096);
        assert!(parse_size_bytes("").is_err());
    }

    #[test]
    fn recovery_section_parses_and_merges_per_field() {
        // A01: the server reads the same `[recovery]` section as the CLI and
        // merges it per field (higher layer wins, absent fields keep defaults).
        let user = parse_layer(
            "[recovery]\nformat_window_size = 5\nrequest_max_retries = 0\n",
            "user",
        );
        let project = parse_layer("[recovery]\nformat_max_errors = 2\n", "project");
        assert_eq!(user.recovery.format_window_size, Some(5));
        assert_eq!(user.recovery.request_max_retries, Some(0));
        let merged = merge(user, project);
        assert_eq!(merged.recovery.format_window_size, Some(5));
        assert_eq!(merged.recovery.format_max_errors, Some(2));
        assert_eq!(merged.recovery.request_max_retries, Some(0));
        assert_eq!(merged.recovery.request_timeout_secs, None);

        // A02: unknown fields inside the section fail closed at parse time.
        let error = toml::from_str::<MinkConfigFile>("[recovery]\nbogus = 1\n").unwrap_err();
        assert!(error.to_string().contains("bogus"));
    }

    #[test]
    fn ptc_section_parses_merges_per_field_and_applies() {
        let user = parse_layer(
            "[ptc]\nmode = \"tool\"\nmax_parallel_sub_calls = 4\n",
            "user",
        );
        let project = parse_layer("[ptc]\nmode = \"exclusive\"\n", "project");
        let merged = merge(user, project);
        assert_eq!(merged.ptc.mode.as_deref(), Some("exclusive"));
        assert_eq!(merged.ptc.max_parallel_sub_calls, Some(4));

        let opts = apply_to(AgentOptions::new("/tmp/mink-test-home", "/tmp"), &merged);
        assert_eq!(opts.ptc().mode, PtcMode::Exclusive);
        assert_eq!(opts.ptc().max_parallel_sub_calls, 4);
    }
}
