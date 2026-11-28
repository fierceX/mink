//! Managed `python3` subprocess PTC runtime with inherited FD 3 control channel,
//! optional OS sandbox wrapping, bounded output ledger, and process-group cleanup.

use super::PtcConfig;
use super::protocol::{finalize_run_result, resolve_timeout_and_cwd, run_control_loop};
use super::runtime::{
    PtcBindingDispatcher, PtcFailureKind, PtcRunFailure, PtcRunRequest, PtcRunResult, PtcRunSpec,
    PtcRuntime,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::mpsc;

const BOOTSTRAP_PY: &str = include_str!("bootstrap.py");
#[cfg(unix)]
const CTRL_FD: i32 = 3;

#[derive(Clone)]
pub struct SubprocessPythonPtcRuntime {
    config: PtcConfig,
}

impl SubprocessPythonPtcRuntime {
    pub fn new(config: PtcConfig) -> Self {
        Self { config }
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn executable(&self) -> &str {
        self.config
            .python_executable
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("python3")
    }
}

#[allow(clippy::double_must_use)]
#[async_trait]
impl PtcRuntime for SubprocessPythonPtcRuntime {
    fn kind(&self) -> super::PtcBackendKind {
        super::PtcBackendKind::SubprocessPython
    }

    fn resolve(&self, request: PtcRunRequest) -> Result<PtcRunSpec> {
        resolve_timeout_and_cwd(&self.config, request)
    }

    async fn run(
        &self,
        spec: PtcRunSpec,
        dispatcher: Arc<dyn PtcBindingDispatcher>,
    ) -> Result<PtcRunResult> {
        #[cfg(not(unix))]
        {
            let _ = (spec, dispatcher);
            return Ok(PtcRunResult {
                output: Vec::new(),
                output_truncated: false,
                error: Some(PtcRunFailure {
                    kind: PtcFailureKind::SandboxUnavailable,
                    message: "subprocess python PTC requires Unix socketpair support".to_string(),
                }),
            });
        }

        #[cfg(unix)]
        {
            self.run_unix(spec, dispatcher).await
        }
    }
}

#[cfg(unix)]
impl SubprocessPythonPtcRuntime {
    async fn run_unix(
        &self,
        spec: PtcRunSpec,
        dispatcher: Arc<dyn PtcBindingDispatcher>,
    ) -> Result<PtcRunResult> {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::net::UnixStream as StdUnixStream;
        use std::os::unix::process::CommandExt;

        let (parent_sock, child_sock) =
            StdUnixStream::pair().context("failed to create PTC control socketpair")?;
        parent_sock.set_nonblocking(true)?;
        let parent_tokio = tokio::net::UnixStream::from_std(parent_sock)?;
        let (ctrl_read_half, ctrl_write) = tokio::io::split(parent_tokio);
        let ctrl_reader = BufReader::new(ctrl_read_half);

        let exe = self.executable();
        let python_args = vec![
            exe.to_string(),
            "-B".to_string(),
            "-W".to_string(),
            "ignore".to_string(),
            "-c".to_string(),
            BOOTSTRAP_PY.to_string(),
        ];

        let (program, argv) = if let Some(sb) = spec.sandbox.as_ref().filter(|s| s.is_active())
            && std::env::var_os("MINK_SANDBOXED").is_none()
        {
            match crate::sandbox::wrap_child_argv(
                sb,
                Path::new(exe),
                &python_args,
                &spec.cwd,
                Some(CTRL_FD),
            ) {
                Ok(mut wrapped) => {
                    let prog = wrapped.remove(0);
                    (prog, wrapped)
                }
                Err(reason) => {
                    return Ok(PtcRunResult {
                        output: Vec::new(),
                        output_truncated: false,
                        error: Some(PtcRunFailure {
                            kind: PtcFailureKind::SandboxUnavailable,
                            message: format!("PTC sandbox unavailable: {reason}"),
                        }),
                    });
                }
            }
        } else {
            let mut rest = python_args;
            let prog = rest.remove(0);
            (prog, rest)
        };

        let mut std_cmd = std::process::Command::new(&program);
        std_cmd
            .args(&argv)
            .current_dir(&spec.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();

        for key in ["PATH", "TMPDIR", "TEMP", "TMP", "HOME", "LANG", "LC_ALL"] {
            if let Some(val) = std::env::var_os(key) {
                std_cmd.env(key, val);
            }
        }
        std_cmd
            .env("PYTHONUTF8", "1")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("MINK_PTC_CTRL_FD", CTRL_FD.to_string());

        let child_raw_fd = child_sock.as_raw_fd();
        unsafe {
            std_cmd.pre_exec(move || {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if child_raw_fd != CTRL_FD && libc::dup2(child_raw_fd, CTRL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let flags = libc::fcntl(CTRL_FD, libc::F_GETFD);
                if flags == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(CTRL_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut tokio_cmd = tokio::process::Command::from(std_cmd);
        let mut child = match tokio_cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Ok(PtcRunResult {
                    output: Vec::new(),
                    output_truncated: false,
                    error: Some(PtcRunFailure {
                        kind: PtcFailureKind::WorkerExit,
                        message: format!("failed to start {program}: {error}"),
                    }),
                });
            }
        };
        // Parent drops its copy of the child socket immediately so EOF works reliably.
        drop(child_sock);

        let (stream_tx, mut stream_rx) = mpsc::channel::<String>(64);
        let stdout_task = child
            .stdout
            .take()
            .map(|pipe| spawn_pipe_reader(pipe, stream_tx.clone()));
        let stderr_task = child
            .stderr
            .take()
            .map(|pipe| spawn_pipe_reader(pipe, stream_tx.clone()));
        drop(stream_tx);

        let mut outcome = run_control_loop(
            &self.config,
            &spec,
            &dispatcher,
            ctrl_reader,
            ctrl_write,
            Some(&mut stream_rx),
        )
        .await;

        let exit_status = if outcome.guest_done {
            match tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
                Ok(Ok(status)) => Some(status),
                _ => {
                    kill_tokio_child_group(&mut child).await;
                    None
                }
            }
        } else {
            kill_tokio_child_group(&mut child).await;
            None
        };

        if let Some(t) = stdout_task {
            let _ = tokio::time::timeout(Duration::from_millis(200), t).await;
        }
        if let Some(t) = stderr_task {
            let _ = tokio::time::timeout(Duration::from_millis(200), t).await;
        }
        outcome.drain_aux_stream(&mut stream_rx);
        let missing_done = match exit_status {
            Some(status) => format!("PTC Python worker exited unexpectedly ({status})"),
            None => "PTC Python worker terminated without reporting completion".to_string(),
        };
        if !outcome.guest_done && outcome.terminal_error.is_none() {
            outcome.terminal_error = Some(PtcRunFailure {
                kind: PtcFailureKind::WorkerExit,
                message: missing_done.clone(),
            });
        }
        Ok(finalize_run_result(outcome, missing_done))
    }
}

#[cfg(unix)]
fn spawn_pipe_reader<R: AsyncReadExt + Unpin + Send + 'static>(
    mut pipe: R,
    tx: mpsc::Sender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut chunk = vec![0u8; 8192];
        loop {
            match pipe.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&chunk[..n]).to_string();
                    if tx.send(text).await.is_err() {
                        break;
                    }
                }
            }
        }
    })
}

#[cfg(unix)]
async fn kill_tokio_child_group(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id()
        && let Ok(pid_i32) = i32::try_from(pid)
        && pid_i32 > 0
    {
        unsafe {
            libc::kill(-pid_i32, libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
    let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::{CancellationToken, InterruptSignal};
    use crate::ptc::runtime::{PtcBindingError, PtcBindingSuccess, PtcOutputItem};
    use crate::tools::metadata::ToolFailureKind;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockDispatcher {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl PtcBindingDispatcher for MockDispatcher {
        async fn invoke(
            &self,
            tool_name: &str,
            args: serde_json::Value,
        ) -> std::result::Result<PtcBindingSuccess, PtcBindingError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match tool_name {
                "Echo" => Ok(PtcBindingSuccess {
                    value: serde_json::json!({"echo": args}),
                    inspection_text: Some(format!("Echoed: {args}")),
                    image: None,
                }),
                "Fail" => Err(PtcBindingError {
                    tool_name: "Fail".to_string(),
                    kind: ToolFailureKind::ProcessFailed,
                    message: "boom".to_string(),
                }),
                other => Err(PtcBindingError {
                    tool_name: other.to_string(),
                    kind: ToolFailureKind::Unknown,
                    message: "unknown".to_string(),
                }),
            }
        }
    }

    fn test_runtime() -> SubprocessPythonPtcRuntime {
        SubprocessPythonPtcRuntime::new(PtcConfig {
            mode: super::super::PtcMode::Exclusive,
            ..PtcConfig::default()
        })
    }

    #[tokio::test]
    async fn executes_top_level_await_gather_inspect_and_tool_call_error() {
        let rt = test_runtime();
        let cwd = std::env::current_dir().unwrap();
        let spec = rt
            .resolve(PtcRunRequest {
                code: r#"
res_a, res_b = await gather(
    tools.Echo(msg="one"),
    tools.Echo({"msg": "two"}),
)
inspect(res_a)
inspect(res_b["echo"]["msg"])
try:
    await tools.Fail()
except ToolCallError as err:
    inspect(f"caught:{err.tool_name}:{err.message}")
print("from-stdout")
"#
                .to_string(),
                cwd,
                tool_names: vec!["Echo".into(), "Fail".into()],
                timeout_secs: Some(15),
                cancel: CancellationToken::new(),
                interrupt: InterruptSignal::new(),
                sandbox: None,
            })
            .unwrap();
        let disp = Arc::new(MockDispatcher {
            calls: AtomicUsize::new(0),
        });
        let result = rt.run(spec, disp.clone()).await.unwrap();
        assert!(
            result.error.is_none(),
            "unexpected error: {:?}",
            result.error
        );
        assert_eq!(disp.calls.load(Ordering::SeqCst), 3);
        let text: String = result
            .output
            .iter()
            .filter_map(|item| match item {
                PtcOutputItem::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("Echoed:"), "{text}");
        assert!(text.contains("two"), "{text}");
        assert!(text.contains("caught:Fail:boom"), "{text}");
        assert!(text.contains("from-stdout"), "{text}");
    }

    #[tokio::test]
    async fn timeout_kills_busy_loop_cleanly() {
        let rt = SubprocessPythonPtcRuntime::new(PtcConfig {
            mode: super::super::PtcMode::Exclusive,
            timeout_secs: 5,
            max_timeout_secs: 30,
            stop_grace_ms: 150,
            ..PtcConfig::default()
        });
        let cwd = std::env::current_dir().unwrap();
        let mut spec = rt
            .resolve(PtcRunRequest {
                code: "while True:\n    pass\n".to_string(),
                cwd,
                tool_names: vec![],
                timeout_secs: Some(5),
                cancel: CancellationToken::new(),
                interrupt: InterruptSignal::new(),
                sandbox: None,
            })
            .unwrap();
        spec.timeout = Duration::from_millis(250);
        let disp = Arc::new(MockDispatcher {
            calls: AtomicUsize::new(0),
        });
        let result = rt.run(spec, disp).await.unwrap();
        let err = result.error.expect("must timeout");
        assert_eq!(err.kind, PtcFailureKind::Timeout);
    }

    #[tokio::test]
    async fn cancel_aborts_even_inside_try_except_exception() {
        let rt = test_runtime();
        let cwd = std::env::current_dir().unwrap();
        let cancel = CancellationToken::new();
        let spec = rt
            .resolve(PtcRunRequest {
                code: r#"
import asyncio
try:
    await asyncio.sleep(30)
except Exception:
    inspect("swallowed")
"#
                .to_string(),
                cwd,
                tool_names: vec![],
                timeout_secs: Some(15),
                cancel: cancel.clone(),
                interrupt: InterruptSignal::new(),
                sandbox: None,
            })
            .unwrap();
        let cancel_bg = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel_bg.cancel();
        });
        let disp = Arc::new(MockDispatcher {
            calls: AtomicUsize::new(0),
        });
        let result = rt.run(spec, disp).await.unwrap();
        let err = result.error.expect("must abort");
        assert_eq!(err.kind, PtcFailureKind::Abort);
        let text: String = result
            .output
            .iter()
            .filter_map(|item| match item {
                PtcOutputItem::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(!text.contains("swallowed"), "{text}");
    }

    #[tokio::test]
    async fn huge_inspect_chunks_cleanly_tuple_extracts_image_and_huge_int_is_value_error() {
        let rt = SubprocessPythonPtcRuntime::new(PtcConfig {
            mode: super::super::PtcMode::Exclusive,
            max_output_bytes: 1024,
            max_message_bytes: 4096,
            ..PtcConfig::default()
        });
        let cwd = std::env::current_dir().unwrap();
        let spec = rt
            .resolve(PtcRunRequest {
                code: r#"
try:
    await tools.Echo(huge=1 << 100)
except ValueError as exc:
    inspect("int-caught")
inspect("x" * 20000)
"#
                .to_string(),
                cwd: cwd.clone(),
                tool_names: vec!["Echo".into()],
                timeout_secs: Some(15),
                cancel: CancellationToken::new(),
                interrupt: InterruptSignal::new(),
                sandbox: None,
            })
            .unwrap();
        let disp = Arc::new(MockDispatcher {
            calls: AtomicUsize::new(0),
        });
        let res = rt.run(spec, disp).await.unwrap();
        assert!(res.error.is_none(), "unexpected error: {:?}", res.error);
        assert!(res.output_truncated);
        let text: String = res
            .output
            .iter()
            .filter_map(|item| match item {
                PtcOutputItem::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("int-caught"), "{text}");
        assert!(
            text.len() <= 1024,
            "truncated text exceeded cap: {}",
            text.len()
        );

        // Tuple traversal emits nested Image handles.
        struct ImgDisp;
        #[async_trait]
        impl PtcBindingDispatcher for ImgDisp {
            async fn invoke(
                &self,
                _tool_name: &str,
                _args: serde_json::Value,
            ) -> std::result::Result<PtcBindingSuccess, PtcBindingError> {
                Ok(PtcBindingSuccess {
                    value: serde_json::json!({}),
                    inspection_text: None,
                    image: Some(crate::ptc::PtcImageDescriptor {
                        image_id: "img_tuple_1".into(),
                        path: "pic.png".into(),
                        format: "png".into(),
                        width: 10,
                        height: 10,
                        bytes: 42,
                    }),
                })
            }
        }
        let tuple_spec = rt
            .resolve(PtcRunRequest {
                code: r#"
img = await tools.Read(path="pic.png")
inspect((img, "caption"))
"#
                .to_string(),
                cwd,
                tool_names: vec!["Read".into()],
                timeout_secs: Some(15),
                cancel: CancellationToken::new(),
                interrupt: InterruptSignal::new(),
                sandbox: None,
            })
            .unwrap();
        let tuple_res = rt.run(tuple_spec, Arc::new(ImgDisp)).await.unwrap();
        assert!(
            tuple_res.error.is_none(),
            "tuple image error: {:?}",
            tuple_res.error
        );
        assert!(tuple_res.output.iter().any(
            |item| matches!(item, PtcOutputItem::Image { image_id } if image_id == "img_tuple_1")
        ));
    }

    #[tokio::test]
    async fn supports_from_tools_import_and_import_tools_syntaxes() {
        let rt = test_runtime();
        let cwd = std::env::current_dir().unwrap();
        let spec = rt
            .resolve(PtcRunRequest {
                code: r#"
from tools import Echo
import tools as t
a = await Echo(via="from-import")
b = await t.Echo(via="module-import")
inspect(a["echo"]["via"], b["echo"]["via"])
"#
                .to_string(),
                cwd,
                tool_names: vec!["Echo".into()],
                timeout_secs: Some(15),
                cancel: CancellationToken::new(),
                interrupt: InterruptSignal::new(),
                sandbox: None,
            })
            .unwrap();
        let disp = Arc::new(MockDispatcher {
            calls: AtomicUsize::new(0),
        });
        let res = rt.run(spec, disp.clone()).await.unwrap();
        assert!(res.error.is_none(), "unexpected error: {:?}", res.error);
        assert_eq!(disp.calls.load(Ordering::SeqCst), 2);
        let text: String = res
            .output
            .iter()
            .filter_map(|item| match item {
                PtcOutputItem::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("from-import"), "{text}");
        assert!(text.contains("module-import"), "{text}");
    }

    #[tokio::test]
    async fn supports_python_3_10_runtime_features_without_leaking_future_annotations() {
        let rt = test_runtime();
        let cwd = std::env::current_dir().unwrap();
        let spec = rt
            .resolve(PtcRunRequest {
                code: r#"
def format_tag(tag: str | None) -> str:
    return tag or "none"

# User code should keep native Python 3.10+ annotation evaluation (dont_inherit=True).
assert format_tag.__annotations__["return"] is str
res = await tools.Echo(tag=format_tag("ok"))
assert isinstance(res, dict | list)
pairs = list(zip(["k"], [res["echo"]["tag"]], strict=True))
match pairs:
    case [("k", val)]:
        inspect(f"matched:{val}")
    case _:
        raise AssertionError("unexpected match")
"#
                .to_string(),
                cwd,
                tool_names: vec!["Echo".into()],
                timeout_secs: Some(15),
                cancel: CancellationToken::new(),
                interrupt: InterruptSignal::new(),
                sandbox: None,
            })
            .unwrap();
        let disp = Arc::new(MockDispatcher {
            calls: AtomicUsize::new(0),
        });
        let res = rt.run(spec, disp.clone()).await.unwrap();
        assert!(
            res.error.is_none(),
            "unexpected error: {:?} (output: {:?})",
            res.error,
            res.output
        );
        assert_eq!(disp.calls.load(Ordering::SeqCst), 1);
        let text: String = res
            .output
            .iter()
            .filter_map(|item| match item {
                PtcOutputItem::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("matched:ok"), "{text}");
    }

    #[tokio::test]
    async fn rejects_python_below_3_10_with_clear_version_diagnostic() {
        let candidate = std::env::var("MINK_TEST_OLD_PYTHON")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                Path::new("/usr/bin/python3")
                    .exists()
                    .then(|| "/usr/bin/python3".to_string())
            });
        let Some(old_py) = candidate else {
            return;
        };
        let is_old = std::process::Command::new(&old_py)
            .args([
                "-c",
                "import sys; raise SystemExit(0 if sys.version_info < (3, 10) else 1)",
            ])
            .status()
            .is_ok_and(|s| s.success());
        if !is_old {
            return;
        }

        let cfg = PtcConfig {
            mode: super::super::PtcMode::Exclusive,
            python_executable: Some(old_py),
            ..PtcConfig::default()
        };
        let sp = crate::config::SandboxPythonConfig::default();
        let validate_err = cfg.validate(&sp).unwrap_err().to_string();
        assert!(
            validate_err.contains("requires Python >= 3.10"),
            "{validate_err}"
        );

        // Even if SubprocessPythonPtcRuntime is called directly, bootstrap.py fails closed with a clear diagnostic.
        let rt = SubprocessPythonPtcRuntime::new(cfg);
        let cwd = std::env::current_dir().unwrap();
        let spec = rt
            .resolve(PtcRunRequest {
                code: "inspect(1)".to_string(),
                cwd,
                tool_names: vec![],
                timeout_secs: Some(15),
                cancel: CancellationToken::new(),
                interrupt: InterruptSignal::new(),
                sandbox: None,
            })
            .unwrap();
        let disp = Arc::new(MockDispatcher {
            calls: AtomicUsize::new(0),
        });
        let res = rt.run(spec, disp).await.unwrap();
        assert_eq!(
            res.error.as_ref().map(|e| e.kind),
            Some(PtcFailureKind::WorkerExit)
        );
        let text: String = res
            .output
            .iter()
            .filter_map(|item| match item {
                PtcOutputItem::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("PTC requires Python >= 3.10"), "{text}");
    }
}
