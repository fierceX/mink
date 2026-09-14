use super::{AgentEvent, AgentEventKind, EventDispatcher, EventSink};
use std::sync::Arc;

#[test]
fn shared_server_protocol_fixture_is_real_agent_event_json() {
    let fixture = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../mink-server/protocol-fixtures/agent-events.json"
    ));
    let expected: serde_json::Value = serde_json::from_str(fixture).unwrap();
    let events: Vec<AgentEvent> = serde_json::from_value(expected.clone()).unwrap();
    assert_eq!(serde_json::to_value(events).unwrap(), expected);
}

struct BlockingSink;

#[async_trait::async_trait]
impl EventSink for BlockingSink {
    async fn on_event(&self, _event: AgentEvent) -> Result<(), String> {
        std::future::pending().await
    }
}

fn info_event(sequence: u64) -> AgentEvent {
    AgentEvent {
        turn_id: None,
        sequence,
        kind: AgentEventKind::Info {
            message: sequence.to_string(),
        },
    }
}

#[tokio::test]
async fn observer_overflow_drops_newest_and_keeps_observer_alive() {
    let dispatcher = EventDispatcher::new(Arc::new(BlockingSink));
    for sequence in 0..2048 {
        dispatcher.dispatch(info_event(sequence));
    }

    dispatcher
        .shutdown_with_timeout(std::time::Duration::from_millis(1))
        .await
        .unwrap_err();
    assert!(
        dispatcher.dropped_events() > 0,
        "overflowing dispatch must record dropped events"
    );
}

struct FailingSink;

#[async_trait::async_trait]
impl EventSink for FailingSink {
    async fn on_event(&self, _event: AgentEvent) -> Result<(), String> {
        Err("observer failure".into())
    }
}

#[tokio::test]
async fn observer_failure_is_reported_at_shutdown() {
    let dispatcher = EventDispatcher::new(Arc::new(FailingSink));
    dispatcher.dispatch(info_event(1));
    tokio::task::yield_now().await;
    dispatcher.dispatch(info_event(2));
    let error = dispatcher.shutdown().await.unwrap_err();
    assert!(error.contains("observer failure"));
    assert!(!error.contains("stopped before runtime shutdown"));
}

#[tokio::test]
async fn observer_shutdown_timeout_aborts_and_reports_failure() {
    let dispatcher = EventDispatcher::new(Arc::new(BlockingSink));
    dispatcher.dispatch(info_event(1));
    tokio::task::yield_now().await;
    let error = dispatcher
        .shutdown_with_timeout(std::time::Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(error.contains("shutdown timed out"));
}

#[test]
fn event_display_preserves_overlapping_event_fields() {
    use crate::tools::metadata::{ToolFailureKind, ToolResultKind, ToolStatus};
    use crate::ui::Display as _;
    use crate::ui::{PresentedToolResultDisplay, ToolCallDisplay, ToolResultDisplay};

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let display = super::EventDisplay::new(None);
    display.begin_turn(Arc::new(super::TurnEventEmitter::new(
        crate::runtime::TurnId::new("turn-1"),
        Some(tx),
        None,
    )));

    let input = serde_json::json!({"path": "a.rs", "start": 1});
    display.render_tool_call(&ToolCallDisplay {
        tool_use_id: "call_1",
        tool_name: "Read",
        summary: "Read a.rs",
        input: Some(&input),
    });
    match rx.try_recv().expect("tool call event").kind {
        AgentEventKind::ToolCall {
            id,
            name,
            summary,
            input,
        } => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "Read");
            assert_eq!(summary, "Read a.rs");
            assert_eq!(input, serde_json::json!({"path": "a.rs", "start": 1}));
        }
        other => panic!("unexpected event: {other:?}"),
    }

    display.render_tool_result(&PresentedToolResultDisplay {
        base: ToolResultDisplay {
            tool_name: "Bash",
            content_preview: "preview",
            content: "full content",
            tool_use_id: Some("call_1"),
            exit_code: Some(2),
        },
        status: ToolStatus::Failed(ToolFailureKind::ProcessFailed),
        result_kind: ToolResultKind::Command,
        presentation: None,
        artifacts: &[],
    });
    match rx.try_recv().expect("tool result event").kind {
        AgentEventKind::ToolResult {
            tool_use_id,
            tool_name,
            content_preview,
            content,
            status,
            exit_code,
            result_kind,
            presentation,
            artifacts,
        } => {
            assert_eq!(tool_use_id.as_deref(), Some("call_1"));
            assert_eq!(tool_name, "Bash");
            assert_eq!(content_preview, "preview");
            assert_eq!(content, "full content");
            assert_eq!(status, ToolStatus::Failed(ToolFailureKind::ProcessFailed));
            assert_eq!(exit_code, Some(2));
            assert_eq!(result_kind, ToolResultKind::Command);
            assert!(presentation.is_none());
            assert!(artifacts.is_empty());
        }
        other => panic!("unexpected event: {other:?}"),
    }

    display.render_signal("ToolFailed", 0.9, "command failed");
    match rx.try_recv().expect("signal event").kind {
        AgentEventKind::Signal {
            signal_kind,
            severity,
            message,
        } => {
            assert_eq!(signal_kind, "ToolFailed");
            assert_eq!(severity, 0.9);
            assert_eq!(message, "command failed");
        }
        other => panic!("unexpected event: {other:?}"),
    }

    display.render_sub_agent_status("child", "ok", 12, 34);
    match rx.try_recv().expect("sub-agent status").kind {
        AgentEventKind::SubAgentStatus {
            session_id,
            status,
            in_tokens,
            out_tokens,
        } => {
            assert_eq!(session_id, "child");
            assert_eq!(status, "ok");
            assert_eq!(in_tokens, 12);
            assert_eq!(out_tokens, 34);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn progress_budget_bounds_pending_bytes_and_counts_drops() {
    let min = super::PROGRESS_EVENT_MIN_BYTES;
    let budget = super::ProgressBudget::new(2 * min);
    assert!(budget.reserve(8));
    // Empty deltas still charge the structural minimum.
    assert!(budget.reserve(0));
    assert_eq!(budget.pending(), 2 * min);
    assert!(!budget.reserve(1));
    assert!(!budget.reserve(1));
    assert_eq!(budget.dropped(), 2);
    budget.release(0);
    assert_eq!(budget.pending(), min);
    assert!(budget.reserve(0));
    // Saturating release must not underflow.
    budget.release(1 << 20);
    assert_eq!(budget.pending(), 0);
}

#[test]
fn emitter_drops_progress_over_budget_but_keeps_reliable_events() {
    use crate::protocol::{Event, StopEvent, TextEvent};

    let budget = super::ProgressBudget::new(super::PROGRESS_EVENT_MIN_BYTES);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let emitter =
        super::TurnEventEmitter::new(crate::runtime::TurnId::new("turn-budget"), Some(tx), None)
            .with_progress_budget(budget.clone());

    emitter.emit(AgentEventKind::Text {
        content: "12345678".into(),
    });
    assert_eq!(budget.pending(), super::PROGRESS_EVENT_MIN_BYTES);

    // Over budget: the first drop emits one notice, later drops are silent.
    emitter.emit(AgentEventKind::Text {
        content: "x".into(),
    });
    emitter.emit(AgentEventKind::Text {
        content: "y".into(),
    });
    assert_eq!(budget.dropped(), 2);

    emitter.emit(AgentEventKind::Stop {
        reason: "end_turn".into(),
    });

    let first = rx.try_recv().expect("budgeted text");
    assert!(matches!(first.kind, AgentEventKind::Text { .. }));
    let notice = rx.try_recv().expect("one-time overflow notice");
    match notice.kind {
        AgentEventKind::Info { message } => {
            assert!(message.contains("budget"), "{message}");
        }
        other => panic!("unexpected event: {other:?}"),
    }
    let stop = rx.try_recv().expect("reliable stop event");
    assert!(matches!(stop.kind, AgentEventKind::Stop { .. }));
    // The notice and stop are reliable: no third event, no dropped progress.
    assert!(rx.try_recv().is_err());
    let _ = (
        Event::Text(TextEvent {
            content: String::new(),
        }),
        StopEvent {
            reason: String::new(),
        },
    );
}

#[tokio::test]
async fn slow_stream_consumer_does_not_starve_observer() {
    struct Recorder {
        tx: std::sync::mpsc::Sender<AgentEventKind>,
    }

    #[async_trait::async_trait]
    impl EventSink for Recorder {
        async fn on_event(&self, event: AgentEvent) -> Result<(), String> {
            let _ = self.tx.send(event.kind);
            Ok(())
        }
    }

    let budget = super::ProgressBudget::new(8);
    // Stream subscriber exists but is never consumed.
    let (stream_tx, _stream_rx) = tokio::sync::mpsc::unbounded_channel();
    let (record_tx, record_rx) = std::sync::mpsc::channel();
    let dispatcher = EventDispatcher::new(Arc::new(Recorder { tx: record_tx }));
    let emitter = super::TurnEventEmitter::new(
        crate::runtime::TurnId::new("turn-observer"),
        Some(stream_tx),
        Some(dispatcher.clone()),
    )
    .with_progress_budget(budget.clone());

    for _ in 0..100 {
        emitter.emit(AgentEventKind::Text {
            content: "0123456789".into(),
        });
    }
    dispatcher.shutdown().await.unwrap();

    let observed = record_rx.try_iter().count();
    assert_eq!(
        observed, 100,
        "observer delivery must not depend on the stream budget"
    );
    assert!(
        budget.dropped() > 0,
        "the stalled stream itself must still be bounded"
    );
}

#[test]
fn empty_progress_deltas_are_charged_and_bounded() {
    let budget = super::ProgressBudget::new(super::PROGRESS_PENDING_BYTES_LIMIT);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let emitter =
        super::TurnEventEmitter::new(crate::runtime::TurnId::new("turn-empty"), Some(tx), None)
            .with_progress_budget(budget.clone());

    // 20k zero-length deltas: without the structural minimum they would count
    // zero bytes while still occupying queue slots.
    for _ in 0..20_000 {
        emitter.emit(AgentEventKind::Text {
            content: String::new(),
        });
    }

    assert!(
        budget.pending() <= super::PROGRESS_PENDING_BYTES_LIMIT,
        "pending {}",
        budget.pending()
    );
    assert!(budget.dropped() > 0);
}
