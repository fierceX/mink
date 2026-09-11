//! CLI broker tests for control-operation output (audit F4).
//!
//! These tests exercise the CLI adaptation layer (not core): the broker must
//! surface `CompactOutcome`, model-switch success, the resolved model label /
//! title snapshot, and errors exactly once, even though core renders those
//! through an event emitter that has no current turn during control
//! operations.

use super::*;
use mink::runtime::StatsSnapshot;

#[derive(Default)]
struct RecordingDisplay {
    infos: std::sync::Mutex<Vec<String>>,
    errors: std::sync::Mutex<Vec<String>>,
    titles: std::sync::Mutex<Vec<(String, StatsSnapshot)>>,
}

impl RecordingDisplay {
    fn infos(&self) -> Vec<String> {
        self.infos.lock().unwrap().clone()
    }
    fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap().clone()
    }
    fn titles(&self) -> Vec<(String, StatsSnapshot)> {
        self.titles.lock().unwrap().clone()
    }
}

impl Display for RecordingDisplay {
    fn render_thinking(&self, _content: &str) {}
    fn render_text(&self, _content: &str) {}
    fn render_tool_call(&self, _call: &ToolCallDisplay<'_>) {}
    fn render_tool_result(&self, _result: &PresentedToolResultDisplay<'_>) {}
    fn render_stop(&self, _reason: &str) {}
    fn render_signal(&self, _signal_kind: &str, _severity: f64, _message: &str) {}
    fn render_error(&self, message: &str) {
        self.errors.lock().unwrap().push(message.to_string());
    }
    fn render_retry(&self) {}
    fn render_info(&self, msg: &str) {
        self.infos.lock().unwrap().push(msg.to_string());
    }
    fn render_title_update(&self, model: &str, stats: &StatsSnapshot) {
        self.titles
            .lock()
            .unwrap()
            .push((model.to_string(), stats.clone()));
    }
    fn render_sub_agent_status(
        &self,
        _session_id: &str,
        _status: &str,
        _in_tokens: u64,
        _out_tokens: u64,
    ) {
    }
    fn render_sub_agent_output(
        &self,
        _session_id: &str,
        _status: &str,
        _thinking: &str,
        _text: &str,
        _in_tokens: u64,
        _out_tokens: u64,
    ) {
    }
    fn render_prompt(&self) {}
    fn render_clear_line(&self) {}
}

/// Control tests never enter a turn, so the stub only needs to satisfy the
/// trait; an accidental call fails fast instead of hanging.
struct StubBackend;

impl mink::runtime::LlmBackend for StubBackend {
    fn name(&self) -> &str {
        "cli-control-stub"
    }

    fn stream<'life0, 'async_trait>(
        &'life0 self,
        _request: mink::runtime::LlmRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = anyhow::Result<mink::runtime::LlmResponseStream>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(std::future::ready(Err(anyhow::anyhow!(
            "stub backend must not be called in control tests"
        ))))
    }
}

fn unique_dir(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "mink-cli-broker-{name}-{}-{nanos}",
        std::process::id()
    ))
}

async fn start_test_runtime(name: &str) -> (AgentRuntime, std::path::PathBuf) {
    let base = unique_dir(name);
    let home = base.join("home");
    let cwd = base.join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let options = AgentOptions::new(home, cwd)
        .with_llm_backend(std::sync::Arc::new(StubBackend))
        .with_model_alias("flash", "deepseek-v4-flash")
        .with_api_key("test-key")
        .with_base_url("https://example.invalid/v1");
    let runtime = AgentRuntime::start(options).await.unwrap();
    (runtime, base)
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    for _ in 0..300 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition was not reached within the test timeout");
}

#[cfg(feature = "tui")]
#[test]
fn tui_launcher_error_carries_context() {
    let error = launch_tui_with(|| anyhow::bail!("terminal init failed")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("TUI error: terminal init failed"),
        "{error}"
    );
}

#[cfg(feature = "tui")]
#[test]
fn tui_launcher_success_is_not_a_failure() {
    launch_tui_with(|| Ok(())).unwrap();
}

#[tokio::test]
async fn finish_turn_still_runs_shutdown_after_failure() {
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_flag = flag.clone();
    let result = finish_turn(
        Err(anyhow::anyhow!("TUI error: terminal init failed")),
        async move {
            shutdown_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
    )
    .await;
    assert!(
        flag.load(std::sync::atomic::Ordering::SeqCst),
        "shutdown must run even when the turn/TUI failed"
    );
    assert!(result.is_err());
}

#[test]
fn combine_reports_primary_and_cleanup_failures() {
    // Both failed: the primary error and the cleanup failure are both kept.
    let error = combine_turn_and_shutdown(
        Err(anyhow::anyhow!("turn failed")),
        Err(crate::runtime::RuntimeError::Command("flush failed".into())),
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("turn failed"), "{message}");
    assert!(
        message.contains("cleanup also failed: runtime command failed: flush failed"),
        "{message}"
    );

    // Primary failure with successful cleanup stays primary.
    let error = combine_turn_and_shutdown(Err(anyhow::anyhow!("turn failed")), Ok(())).unwrap_err();
    assert!(error.to_string().contains("turn failed"), "{error}");

    // Cleanup failure without a primary failure is still reported.
    let error =
        combine_turn_and_shutdown(Ok(()), Err(crate::runtime::RuntimeError::ShutdownTimeout))
            .unwrap_err();
    assert!(error.to_string().contains("shutdown timed out"), "{error}");

    combine_turn_and_shutdown(Ok(()), Ok(())).unwrap();
}

#[tokio::test]
async fn broker_renders_compact_skip_once() {
    let (runtime, base) = start_test_runtime("compact-skip").await;
    let recording = std::sync::Arc::new(RecordingDisplay::default());
    let cmd_tx = start_runtime_broker(runtime.handle(), recording.clone());

    cmd_tx.send(RuntimeCmd::Compact).unwrap();
    wait_until(|| {
        recording
            .infos()
            .iter()
            .any(|message| message.contains("Compaction skipped"))
    })
    .await;

    let skipped = recording
        .infos()
        .iter()
        .filter(|message| message.starts_with("Compaction skipped"))
        .count();
    assert_eq!(skipped, 1, "{:?}", recording.infos());

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(base).await;
}

#[tokio::test]
async fn broker_renders_set_model_success_label_and_title_once() {
    let (runtime, base) = start_test_runtime("set-model").await;
    let recording = std::sync::Arc::new(RecordingDisplay::default());
    let cmd_tx = start_runtime_broker(runtime.handle(), recording.clone());

    cmd_tx
        .send(RuntimeCmd::SetModel("flash".to_string()))
        .unwrap();
    wait_until(|| {
        recording
            .infos()
            .iter()
            .any(|message| message.contains("Switched to flash model"))
    })
    .await;

    let switched = recording
        .infos()
        .iter()
        .filter(|message| message.contains("Switched to flash model"))
        .count();
    assert_eq!(switched, 1, "{:?}", recording.infos());
    let titles = recording.titles();
    assert_eq!(titles.len(), 1, "{titles:?}");
    assert_eq!(titles[0].0, "flash");

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(base).await;
}

#[tokio::test]
async fn broker_renders_set_model_error_once() {
    let (runtime, base) = start_test_runtime("set-model-error").await;
    let recording = std::sync::Arc::new(RecordingDisplay::default());
    let cmd_tx = start_runtime_broker(runtime.handle(), recording.clone());

    cmd_tx.send(RuntimeCmd::SetModel(String::new())).unwrap();
    wait_until(|| {
        recording
            .errors()
            .iter()
            .any(|message| message.contains("must not be empty"))
    })
    .await;

    let errors = recording
        .errors()
        .iter()
        .filter(|message| message.contains("must not be empty"))
        .count();
    assert_eq!(errors, 1, "{:?}", recording.errors());

    runtime.shutdown().await.unwrap();
    let _ = tokio::fs::remove_dir_all(base).await;
}
