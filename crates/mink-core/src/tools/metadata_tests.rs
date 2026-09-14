use super::*;

#[test]
fn failure_adapter_covers_stable_failure_kinds() {
    let cases = [
        ("operation timed out", None, ToolFailureKind::Timeout),
        ("operation timed out", Some(124), ToolFailureKind::Timeout),
        (
            "invalid tag: stale snapshot",
            None,
            ToolFailureKind::StaleTag,
        ),
        (
            "multiple matches found",
            None,
            ToolFailureKind::AmbiguousMatch,
        ),
        (
            "path is outside workspace",
            None,
            ToolFailureKind::PathOutOfScope,
        ),
        (
            "blocked by bash safety policy",
            None,
            ToolFailureKind::SafetyBlocked,
        ),
        ("invalid argument", None, ToolFailureKind::ArgumentInvalid),
        ("interrupted by user", None, ToolFailureKind::Aborted),
        ("interrupted by user", Some(130), ToolFailureKind::Aborted),
        ("command interrupted", Some(130), ToolFailureKind::Aborted),
        (
            "unclassified internal failure",
            None,
            ToolFailureKind::Unknown,
        ),
        ("command failed", Some(2), ToolFailureKind::ProcessFailed),
    ];
    for (content, exit_code, expected) in cases {
        assert_eq!(classify_failure_kind(content, exit_code), expected);
    }
}

#[test]
fn status_matrix_exposes_failure_kind_without_reading_display_text() {
    // Real process/tool outcomes → structured status (no display-text parsing).
    let cases = [
        (
            ToolStatus::Failed(ToolFailureKind::ArgumentInvalid),
            Some(ToolFailureKind::ArgumentInvalid),
        ),
        (
            ToolStatus::Failed(ToolFailureKind::Timeout),
            Some(ToolFailureKind::Timeout),
        ),
        (ToolStatus::Interrupted, Some(ToolFailureKind::Aborted)),
        (ToolStatus::Blocked(ToolBlocker::ToolSurface), None),
        (ToolStatus::Blocked(ToolBlocker::RecoveryGuard), None),
        (ToolStatus::Succeeded, None),
    ];
    for (status, expected) in cases {
        assert_eq!(status.failure_kind(), expected, "{status:?}");
    }
}
