use super::*;

#[test]
fn json_parse_error_is_parse() {
    let err = anyhow::anyhow!("json parse error: invalid syntax");
    let info = classify_anyhow(&err);
    assert_eq!(info.category, ErrorCategory::Parse);
    assert!(!info.recoverable);
}

#[test]
fn tool_error_is_tool() {
    let err = anyhow::anyhow!("Error: tool execution failed: no such file");
    let info = classify_anyhow(&err);
    assert_eq!(info.category, ErrorCategory::Tool);
    assert!(info.recoverable);
}

#[test]
fn auth_error_classification() {
    let err = anyhow::anyhow!("unauthorized: invalid api key");
    let info = classify_anyhow(&err);
    assert_eq!(info.category, ErrorCategory::Auth);
    assert!(!info.recoverable);
}

#[test]
fn rate_limit_classification() {
    let err = anyhow::anyhow!("rate limit exceeded");
    let info = classify_anyhow(&err);
    assert_eq!(info.category, ErrorCategory::RateLimit);
    assert!(info.recoverable);
}

#[test]
fn typed_upstream_errors_classify_by_kind_and_status() {
    use crate::llm::recovery::LlmUpstreamError;

    let bad_request =
        anyhow::Error::new(LlmUpstreamError::permanent("bad request").with_status(400));
    let info = classify_anyhow(&bad_request);
    assert_eq!(info.category, ErrorCategory::Parse);
    assert!(!info.recoverable);

    let unauthorized =
        anyhow::Error::new(LlmUpstreamError::permanent("unauthorized").with_status(401));
    let info = classify_anyhow(&unauthorized);
    assert_eq!(info.category, ErrorCategory::Auth);
    assert!(!info.recoverable);

    let rate_limited =
        anyhow::Error::new(LlmUpstreamError::recoverable("too many requests").with_status(429));
    let info = classify_anyhow(&rate_limited);
    assert_eq!(info.category, ErrorCategory::RateLimit);
    assert!(info.recoverable);
}
