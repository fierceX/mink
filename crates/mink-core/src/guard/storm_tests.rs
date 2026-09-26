use super::*;

#[test]
fn allows_if_below_threshold() {
    let mut sb = StormBreaker::new(6, 3);
    assert!(matches!(
        sb.check("Read", r#"{"path":"/tmp/x"}"#, false),
        StormDecision::Allow
    ));
    assert!(matches!(
        sb.check("Read", r#"{"path":"/tmp/x"}"#, false),
        StormDecision::Allow
    ));
}

#[test]
fn suppresses_after_threshold() {
    // The first `threshold` identical calls are allowed so graded tool
    // feedback (Edit soft no-op escalation) can complete; suppression
    // starts from the threshold+1-th identical call.
    let mut sb = StormBreaker::new(6, 3);
    for i in 0..3 {
        assert!(
            matches!(
                sb.check("Bash", r#"{"command":"ls"}"#, false),
                StormDecision::Allow
            ),
            "call {} should be allowed",
            i + 1
        );
    }
    assert!(matches!(
        sb.check("Bash", r#"{"command":"ls"}"#, false),
        StormDecision::Suppress(_)
    ));
}

#[test]
fn mutating_calls_share_window_and_suppress() {
    // Repeated identical mutating calls are exactly the edit-loop storm the
    // breaker exists to stop; they must count toward the threshold instead
    // of being permanently immunized by a window clear.
    let mut sb = StormBreaker::new(6, 3);
    for _ in 0..3 {
        assert!(matches!(
            sb.check("Write", r#"{"path":"/x","content":"hi"}"#, true),
            StormDecision::Allow
        ));
    }
    assert!(matches!(
        sb.check("Write", r#"{"path":"/x","content":"hi"}"#, true),
        StormDecision::Suppress(_)
    ));
}

#[test]
fn mutating_call_resets_non_mutating_history_only() {
    let mut sb = StormBreaker::new(6, 3);
    sb.check("Read", r#"{"path":"/x"}"#, false);
    sb.check("Read", r#"{"path":"/x"}"#, false);
    sb.check("Write", r#"{"path":"/x","content":"hi"}"#, true);

    // The Read count restarts after the mutation.
    assert!(matches!(
        sb.check("Read", r#"{"path":"/x"}"#, false),
        StormDecision::Allow
    ));
    assert!(matches!(
        sb.check("Read", r#"{"path":"/x"}"#, false),
        StormDecision::Allow
    ));

    // A legitimate read-edit-read loop never suppresses the same Read.
    for _ in 0..8 {
        sb.check("Read", r#"{"path":"/x"}"#, false);
        sb.check("Write", r#"{"path":"/x","content":"new"}"#, true);
    }
    assert!(matches!(
        sb.check("Read", r#"{"path":"/x"}"#, false),
        StormDecision::Allow
    ));
}

#[test]
fn different_mutating_arguments_do_not_cross_count() {
    let mut sb = StormBreaker::new(6, 3);
    for _ in 0..3 {
        assert!(matches!(
            sb.check("Write", r#"{"path":"/x","content":"a"}"#, true),
            StormDecision::Allow
        ));
        assert!(matches!(
            sb.check("Write", r#"{"path":"/x","content":"b"}"#, true),
            StormDecision::Allow
        ));
    }
}

#[test]
fn window_slides_old_entries_out() {
    let mut sb = StormBreaker::new(3, 3);
    sb.check("Bash", "a", false);
    sb.check("Bash", "b", false);
    sb.check("Bash", "c", false);
    // "a" should be evicted, so only 2 of "Bash/a"
    let d = sb.check("Bash", "a", false);
    assert!(matches!(d, StormDecision::Allow));
}

#[test]
fn reset_clears_window() {
    let mut sb = StormBreaker::new(3, 3);
    sb.check("Read", "a", false);
    sb.check("Read", "a", false);
    sb.reset();
    let d = sb.check("Read", "a", false);
    assert!(matches!(d, StormDecision::Allow));
}

#[test]
fn format_confirmation_matches_only_the_identical_payload() {
    let mut sb = StormBreaker::new(6, 3);
    sb.check("Bash", "raw:aaaa", false);
    sb.mark_format("Bash", "raw:aaaa");
    assert!(sb.is_format_confirmed("Bash", "raw:aaaa", false));
    // A different bad payload keeps its own identity in the same window.
    assert!(!sb.is_format_confirmed("Bash", "raw:bbbb", false));
    assert!(!sb.is_format_confirmed("Read", "raw:aaaa", false));
    // Different mutating class does not cross-match.
    sb.check("Bash", "raw:aaaa", true);
    assert!(!sb.is_format_confirmed("Bash", "raw:aaaa", true));
}

#[test]
fn format_flag_survives_until_the_entry_leaves_the_window() {
    let mut sb = StormBreaker::new(2, 3);
    sb.check("Bash", "raw:aaaa", false);
    sb.mark_format("Bash", "raw:aaaa");
    // Evict the only entry with unrelated calls.
    sb.check("Read", "a", false);
    sb.check("Read", "b", false);
    sb.check("Read", "c", false);
    assert!(!sb.is_format_confirmed("Bash", "raw:aaaa", false));
}
