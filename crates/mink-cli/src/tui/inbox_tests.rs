use super::*;
use crate::runtime::{
    AgentOptions, AgentRuntime, InputStatus, LlmBackend, LlmRequest, LlmResponseStream,
};
use crate::tui::command::{SlashCommand, parse_slash_command};
use crate::tui::input::{handle_ctrl_c, handle_event};
use crate::tui::state::{TuiState, WorkState};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct BoundaryBackend {
    requests: Mutex<Vec<Vec<serde_json::Value>>>,
    release: Arc<tokio::sync::Notify>,
}
impl LlmBackend for BoundaryBackend {
    fn name(&self) -> &str {
        "tui-boundary-test"
    }
    fn stream<'a, 'b>(
        &'a self,
        request: LlmRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<LlmResponseStream>> + Send + 'b>,
    >
    where
        'a: 'b,
        Self: 'b,
    {
        let mut requests = self.requests.lock().unwrap();
        let first = requests.is_empty();
        requests.push(request.messages);
        let release = self.release.clone();
        Box::pin(async move {
            let text = crate::runtime::LlmEvent::Text(crate::runtime::LlmTextEvent {
                content: if first { "answer one" } else { "guided answer" }.into(),
            });
            let stop = crate::runtime::LlmEvent::Stop(crate::runtime::LlmStopEvent {
                reason: "end_turn".into(),
            });
            let events = futures::stream::once(async move { Ok(text) }).chain(
                futures::stream::once(async move {
                    if first {
                        release.notified().await;
                    }
                    Ok(stop)
                }),
            );
            Ok(LlmResponseStream {
                events: Box::pin(events),
                attempt_count: 1,
            })
        })
    }
}

fn options(root: &std::path::Path, backend: Arc<BoundaryBackend>) -> AgentOptions {
    AgentOptions::new(root.join("home"), root.join("project"))
        .with_project_scoped_sessions()
        .with_session(crate::runtime::SessionPolicy::UseOrCreate(
            "guided-tui".into(),
        ))
        .with_llm_backend(backend)
        .with_api_key("test-key")
        .with_max_context_tokens(0)
        .with_log_events(true)
}
fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "mink-tui-{label}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("project")).unwrap();
    root
}
fn submit(
    state: &mut TuiState,
    tx: &tokio::sync::mpsc::UnboundedSender<crate::cli::RuntimeCmd>,
    text: &str,
) {
    state.input.buf = text.into();
    state.input.cursor = text.len();
    assert!(!handle_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        state,
        tx
    ));
}
async fn until(
    state: &mut TuiState,
    rx: &mut mpsc::Receiver<TuiSignal>,
    condition: impl Fn(&TuiState) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            drain_signals(rx, state, TuiMode::Full);
            state.refresh_inputs();
            if condition(state) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("TUI must receive reliable boundary/terminal signals");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_enter_uses_current_turn_and_formal_commit_handoff_without_breaking_stream() {
    let root = temp_root("guide");
    let backend = Arc::new(BoundaryBackend::default());
    let runtime = AgentRuntime::start(options(&root, backend.clone()))
        .await
        .unwrap();
    let (signals, mut rx) = mpsc::channel();
    let tx = crate::cli::start_runtime_broker(
        runtime.handle(),
        Arc::new(TuiDisplay::new(signals.clone())),
    );
    let mut state = TuiState {
        runtime: Some(TuiRuntime {
            handle: runtime.handle(),
            signals,
        }),
        ..Default::default()
    };
    submit(&mut state, &tx, "start task");
    until(&mut state, &mut rx, |s| {
        s.stream_line.contains("answer one")
    })
    .await;
    let turn = state.active_turn_id.clone();
    submit(&mut state, &tx, "keep the public API");
    assert_eq!(state.active_turn_id, turn);
    assert!(state.streaming);
    assert_eq!(state.stream_line, "answer one");
    assert!(
        !state
            .lines
            .iter()
            .any(|line| line.text.contains("keep the public API"))
    );
    assert_eq!(state.inputs.len(), 1);
    assert_eq!(state.inputs[0].status, InputStatus::Pending);
    let input_id = state.inputs[0].input_id.clone();
    for mode in [TuiMode::Full, TuiMode::Inline] {
        let mut surface = state.clone();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| super::render::render(frame, &mut surface, mode))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..12)
            .map(|y| {
                (0..80)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Enter: submit guidance"));
        assert!(text.contains("Accepted; waiting for safe boundary"));
    }
    // Submission leaves room for a new draft, preserved by the eventual final.
    state.input.buf = "next draft".into();
    backend.release.notify_one();
    until(&mut state, &mut rx, |s| s.active_turn_id.is_none()).await;
    assert_eq!(state.input.buf, "next draft");
    assert_eq!(state.stats.current_turn_count, 1);
    let texts = state
        .lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>();
    let guide = texts
        .iter()
        .position(|text| text.contains("[Added to context] keep the public API"))
        .unwrap();
    assert!(
        texts
            .iter()
            .position(|text| text.contains("answer one"))
            .unwrap()
            < guide
    );
    assert!(
        texts
            .iter()
            .position(|text| text.contains("guided answer"))
            .unwrap()
            > guide
    );
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.contains("keep the public API"))
            .count(),
        1
    );
    state.apply(&TuiSignal::GuidanceApplied {
        input_id,
        text: "keep the public API".into(),
    });
    assert_eq!(
        state
            .lines
            .iter()
            .filter(|l| l.text.contains("keep the public API"))
            .count(),
        1
    );
    let requests = backend.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .iter()
            .any(|row| row["content"].as_str() == Some("keep the public API"))
    );
    let replay = load_session(&runtime.session_info().events_path);
    let replay_text = replay.iter().map(|l| l.text.clone()).collect::<Vec<_>>();
    assert_eq!(
        replay_text
            .iter()
            .filter(|t| t.contains("keep the public API"))
            .count(),
        1
    );
    assert!(
        replay_text
            .iter()
            .position(|t| t.contains("answer one"))
            .unwrap()
            < replay_text
                .iter()
                .position(|t| t.contains("keep the public API"))
                .unwrap()
    );
    assert!(
        replay_text
            .iter()
            .position(|t| t.contains("keep the public API"))
            .unwrap()
            < replay_text
                .iter()
                .position(|t| t.contains("guided answer"))
                .unwrap()
    );
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_inputs_survive_reopen_and_only_explicit_resume_runs_them() {
    let root = temp_root("resume");
    let backend = Arc::new(BoundaryBackend::default());
    let runtime = AgentRuntime::start(options(&root, backend.clone()))
        .await
        .unwrap();
    let (signals, mut rx) = mpsc::channel();
    let tx = crate::cli::start_runtime_broker(
        runtime.handle(),
        Arc::new(TuiDisplay::new(signals.clone())),
    );
    let mut state = TuiState {
        runtime: Some(TuiRuntime {
            handle: runtime.handle(),
            signals: signals.clone(),
        }),
        ..Default::default()
    };
    submit(&mut state, &tx, "start task");
    until(&mut state, &mut rx, |s| s.streaming).await;
    submit(&mut state, &tx, "resume this");
    submit(&mut state, &tx, "withdraw this");
    let first = state.inputs[0].input_id.clone();
    let second = state.inputs[1].input_id.clone();
    assert!(!handle_ctrl_c(&mut state, &tx));
    assert!(state.stopping);
    submit(&mut state, &tx, "draft while stopping");
    assert_eq!(state.input.buf, "draft while stopping");
    until(&mut state, &mut rx, |s| s.active_turn_id.is_none()).await;
    assert!(
        state
            .inputs
            .iter()
            .all(|i| i.status == InputStatus::Unapplied)
    );
    assert_eq!(
        state.take_task_notification().unwrap().kind,
        super::notify::TaskNotificationKind::Interrupted
    );
    assert_eq!(backend.requests.lock().unwrap().len(), 1);
    runtime.shutdown().await.unwrap();
    let runtime = AgentRuntime::start(options(&root, backend.clone()))
        .await
        .unwrap();
    let tx = crate::cli::start_runtime_broker(
        runtime.handle(),
        Arc::new(TuiDisplay::new(signals.clone())),
    );
    state.runtime = Some(TuiRuntime {
        handle: runtime.handle(),
        signals,
    });
    state.refresh_inputs();
    assert_eq!(state.inputs.len(), 2);
    assert_eq!(backend.requests.lock().unwrap().len(), 1);
    submit(&mut state, &tx, "/inputs");
    assert!(
        state
            .lines
            .iter()
            .any(|l| l.text.contains(&first) && l.text.contains("Unapplied"))
    );
    submit(&mut state, &tx, &format!("/withdraw {second}"));
    assert_eq!(state.inputs.len(), 1);
    assert_eq!(backend.requests.lock().unwrap().len(), 1);
    submit(&mut state, &tx, &format!("/resume {first}"));
    until(&mut state, &mut rx, |s| s.active_turn_id.is_none()).await;
    assert_eq!(backend.requests.lock().unwrap().len(), 2);
    assert!(state.inputs.is_empty());
    // A stale UI turn identity is a rejected guide, never a new queued turn.
    state.active_turn_id = Some("ended-turn".into());
    state.input.pending_images.push(super::state::PendingImage {
        path: root.join("attachments/image.png"),
        width: 1,
        height: 1,
        bytes: 3,
    });
    submit(&mut state, &tx, "late guide draft");
    assert_eq!(state.input.buf, "late guide draft");
    assert_eq!(state.input.pending_images.len(), 1);
    assert!(
        state
            .input_notice
            .as_deref()
            .unwrap()
            .contains("Input rejected")
    );
    assert!(state.inputs.is_empty());
    assert_eq!(backend.requests.lock().unwrap().len(), 2);
    runtime.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn round_stop_and_stale_final_cannot_end_an_active_guided_turn() {
    let mut state = TuiState {
        active_turn_id: Some("current".into()),
        work_state: WorkState::RunningTool,
        ..Default::default()
    };
    state.arm_task_notification();
    state.apply(&TuiSignal::Stop);
    state.apply(&TuiSignal::TurnFinished {
        turn_id: "old".into(),
        status: crate::runtime::TurnStatus::Ok,
    });
    assert_eq!(state.active_turn_id.as_deref(), Some("current"));
    assert!(state.take_task_notification().is_none());
    assert_eq!(
        parse_slash_command("/resume saved-id").unwrap(),
        Some(SlashCommand::Resume("saved-id".into()))
    );
    assert_eq!(
        parse_slash_command("/withdraw saved-id").unwrap(),
        Some(SlashCommand::Withdraw("saved-id".into()))
    );
}
