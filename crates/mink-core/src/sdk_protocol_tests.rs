use super::*;

#[test]
fn sdk_request_defaults_version_and_options() {
    let req: SdkRequest = serde_json::from_str(r#"{"prompt":"hi"}"#).unwrap();
    assert_eq!(req.version, Some(PROTOCOL_VERSION));
    assert_eq!(req.prompt, "hi");
    assert_eq!(req.options.session.session_layout, None);
    assert_eq!(req.options.tools.skills, None);
    assert_eq!(req.options.tools.inline_skills, None);
    assert_eq!(req.options.tools.skill_discovery_policy, None);
    assert_eq!(req.options.context.max_context, None);
    assert_eq!(req.options.context.context_compact_pct, None);
}

#[test]
fn sdk_request_accepts_session_layout() {
    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"session":{"session_layout":"home"}}}"#,
    )
    .unwrap();
    assert_eq!(
        req.options.session.session_layout,
        Some(SessionLayout::HomeScoped)
    );

    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"session":{"session_layout":"isolated"}}}"#,
    )
    .unwrap();
    assert_eq!(
        req.options.session.session_layout,
        Some(SessionLayout::Isolated)
    );
}

#[test]
fn sdk_request_accepts_selected_skills() {
    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"tools":{"skills":["debugging","verification"]}}}"#,
    )
    .unwrap();
    assert_eq!(
        req.options.tools.skills,
        Some(vec!["debugging".to_string(), "verification".to_string()])
    );
}

#[test]
fn sdk_request_accepts_inline_skills_and_policy() {
    let req = parse_agent_jsonl_request(
        r#"{
                "prompt":"hi",
                "options":{
                    "tools":{
                        "inline_skills":[{
                            "name":"company-policy",
                            "description":"Company policy",
                            "content":"private policy",
                            "exposure":"model_addressable",
                            "revision":"rev-1"
                        }],
                        "skill_discovery_policy":"runtime_only"
                    }
                }
            }"#,
    )
    .unwrap();
    let inline = req.options.tools.inline_skills.as_ref().unwrap();
    assert_eq!(inline[0].name, "company-policy");
    assert_eq!(
        inline[0].exposure,
        Some(SdkCapabilityExposure::ModelAddressable)
    );
    assert_eq!(
        req.options.tools.skill_discovery_policy,
        Some(SdkSkillDiscoveryPolicy::RuntimeOnly)
    );
}

#[test]
fn validate_sdk_request_rejects_invalid_inline_skill() {
    let req = parse_agent_jsonl_request(
            r#"{"prompt":"hi","options":{"tools":{"inline_skills":[{"name":"../secret","content":"x"}]}}}"#,
        )
        .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("invalid inline skill name"), "{err}");

    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"tools":{"skills":[" debugging"]}}}"#,
    )
    .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("invalid skill name"), "{err}");

    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"tools":{"inline_skills":[{"name":"empty","content":""}]}}}"#,
    )
    .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("content must be non-empty"), "{err}");
}

#[test]
fn sdk_request_rejects_unknown_session_layout() {
    let err = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"session":{"session_layout":"workspace"}}}"#,
    )
    .unwrap_err();
    assert!(err.contains("workspace"));
}

#[test]
fn parse_agent_jsonl_request_rejects_missing_prompt() {
    let err = parse_agent_jsonl_request(r#"{"version":2}"#).unwrap_err();
    assert!(err.contains("missing required field prompt"));
}

#[test]
fn parse_agent_jsonl_request_rejects_non_string_prompt() {
    let err = parse_agent_jsonl_request(r#"{"version":2,"prompt":123}"#).unwrap_err();
    assert!(err.contains("prompt must be a string"));
}

#[test]
fn validate_sdk_request_rejects_bad_numeric_options() {
    let req =
        parse_agent_jsonl_request(r#"{"prompt":"hi","options":{"generation":{"max_tokens":0}}}"#)
            .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("max_tokens must be greater than 0"));

    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"context":{"context_compact_pct":0}}}"#,
    )
    .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("context_compact_pct must be between 1 and 100"));

    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"context":{"context_reserve_tokens":0}}}"#,
    )
    .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("context_reserve_tokens must be greater than 0"));

    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"context":{"context_compact_tail_tokens":0}}}"#,
    )
    .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("context_compact_tail_tokens must be greater than 0"));

    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"context":{"context_compact_max_output_tokens":0}}}"#,
    )
    .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("context_compact_max_output_tokens must be greater than 0"));
}

#[test]
fn sdk_request_accepts_explicit_compaction_policy() {
    let req = parse_agent_jsonl_request(
        r#"{
                "prompt":"hi",
                "options":{
                    "context":{
                        "max_context":64000,
                        "context_compact_pct":65,
                        "context_reserve_tokens":12000,
                        "context_compact_tail_tokens":16000,
                        "context_compact_max_output_tokens":4096,
                        "context_compact_input_reduction":true
                    }
                }
            }"#,
    )
    .unwrap();
    validate_sdk_request(&req).unwrap();
    assert_eq!(req.options.context.max_context, Some(64_000));
    assert_eq!(req.options.context.context_compact_pct, Some(65));
    assert_eq!(req.options.context.context_reserve_tokens, Some(12_000));
    assert_eq!(
        req.options.context.context_compact_tail_tokens,
        Some(16_000)
    );
    assert_eq!(
        req.options.context.context_compact_max_output_tokens,
        Some(4_096)
    );
    assert_eq!(
        req.options.context.context_compact_input_reduction,
        Some(true)
    );
}

#[test]
fn sdk_request_rejects_removed_plan_projection_tail() {
    let error = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"context":{"plan_projection_tail":false}}}"#,
    )
    .unwrap_err();
    assert!(error.contains("plan_projection_tail"), "{error}");
}

#[test]
fn validate_sdk_request_rejects_bad_tool_timeout_options() {
    let req =
        parse_agent_jsonl_request(r#"{"prompt":"hi","options":{"tools":{"tool_timeout":0}}}"#)
            .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("tool_timeout must be greater than 0"), "{err}");

    let req =
        parse_agent_jsonl_request(r#"{"prompt":"hi","options":{"tools":{"tool_timeout_max":4}}}"#)
            .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("tool_timeout_max must be at least 5"), "{err}");

    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"tools":{"tool_timeout":30,"tool_timeout_max":900}}}"#,
    )
    .unwrap();
    validate_sdk_request(&req).unwrap();
    assert_eq!(req.options.tools.tool_timeout, Some(30));
    assert_eq!(req.options.tools.tool_timeout_max, Some(900));
}

#[test]
fn validate_sdk_request_rejects_bad_llm_timeout_options() {
    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"generation":{"llm_idle_timeout":0}}}"#,
    )
    .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("llm_idle_timeout must be greater than 0"));

    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"generation":{"llm_wait_heartbeat":-1}}}"#,
    )
    .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("llm_wait_heartbeat must be zero or greater"));
}

#[test]
fn validate_sdk_request_accepts_custom_model() {
    let req =
        parse_agent_jsonl_request(r#"{"prompt":"hi","options":{"provider":{"model":"gpt-4"}}}"#)
            .unwrap();
    validate_sdk_request(&req).unwrap();
}

#[test]
fn validate_sdk_request_rejects_empty_model() {
    let req = parse_agent_jsonl_request(r#"{"prompt":"hi","options":{"provider":{"model":" "}}}"#)
        .unwrap();
    let err = validate_sdk_request(&req).unwrap_err();
    assert!(err.contains("model must not be empty"));
}

#[test]
fn sdk_request_rejects_flat_legacy_options() {
    let error = parse_agent_jsonl_request(r#"{"prompt":"hi","options":{"max_context":64000}}"#)
        .unwrap_err();
    assert!(error.contains("unknown field `max_context`"), "{error}");
}

#[test]
fn sdk_recovery_options_are_parsed_validated_and_defaulted() {
    // JSONL `options.recovery` is an optional group; absent fields keep
    // the single Rust-side defaults and unknown fields are rejected.
    let plain = parse_agent_jsonl_request(r#"{"prompt":"hi"}"#).unwrap();
    assert!(plain.options.recovery.format_window_size.is_none());
    assert!(plain.options.recovery.request_max_retries.is_none());

    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"recovery":{"format_window_size":5,"format_max_errors":2,"request_max_retries":1,"request_timeout_secs":30}}}"#,
    )
    .unwrap();
    validate_sdk_request(&req).unwrap();
    assert_eq!(req.options.recovery.format_window_size, Some(5));
    assert_eq!(req.options.recovery.format_max_errors, Some(2));
    assert_eq!(req.options.recovery.request_max_retries, Some(1));
    assert_eq!(req.options.recovery.request_timeout_secs, Some(30));

    let error = parse_agent_jsonl_request(r#"{"prompt":"hi","options":{"recovery":{"bogus":1}}}"#)
        .unwrap_err();
    assert!(error.contains("unknown field `bogus`"), "{error}");
}

#[test]
fn sdk_recovery_options_fail_closed_on_invalid_values() {
    // Invalid W/K/retries/timeout are rejected before session creation.
    let cases = [
        (
            r#"{"prompt":"hi","options":{"recovery":{"format_window_size":0}}}"#,
            "format_window_size",
        ),
        (
            r#"{"prompt":"hi","options":{"recovery":{"format_window_size":1025}}}"#,
            "format_window_size",
        ),
        (
            r#"{"prompt":"hi","options":{"recovery":{"format_window_size":4,"format_max_errors":4}}}"#,
            "format_max_errors",
        ),
        (
            r#"{"prompt":"hi","options":{"recovery":{"request_max_retries":17}}}"#,
            "request_max_retries",
        ),
        (
            r#"{"prompt":"hi","options":{"recovery":{"request_timeout_secs":0}}}"#,
            "request_timeout_secs",
        ),
    ];
    for (body, expected) in cases {
        let req = parse_agent_jsonl_request(body).unwrap();
        let error = validate_sdk_request(&req).unwrap_err();
        assert!(error.contains(expected), "{body}: {error}");
    }
    // K=0 with an explicit window is valid (no format budget).
    let req = parse_agent_jsonl_request(
        r#"{"prompt":"hi","options":{"recovery":{"format_window_size":1,"format_max_errors":0}}}"#,
    )
    .unwrap();
    validate_sdk_request(&req).unwrap();
}
