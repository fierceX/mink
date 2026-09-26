use super::*;

/// Parsed arguments of one recovered candidate.
fn args_of(info: &ToolCallInfo) -> Value {
    serde_json::from_str(&info.arguments).unwrap_or(Value::Null)
}

// ---- Existing scavenge tests ----

#[test]
fn scavenge_xml_style() {
    let text = r#"I'll update it <tool_call>{"name":"Edit","arguments":{"path":"src/a.rs","patch":"@src/a.rs#0A3B\nreplace 2:\n+new()"}}</tool_call> now"#;
    let result = scavenge_tool_calls(text).unwrap();
    assert_eq!(result[0].name, "Edit");
    assert_eq!(
        args_of(&result[0])["patch"],
        "@src/a.rs#0A3B\nreplace 2:\n+new()"
    );
}

#[test]
fn scavenge_bracket_style() {
    let text = r#"Let me check [TOOL_CALL]{"name":"Read","arguments":{"path":"/tmp/x:10-20"}}[/TOOL_CALL]"#;
    let result = scavenge_tool_calls(text).unwrap();
    assert_eq!(result[0].name, "Read");
    assert_eq!(args_of(&result[0])["path"], "/tmp/x:10-20");
}

#[test]
fn scavenge_bare_json() {
    let text = r#"Here: {"name":"Read","arguments":{"path":"artifact://bash-0001:1-20"}}"#;
    let result = scavenge_tool_calls(text).unwrap();
    assert_eq!(result[0].name, "Read");
    assert_eq!(args_of(&result[0])["path"], "artifact://bash-0001:1-20");
}

#[test]
fn no_tool_call_returns_none() {
    let text = "No tool calls here.";
    assert!(scavenge_tool_calls(text).is_none());
}

#[test]
fn escape_hatches_fallback() {
    let text = r#"some text and then {"name": "Read", "arguments": {"path": "skill://debugging"}} trailing"#;
    let result = scavenge_tool_calls(text);
    assert!(result.is_some());
}

// ---- New: DSML ----

#[test]
fn scavenge_dsml_half_width() {
    let text = r#"<|DSML|invoke name="Read">
<|DSML|parameter name="path" string="true">/tmp/x.txt<|DSML|parameter>
</|DSML|invoke>"#;
    let result = scavenge_tool_calls(text).unwrap();
    assert_eq!(result[0].name, "Read");
    assert_eq!(args_of(&result[0])["path"], "/tmp/x.txt");
}

#[test]
fn scavenge_dsml_full_width() {
    let text = r#"<|DSML|invoke name="Grep">
<|DSML|parameter name="pattern" string="true">foo<|DSML|parameter>
</|DSML|invoke>"#;
    let result = scavenge_tool_calls(text).unwrap();
    assert_eq!(result[0].name, "Grep");
}

#[test]
fn scavenge_dsml_with_json_param() {
    let text = r#"<|DSML|invoke name="Edit">
<|DSML|parameter name="path" string="true">/tmp/x<|DSML|parameter>
<|DSML|parameter name="patch" string="true">@/tmp/x#0A3B
replace 1:
+new<|DSML|parameter>
</|DSML|invoke>"#;
    let result = scavenge_tool_calls(text).unwrap();
    assert_eq!(result[0].name, "Edit");
    assert_eq!(args_of(&result[0])["path"], "/tmp/x");
    assert!(
        args_of(&result[0])["patch"]
            .as_str()
            .unwrap()
            .contains("replace 1")
    );
}

// ---- New: 3-shape coerce ----

#[test]
fn coerce_openai_style() {
    let v: Value = serde_json::from_str(
        r#"{"type":"function","function":{"name":"Read","arguments":"{\"path\":\"/x\"}"}}"#,
    )
    .unwrap();
    let result = coerce_to_tool_call(&v);
    assert!(result.is_some());
    let r = result.unwrap();
    assert_eq!(r.name, "Read");
    assert_eq!(args_of(&r)["path"], "/x");
}

#[test]
fn coerce_openai_style_rejects_unparsable_arguments() {
    // A declared-but-unparsable `function.arguments` string must keep
    // the raw payload and the parse error — never become an empty object.
    let v: Value = serde_json::from_str(
        r#"{"type":"function","function":{"name":"TodoRead","arguments":"not-json"}}"#,
    )
    .unwrap();
    let info = coerce_to_tool_call(&v).expect("the candidate shape is recognized");
    assert_eq!(info.name, "TodoRead");
    assert_eq!(info.arguments, "not-json");
    let error = info.parse_error.expect("parse failure is preserved");
    assert!(error.contains("parse function arguments"), "{error}");
}

#[test]
fn coerce_openai_style_rejects_non_string_arguments() {
    // A present-but-wrong-typed `function.arguments` is a type
    // error (never silently `{}`), and the raw payload is preserved.
    let v: Value = serde_json::from_str(
        r#"{"type":"function","function":{"name":"TodoRead","arguments":false}}"#,
    )
    .unwrap();
    let info = coerce_to_tool_call(&v).expect("the candidate shape is recognized");
    assert_eq!(info.name, "TodoRead");
    assert_eq!(info.arguments, "false");
    let error = info.parse_error.expect("type error is preserved");
    assert!(error.contains("must be a JSON string"), "{error}");
    assert!(error.contains("boolean"), "{error}");

    // A genuinely missing field keeps the (empty) default: that is not an
    // error, the provider simply declared no arguments.
    let v: Value =
        serde_json::from_str(r#"{"type":"function","function":{"name":"TodoRead"}}"#).unwrap();
    let info = coerce_to_tool_call(&v).expect("shape is recognized");
    assert!(info.parse_error.is_none());
    assert_eq!(info.arguments, "{}");
}

#[test]
fn coerce_tool_name_style() {
    let v: Value =
        serde_json::from_str(r#"{"tool_name":"Read","tool_args":{"path":"session://current"}}"#)
            .unwrap();
    let result = coerce_to_tool_call(&v);
    assert!(result.is_some());
    let result = result.unwrap();
    assert_eq!(result.name, "Read");
    assert_eq!(args_of(&result)["path"], "session://current");
}

#[test]
fn coerce_standard_style() {
    let v: Value =
        serde_json::from_str(r#"{"name":"Glob","arguments":{"pattern":"*.rs"}}"#).unwrap();
    let result = coerce_to_tool_call(&v);
    assert!(result.is_some());
    assert_eq!(result.unwrap().name, "Glob");
}

// ---- New: Combined scavenge ----

#[test]
fn plain_json_examples_are_not_scavenged_as_tool_calls() {
    // Ordinary JSON/code examples are not format errors.
    let (calls, notes) = scavenge_combined(None, Some(r#"config: {"foo": 1, "bar": true}"#), 4);
    assert!(calls.is_empty(), "{calls:?}");
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn unparsable_wrapper_is_surfaced_as_a_degraded_candidate() {
    // An explicit <tool_call> wrapper with damaged inner JSON is a
    // candidate parse failure, not "no candidate".
    let (calls, _) = scavenge_combined(None, Some("<tool_call>{oops}</tool_call>"), 4);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].name.is_empty());
    assert_eq!(calls[0].arguments, "{oops}");
    let error = calls[0].parse_error.as_deref().unwrap_or_default();
    assert!(error.contains("parse tool call JSON"), "{error}");
}

#[test]
fn missing_brace_wrapper_is_surfaced() {
    // The wrapper tag is the candidate marker; a payload with a
    // missing closing brace must not fall through to "plain text success".
    let text = r#"<tool_call>{"name":"TodoRead","arguments":false</tool_call>"#;
    let (calls, _) = scavenge_combined(None, Some(text), 4);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].name.is_empty());
    assert_eq!(
        calls[0].arguments,
        r#"{"name":"TodoRead","arguments":false"#
    );
    assert!(calls[0].parse_error.is_some());
}

#[test]
fn unclosed_wrapper_with_unparsable_body_is_surfaced() {
    let text = r#"Trying <tool_call>{"name":"TodoRead","arguments":false"#;
    let (calls, _) = scavenge_combined(None, Some(text), 4);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let error = calls[0].parse_error.as_deref().unwrap_or_default();
    assert!(error.contains("parse tool call JSON"), "{error}");
}

#[test]
fn unclosed_wrapper_with_usable_json_but_no_identity_is_surfaced() {
    // Valid JSON after an explicit (even unclosed) wrapper marker
    // without a usable identity must not fall back to a plain-text success.
    let text = r#"<tool_call>{"name":"","arguments":{}}"#;
    let (calls, _) = scavenge_combined(None, Some(text), 4);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let error = calls[0].parse_error.as_deref().unwrap_or_default();
    assert!(error.contains("usable tool name"), "{error}");
}

#[test]
fn closed_wrapper_matrix_is_covered() {
    // Closed/unclosed wrapper × valid/invalid JSON × named/nameless name:
    // every explicit candidate either yields a usable call or a degraded one.
    let cases = [
        (
            r#"<tool_call>{"name":"Read","arguments":{"path":"/x"}}</tool_call>"#,
            true,
            false,
        ),
        (
            r#"<tool_call>{"name":"","arguments":{}}</tool_call>"#,
            false,
            true,
        ),
        (
            r#"<tool_call>{"name":"Read","arguments":[/x"}</tool_call>"#,
            false,
            true,
        ),
        (
            r#"<tool_call>{"name":"Read","arguments":{"path":"/x"}}"#,
            true,
            false,
        ),
        (r#"<tool_call>{"name":"","arguments":{}}"#, false, true),
        (
            r#"<tool_call>{"name":"Read","arguments":[/x"}"#,
            false,
            true,
        ),
    ];
    for (text, usable, degraded) in cases {
        let (calls, _) = scavenge_combined(None, Some(text), 4);
        assert_eq!(calls.len(), 1, "{text}: {calls:?}");
        assert_eq!(calls[0].parse_error.is_none(), usable, "{text}: {calls:?}");
        assert_eq!(
            calls[0].parse_error.is_some(),
            degraded,
            "{text}: {calls:?}"
        );
    }
}

#[test]
fn wrapper_with_usable_json_but_no_identity_is_surfaced() {
    // Valid JSON inside an explicit wrapper without a usable
    // tool name is an identity failure, not "no candidate".
    let (calls, _) = scavenge_combined(
        None,
        Some(r#"<tool_call>{"name":"","arguments":{}}</tool_call>"#),
        4,
    );
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].name.is_empty());
    let error = calls[0].parse_error.as_deref().unwrap_or_default();
    assert!(error.contains("usable tool name"), "{error}");

    // Bare JSON (no wrapper) keeps the cautious stance: nothing to recover.
    let (bare, _) = scavenge_combined(None, Some(r#"{"name":"","arguments":{}}"#), 4);
    assert!(bare.is_empty(), "{bare:?}");
}

#[test]
fn dsml_truncation_never_panics() {
    // Every UTF-8 boundary truncation of legal DSML must be safe.
    let samples = [
        r#"<|DSML|invoke name="Bash">"#,
        r#"<|DSML|invoke name="Bash"><|DSML|parameter name="command" string="true">ls<|DSML|parameter></|DSML|invoke>"#,
        r#"<|DSML|invoke name="Bash"><|DSML|parameter name="command" string="false">{"command":"ls"}<|DSML|parameter></|DSML|invoke>"#,
        r#"前缀 <|DSML|invoke name="Read"><|DSML|parameter name="path" string="true">/tmp/x.txt<|DSML|parameter></|DSML|invoke> 后缀"#,
    ];
    for sample in samples {
        for (index, _) in sample
            .char_indices()
            .chain(std::iter::once((sample.len(), ' ')))
        {
            let _ = scavenge_combined(None, Some(&sample[..index]), 4);
        }
    }
}

#[test]
fn truncated_dsml_header_is_degraded_not_a_panic() {
    let truncated = "<|DSML|invoke name=\"Bash\"";
    let (calls, _) = scavenge_combined(None, Some(truncated), 4);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let error = calls[0].parse_error.as_deref().unwrap_or_default();
    assert!(error.contains("header is not terminated"), "{error}");
    assert!(calls[0].arguments.contains("<|DSML|invoke"));
}

#[test]
fn dsml_invalid_json_parameter_is_degraded() {
    // A `string="false"` parameter declaring JSON must not be
    // converted into a string when it fails to parse.
    let text = r#"<|DSML|invoke name="Bash"><|DSML|parameter name="command" string="false">echo audit<|DSML|parameter></|DSML|invoke>"#;
    let (calls, _) = scavenge_combined(None, Some(text), 4);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].name, "Bash");
    let error = calls[0].parse_error.as_deref().unwrap_or_default();
    assert!(
        error.contains("parse DSML parameter `command` as JSON"),
        "{error}"
    );
    assert!(calls[0].arguments.contains("echo audit"));
}

#[test]
fn dsml_damaged_parameter_headers_are_degraded() {
    // Keep the invoke closed: truncating the whole response returns before
    // parameter parsing and cannot catch the original out-of-bounds panic.
    for header in [" string=\"true\">", " string=\"false\">", ">"] {
        for end in 0..header.len() {
            for tail in ["", "中文<|DSML|parameter>"] {
                let text = format!(
                    "<|DSML|invoke name=\"Bash\"><|DSML|parameter name=\"command\"{}{tail}</|DSML|invoke>",
                    &header[..end]
                );
                let (calls, _) = scavenge_combined(None, Some(&text), 4);
                assert_eq!(calls.len(), 1, "{text}: {calls:?}");
                assert_eq!(calls[0].name, "Bash");
                assert!(calls[0].parse_error.is_some(), "{text}: {calls:?}");
                assert_eq!(calls[0].arguments, text);
            }
        }
    }
}

#[test]
fn dsml_valid_parameters_still_parse() {
    let json = r#"<|DSML|invoke name="Bash"><|DSML|parameter name="command" string="false">{"command":"ls"}<|DSML|parameter></|DSML|invoke>"#;
    let (calls, _) = scavenge_combined(None, Some(json), 4);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].parse_error.is_none(), "{calls:?}");
    assert!(calls[0].arguments.contains("ls"), "{}", calls[0].arguments);

    let plain = r#"<|DSML|invoke name="Read"><|DSML|parameter name="path" string="true">/tmp/x.txt<|DSML|parameter></|DSML|invoke>"#;
    let (calls, _) = scavenge_combined(None, Some(plain), 4);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].parse_error.is_none(), "{calls:?}");
    assert!(calls[0].arguments.contains("/tmp/x.txt"));
}

#[test]
fn dsml_truncated_parameter_is_degraded() {
    // The `parameter` closer never arrived: no partially parsed call.
    let text = r#"<|DSML|invoke name="Bash"><|DSML|parameter name="command" string="true">ls</|DSML|invoke>"#;
    let (calls, _) = scavenge_combined(None, Some(text), 4);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let error = calls[0].parse_error.as_deref().unwrap_or_default();
    assert!(error.contains("not terminated"), "{error}");
}

#[test]
fn scavenge_combined_dedup() {
    let text1 = r#"<tool_call>{"name":"Read","arguments":{"path":"/x:1-4"}}</tool_call>"#;
    let text2 = r#"{"name":"Read","arguments":{"path":"/x:1-4"}}"#;
    let (calls, _) = scavenge_combined(Some(text1), Some(text2), 10);
    assert_eq!(calls.len(), 1);
}

#[test]
fn scavenge_combined_respects_max() {
    let text = r#"<tool_call>{"name":"Read","arguments":{"path":"/a"}}</tool_call>
<tool_call>{"name":"Read","arguments":{"path":"/b"}}</tool_call>
<tool_call>{"name":"Read","arguments":{"path":"/c"}}</tool_call>"#;
    let (calls, notes) = scavenge_combined(Some(text), None, 2);
    assert_eq!(calls.len(), 2);
    assert!(notes.iter().any(|n| n.contains("reached max")));
}

#[test]
fn scavenge_dsml_param_without_string_attribute_strips_separator() {
    // 兜底路径（参数无 string= 属性）：此前值会带上前导 '>' 被静默传给工具。
    let text = r#"<|DSML|invoke name="Read">
<|DSML|parameter name="path">/tmp/no-attr.txt<|DSML|parameter>
</|DSML|invoke>"#;
    let result = scavenge_tool_calls(text).unwrap();
    assert_eq!(result[0].name, "Read");
    assert_eq!(args_of(&result[0])["path"], "/tmp/no-attr.txt");
}
