use super::*;
use crate::protocol::{Event, StopEvent, TextEvent};
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<(String, SessionEvent)>>,
    skip_text: bool,
}

impl RecordingSink {
    fn with_text_filter() -> Self {
        Self {
            skip_text: true,
            ..Self::default()
        }
    }

    fn take(&self) -> Vec<(String, SessionEvent)> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

impl SessionEventSink for RecordingSink {
    fn wants(&self, _session_id: &str, event: &SessionEvent) -> bool {
        !(self.skip_text && matches!(event, SessionEvent::Text { .. }))
    }

    fn on_session_event(&self, session_id: &str, event: &SessionEvent) {
        self.events
            .lock()
            .unwrap()
            .push((session_id.to_string(), event.clone()));
    }
}

fn unique_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "mink-sink-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn mock_hello() -> crate::llm::mock::MockLlmBackend {
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Text(TextEvent {
                content: "Hello".into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]],
    )
}

fn mock_stop() -> crate::llm::mock::MockLlmBackend {
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![vec![Ok(Event::Stop(StopEvent {
            reason: "error".into(),
        }))]],
    )
}

async fn run_with_sink(sink: Arc<RecordingSink>) -> crate::runtime::TurnOutcome {
    run_with_sink_backend(sink, mock_hello()).await
}

async fn run_with_sink_backend(
    sink: Arc<RecordingSink>,
    backend: crate::llm::mock::MockLlmBackend,
) -> crate::runtime::TurnOutcome {
    let home = unique_dir("home");
    let cwd = unique_dir("cwd");
    tokio::fs::create_dir_all(&cwd).await.unwrap();
    let runtime = AgentRuntime::start(
        AgentOptions::new(&home, &cwd)
            .with_api_key("test-key")
            .with_base_url("https://example.invalid/v1")
            .with_model("flash")
            .with_max_context_tokens(1_000_000)
            .with_llm_backend(Arc::new(backend))
            .with_log_events(false)
            .with_session_event_sink(sink),
    )
    .await
    .unwrap();
    let outcome = runtime.run_turn("hello").await.unwrap();
    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(home).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
    outcome
}

#[tokio::test]
async fn sink_receives_turn_lifecycle_even_without_disk_logging() {
    let sink = Arc::new(RecordingSink::default());
    let outcome = run_with_sink(sink.clone()).await;
    let events = sink.take();
    let session_id = outcome.session.session_id.clone();
    assert!(
        events
            .iter()
            .any(|(id, event)| id == &session_id && matches!(event, SessionEvent::TurnStart { .. })),
        "missing turn_start for {session_id}: {events:?}"
    );
    assert!(
        events.iter().any(|(id, event)| id == &session_id
            && matches!(event, SessionEvent::TurnFinal { status, .. } if status == "ok")),
        "missing ok turn_final: {events:?}"
    );
    let final_failure = events
        .iter()
        .find_map(|(_, event)| match event {
            SessionEvent::TurnFinal { failure, .. } => Some(*failure),
            _ => None,
        })
        .expect("turn_final present");
    assert_eq!(final_failure, outcome.failure);
}

#[tokio::test]
async fn sink_reports_the_terminal_failure_reason() {
    let sink = Arc::new(RecordingSink::default());
    let outcome = run_with_sink_backend(sink.clone(), mock_stop()).await;
    assert_eq!(
        outcome.failure,
        Some(crate::runtime::TurnFailureReason::ProviderStopped)
    );
    let events = sink.take();
    let final_failure = events
        .iter()
        .find_map(|(_, event)| match event {
            SessionEvent::TurnFinal { failure, .. } => Some(*failure),
            _ => None,
        })
        .expect("turn_final present");
    assert_eq!(
        final_failure,
        Some(crate::runtime::TurnFailureReason::ProviderStopped)
    );
}

#[tokio::test]
async fn sink_wants_filters_high_volume_events() {
    let sink = Arc::new(RecordingSink::with_text_filter());
    run_with_sink(sink.clone()).await;
    let events = sink.take();
    assert!(!events.is_empty());
    assert!(
        !events
            .iter()
            .any(|(_, event)| matches!(event, SessionEvent::Text { .. })),
        "wants=false must suppress text deltas"
    );
    assert!(
        events
            .iter()
            .any(|(_, event)| matches!(event, SessionEvent::TurnStart { .. }))
    );
}
