use super::*;
use crate::events::EventLog;

fn temp_session(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mink-session-events-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn line(event: &EventLog) -> String {
    serde_json::to_string(event).unwrap()
}

#[test]
fn events_reader_returns_known_records_and_counts_skipped_lines() {
    let dir = temp_session("mixed");
    let known_a = line(&EventLog::SessionStart {
        session_id: "s-1".into(),
    });
    let known_b = line(&EventLog::Retry);
    let unknown = r#"{"type":"future_event","value":1}"#;
    let malformed = "not json";
    let partial = r#"{"type":"stop","reason":"unfinished""#;
    let data = format!("{known_a}\n{unknown}\n{malformed}\n{known_b}\n{partial}");
    std::fs::write(dir.join("events.jsonl"), &data).unwrap();

    let batch = SessionReader::new(&dir).events_since(0, 100).unwrap();
    assert_eq!(batch.events.len(), 2);
    assert_eq!(batch.skipped, 2);
    assert!(!batch.truncated);
    assert_eq!(
        batch.next_offset,
        (known_a.len() + unknown.len() + malformed.len() + known_b.len() + 4) as u64
    );

    // Finish the partial line: the next call must see it exactly once.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(dir.join("events.jsonl"))
        .unwrap();
    use std::io::Write as _;
    file.write_all(b"}\n").unwrap();
    let next = SessionReader::new(&dir)
        .events_since(batch.next_offset, 100)
        .unwrap();
    assert_eq!(next.events.len(), 1);
    assert!(matches!(
        &next.events[0],
        EventLog::Stop { reason } if reason == "unfinished"
    ));
    assert_eq!(next.skipped, 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn events_reader_pages_without_duplicates_or_loss() {
    let dir = temp_session("pages");
    let mut data = String::new();
    for index in 0..4 {
        data.push_str(&line(&EventLog::Stop {
            reason: format!("r{index}"),
        }));
        data.push('\n');
    }
    std::fs::write(dir.join("events.jsonl"), data).unwrap();
    let reader = SessionReader::new(&dir);

    let first = reader.events_since(0, 2).unwrap();
    assert_eq!(first.events.len(), 2);
    let second = reader.events_since(first.next_offset, 2).unwrap();
    assert_eq!(second.events.len(), 2);
    assert_ne!(first.next_offset, second.next_offset);
    let third = reader.events_since(second.next_offset, 2).unwrap();
    assert!(third.events.is_empty());
    assert_eq!(third.skipped, 0);

    let reasons: Vec<String> = first
        .events
        .iter()
        .chain(second.events.iter())
        .map(|event| match event {
            EventLog::Stop { reason } => reason.clone(),
            other => panic!("unexpected event: {other:?}"),
        })
        .collect();
    assert_eq!(reasons, ["r0", "r1", "r2", "r3"]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn events_reader_rejects_a_mid_line_offset() {
    let dir = temp_session("midline");
    std::fs::write(
        dir.join("events.jsonl"),
        format!("{}\n", line(&EventLog::Retry)),
    )
    .unwrap();
    let error = SessionReader::new(&dir).events_since(1, 10).unwrap_err();
    assert!(format!("{error:#}").contains("not at a line boundary"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn events_reader_reports_a_truncated_offset_and_resets_it() {
    let dir = temp_session("truncated");
    std::fs::write(
        dir.join("events.jsonl"),
        format!("{}\n", line(&EventLog::Retry)),
    )
    .unwrap();
    let batch = SessionReader::new(&dir).events_since(999, 10).unwrap();
    assert!(batch.events.is_empty());
    assert!(batch.truncated);
    assert_eq!(batch.next_offset, 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn events_reader_handles_missing_files_and_the_end_offset() {
    let dir = temp_session("missing");
    let missing = SessionReader::new(dir.join("absent"))
        .events_since(0, 10)
        .unwrap();
    assert!(missing.events.is_empty());
    assert_eq!(missing.next_offset, 0);
    assert!(!missing.truncated);

    let data = format!("{}\n", line(&EventLog::Retry));
    std::fs::write(dir.join("events.jsonl"), &data).unwrap();
    let at_end = SessionReader::new(&dir)
        .events_since(data.len() as u64, 10)
        .unwrap();
    assert!(at_end.events.is_empty());
    assert_eq!(at_end.next_offset, data.len() as u64);
    assert!(!at_end.truncated);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn events_reader_requires_a_positive_limit() {
    let dir = temp_session("limit");
    let error = SessionReader::new(&dir).events_since(0, 0).unwrap_err();
    assert!(format!("{error:#}").contains("max_events"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn turn_final_failure_field_is_optional_and_round_trips() {
    let with_failure = EventLog::TurnFinal {
        billing_turn_id: "turn-1".into(),
        status: "failed".into(),
        failure: Some(crate::runtime::TurnFailureReason::RequestTimeout),
        tool_call_count: 1,
        tool_error_count: 0,
        elapsed_ms: 12,
        error: Some("request_timeout: deadline".into()),
        usage: Default::default(),
    };
    let line = serde_json::to_string(&with_failure).unwrap();
    assert!(line.contains("\"failure\":\"request_timeout\""), "{line}");
    let parsed: EventLog = serde_json::from_str(&line).unwrap();
    assert!(matches!(
        parsed,
        EventLog::TurnFinal {
            failure: Some(crate::runtime::TurnFailureReason::RequestTimeout),
            ..
        }
    ));

    // A pre-failure log line has no such field and must still decode.
    let without = EventLog::TurnFinal {
        billing_turn_id: "turn-2".into(),
        status: "failed".into(),
        failure: None,
        tool_call_count: 0,
        tool_error_count: 0,
        elapsed_ms: 1,
        error: Some("old record".into()),
        usage: Default::default(),
    };
    let line = serde_json::to_string(&without).unwrap();
    assert!(!line.contains("failure"), "{line}");
    let parsed: EventLog = serde_json::from_str(&line).unwrap();
    assert!(matches!(parsed, EventLog::TurnFinal { failure: None, .. }));
}

#[test]
fn session_event_alias_is_public() {
    // The prelude and runtime must expose the same durable vocabulary.
    let event: crate::runtime::SessionEvent = crate::prelude::SessionEvent::Retry;
    let _: crate::runtime::SessionEventBatch = crate::runtime::SessionEventBatch::default();
    assert!(matches!(event, EventLog::Retry));
}
