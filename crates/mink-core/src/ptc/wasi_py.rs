//! In-process CPython WASI sandbox PTC runtime (`python-sandbox` feature).
//!
//! Bridges the NDJSON control protocol over WASI stdin/stdout duplex streams
//! to `PtcBindingDispatcher`, sharing the cached Wasmtime Engine/Module and
//! epoch-interruption mechanism from `tools::sandbox_python`.

use super::PtcConfig;
use super::protocol::{finalize_run_result, resolve_timeout_and_cwd, run_control_loop};
use super::runtime::{
    PtcBindingDispatcher, PtcFailureKind, PtcRunFailure, PtcRunRequest, PtcRunResult, PtcRunSpec,
    PtcRuntime,
};
use crate::config::SandboxPythonConfig;
use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::BufReader;
use wasmtime::{Linker, Store};
use wasmtime_wasi::cli::{AsyncStdinStream, AsyncStdoutStream};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

const BOOTSTRAP_PY: &str = include_str!("bootstrap.py");

#[derive(Clone)]
pub struct WasiPythonPtcRuntime {
    config: PtcConfig,
    sandbox_python: SandboxPythonConfig,
}

impl WasiPythonPtcRuntime {
    pub fn new(config: PtcConfig, sandbox_python: SandboxPythonConfig) -> Self {
        Self {
            config,
            sandbox_python,
        }
    }
}

fn resolve_abs(dir: &str, cwd: &Path) -> PathBuf {
    let p = Path::new(dir);
    if p.is_absolute() {
        p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
    } else {
        cwd.join(p).canonicalize().unwrap_or_else(|_| cwd.join(p))
    }
}

#[allow(clippy::double_must_use)]
#[async_trait]
impl PtcRuntime for WasiPythonPtcRuntime {
    fn kind(&self) -> super::PtcBackendKind {
        super::PtcBackendKind::WasiPython
    }

    fn resolve(&self, request: PtcRunRequest) -> Result<PtcRunSpec> {
        resolve_timeout_and_cwd(&self.config, request)
    }

    async fn run(
        &self,
        spec: PtcRunSpec,
        dispatcher: Arc<dyn PtcBindingDispatcher>,
    ) -> Result<PtcRunResult> {
        let wasm_path = Path::new(&self.sandbox_python.wasm_path);
        if !wasm_path.exists() {
            return Ok(PtcRunResult {
                output: Vec::new(),
                output_truncated: false,
                error: Some(PtcRunFailure {
                    kind: PtcFailureKind::SandboxUnavailable,
                    message: format!("python.wasm not found at {}", self.sandbox_python.wasm_path),
                }),
            });
        }

        let wasm_path_owned = wasm_path.to_path_buf();
        let (engine, module) = tokio::task::spawn_blocking(move || {
            crate::tools::sandbox_python::load_compiled_wasm(&wasm_path_owned)
        })
        .await
        .map_err(|e| anyhow::anyhow!("WASI Python load task failed: {e}"))??;
        let (host_to_guest_w, guest_stdin_r) = tokio::io::duplex(64 * 1024);
        let (guest_stdout_w, host_from_guest_r) = tokio::io::duplex(64 * 1024);
        let stderr_pipe = MemoryOutputPipe::new(256 * 1024);

        let mut builder = WasiCtxBuilder::new();
        builder
            .stdin(AsyncStdinStream::new(guest_stdin_r))
            .stdout(AsyncStdoutStream::new(1024 * 1024, guest_stdout_w))
            .stderr(stderr_pipe.clone());

        let wasm_argv = [
            "python.wasm".to_string(),
            "-B".to_string(),
            "-W".to_string(),
            "ignore".to_string(),
            "-c".to_string(),
            BOOTSTRAP_PY.to_string(),
        ];
        builder.args(&wasm_argv.iter().map(String::as_str).collect::<Vec<_>>());
        builder
            .env("PWD", spec.cwd.to_string_lossy())
            .env("PYTHONUTF8", "1")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("MINK_PTC_STDIO_CTRL", "1");

        let stdlib_dir = Path::new(&self.sandbox_python.stdlib_dir);
        if stdlib_dir.exists() {
            builder.preopened_dir(stdlib_dir, "/usr/local", DirPerms::READ, FilePerms::READ)?;
        }
        for dir in &self.sandbox_python.write_dirs {
            let abs = resolve_abs(dir, &spec.cwd);
            if abs.exists() {
                builder.preopened_dir(
                    &abs,
                    abs.to_string_lossy(),
                    DirPerms::all(),
                    FilePerms::all(),
                )?;
            }
        }
        for dir in &self.sandbox_python.read_dirs {
            let abs = resolve_abs(dir, &spec.cwd);
            if abs.exists() {
                builder.preopened_dir(
                    &abs,
                    abs.to_string_lossy(),
                    DirPerms::READ,
                    FilePerms::READ,
                )?;
            }
        }
        let canonical_cwd = spec.cwd.canonicalize().unwrap_or_else(|_| spec.cwd.clone());
        let cwd_is_writable = self.sandbox_python.write_dirs.iter().any(|d| {
            let abs = resolve_abs(d, &spec.cwd);
            abs.exists() && (abs == canonical_cwd || canonical_cwd.starts_with(&abs))
        });
        if !cwd_is_writable && spec.cwd.exists() {
            builder.preopened_dir(
                &spec.cwd,
                spec.cwd.to_string_lossy(),
                DirPerms::READ,
                FilePerms::READ,
            )?;
        }
        let mut pythonpath = Vec::new();
        for (index, dir) in self.sandbox_python.package_dirs.iter().enumerate() {
            let p = Path::new(dir);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                spec.cwd.join(p)
            };
            if abs.exists() {
                let guest_path = format!("/packages/{index}");
                builder.preopened_dir(&abs, &guest_path, DirPerms::READ, FilePerms::READ)?;
                pythonpath.push(guest_path);
            }
        }
        if !pythonpath.is_empty() {
            builder.env("PYTHONPATH", pythonpath.join(":"));
        }

        let wasi = builder.build_p1();
        let mut store = Store::new(&engine, wasi);
        let stop = Arc::new(AtomicBool::new(false));
        let store_stop = stop.clone();
        store.epoch_deadline_callback(move |_| {
            Ok(if store_stop.load(Ordering::Acquire) {
                wasmtime::UpdateDeadline::Interrupt
            } else {
                wasmtime::UpdateDeadline::Continue(1)
            })
        });
        store.set_epoch_deadline(1);
        let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
        p1::add_to_linker_sync(&mut linker, |ctx| ctx)?;

        let instance = linker.instantiate(&mut store, &module)?;
        let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;

        let (wasm_done_tx, mut wasm_done_rx) = tokio::sync::oneshot::channel();
        let wasm_thread = std::thread::spawn(move || {
            let result = start.call(&mut store, ());
            let _ = wasm_done_tx.send(result);
        });

        let mut outcome = run_control_loop(
            &self.config,
            &spec,
            &dispatcher,
            BufReader::new(host_from_guest_r),
            host_to_guest_w,
            None,
        )
        .await;

        if tokio::time::timeout(Duration::from_millis(300), &mut wasm_done_rx)
            .await
            .is_err()
        {
            stop.store(true, Ordering::Release);
            loop {
                engine.increment_epoch();
                if tokio::time::timeout(Duration::from_millis(20), &mut wasm_done_rx)
                    .await
                    .is_ok()
                {
                    break;
                }
            }
        }
        let _ = tokio::task::spawn_blocking(move || {
            let _ = wasm_thread.join();
        })
        .await;

        let stderr = String::from_utf8_lossy(&stderr_pipe.contents()).to_string();
        if !stderr.trim().is_empty() && outcome.ledger.push_text(&stderr) {
            outcome.output_truncated = true;
        }

        Ok(finalize_run_result(
            outcome,
            "WASI PTC guest exited without reporting completion".to_string(),
        ))
    }
}
