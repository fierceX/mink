//! PythonSandbox — CPython WASI 沙箱工具。
//!
//! 在 wasmtime + CPython WASI 沙箱中执行 Python 代码。
//! 与宿主 Python 工具不同，此工具提供 WASI 级隔离：
//! - 无 subprocess（WASI 无 execve）
//! - 无网络（WASI 无 socket，除非显式配置）
//! - 无任意文件系统访问（通过配置的 read/write dirs 授权）
//! - 无 C 扩展（WASI 不支持动态链接）
//! - 完整 CPython 标准库

use super::process::ProcessTermination;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use wasmtime::{Config, Engine, Linker, Module, Store};
use wasmtime_wasi::p1::{self, WasiP1Ctx};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtxBuilder};

fn resolve_abs(dir: &str, cwd: &Path) -> PathBuf {
    let p = Path::new(dir);
    if p.is_absolute() {
        p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
    } else {
        cwd.join(p).canonicalize().unwrap_or_else(|_| cwd.join(p))
    }
}

/// Format the CWD-switching preamble using a JSON-escaped Python string literal
/// so quotes, backslashes, or newlines in `cwd` cannot break or inject code.
fn build_chdir_script(cwd: &Path, script: &str) -> Result<String> {
    let cwd_literal = serde_json::to_string(&*cwd.to_string_lossy())?;
    Ok(format!("import os; os.chdir({cwd_literal})\n{script}"))
}

/// Resolve `script` or `script_file` for `PythonSandboxTool`.
///
/// Unlike host `PythonTool`, `script_file` must stay inside the directories
/// authorized for the WASI sandbox (`cwd`, `read_dirs`, `write_dirs`) so
/// specifying an arbitrary host path cannot read files outside the sandbox.
fn resolve_sandbox_script(
    args: &super::process::PythonScriptArgs,
    cwd: &Path,
    read_dirs: &[String],
    write_dirs: &[String],
) -> Result<String> {
    match (&args.script, &args.script_file) {
        (Some(script), None) => Ok(script.clone()),
        (None, Some(path)) => {
            let full_path = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                cwd.join(path)
            };
            let canonical = full_path.canonicalize().with_context(|| {
                format!("Failed to resolve script file {}", full_path.display())
            })?;
            let mut allowed_roots = Vec::with_capacity(1 + read_dirs.len() + write_dirs.len());
            if let Ok(canon_cwd) = cwd.canonicalize() {
                allowed_roots.push(canon_cwd);
            } else {
                allowed_roots.push(cwd.to_path_buf());
            }
            for dir in read_dirs.iter().chain(write_dirs.iter()) {
                allowed_roots.push(resolve_abs(dir, cwd));
            }
            if !allowed_roots.iter().any(|root| canonical.starts_with(root)) {
                bail!(
                    "Error: script_file {} is outside allowed sandbox directories",
                    full_path.display()
                );
            }
            let meta = std::fs::metadata(&canonical).with_context(|| {
                format!("Failed to inspect script file {}", canonical.display())
            })?;
            if !meta.is_file() {
                bail!(
                    "Error: script_file {} must be a regular file",
                    full_path.display()
                );
            }
            std::fs::read_to_string(&canonical).map_err(|error| {
                anyhow::anyhow!(
                    "Failed to read script file {}: {error}",
                    full_path.display()
                )
            })
        }
        (Some(_), Some(_)) => {
            bail!("Error: provide either 'script' or 'script_file', not both")
        }
        (None, None) => bail!("Error: provide either 'script' or 'script_file'"),
    }
}

struct SandboxExecution {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    termination: ProcessTermination,
}

#[derive(Hash, PartialEq, Eq)]
struct WasmModuleCacheKey {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

pub(crate) fn load_compiled_wasm(wasm_path: &Path) -> Result<(Engine, Module)> {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    const MAX_CACHED_MODULES: usize = 4;
    static CACHE: LazyLock<Mutex<HashMap<WasmModuleCacheKey, (Engine, Module)>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    let canonical = wasm_path
        .canonicalize()
        .unwrap_or_else(|_| wasm_path.to_path_buf());
    let meta =
        std::fs::metadata(&canonical).map_err(|e| anyhow::anyhow!("读取 python.wasm 失败: {e}"))?;
    let key = WasmModuleCacheKey {
        path: canonical.clone(),
        len: meta.len(),
        modified: meta.modified().ok(),
    };
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((engine, module)) = cache.get(&key) {
            return Ok((engine.clone(), module.clone()));
        }
    }
    let wasm_bytes =
        std::fs::read(&canonical).map_err(|e| anyhow::anyhow!("读取 python.wasm 失败: {e}"))?;
    let mut engine_config = Config::new();
    engine_config.epoch_interruption(true);
    let engine = Engine::new(&engine_config)?;
    let module = Module::new(&engine, &wasm_bytes)?;
    {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= MAX_CACHED_MODULES {
            cache.clear();
        }
        cache.insert(key, (engine.clone(), module.clone()));
    }
    Ok((engine, module))
}

/// 在 CPython WASI 沙箱中执行 Python 脚本并返回结构化终态。
#[allow(clippy::too_many_arguments)]
fn execute_in_sandbox_detailed(
    script: &str,
    wasm_path: &Path,
    stdlib_dir: &Path,
    read_dirs: &[String],
    write_dirs: &[String],
    package_dirs: &[String],
    cwd: &Path,
    max_output: usize,
    timeout_secs: u64,
    interrupt: Option<&AtomicBool>,
) -> Result<SandboxExecution> {
    let (engine, module) = load_compiled_wasm(wasm_path)?;

    // ── WASI 上下文 ──
    let stdout_pipe = MemoryOutputPipe::new(max_output);
    let stderr_pipe = MemoryOutputPipe::new(max_output);
    let mut builder = WasiCtxBuilder::new();
    builder
        .stdout(stdout_pipe.clone())
        .stderr(stderr_pipe.clone());

    // 注入 os.chdir，使 Python 相对路径解析指向项目根（使用 JSON 转义避免路径注入）
    let script = build_chdir_script(cwd, script)?;

    // 通过 -c 传递代码
    let wasm_argv = [
        "python.wasm".to_string(),
        "-c".to_string(),
        script.to_string(),
    ];
    builder.args(&wasm_argv.iter().map(|s| s.as_str()).collect::<Vec<_>>());

    builder.env("PWD", cwd.to_string_lossy());

    // 映射标准库目录到 /usr/local（CPython WASI 查找 stdlib 的位置）
    if stdlib_dir.exists() {
        builder.preopened_dir(stdlib_dir, "/usr/local", DirPerms::READ, FilePerms::READ)?;
    }

    // 目录挂载：只挂载到绝对路径
    // 顺序：写目录（读写）→ 读目录（只读）→ CWD（只读）
    // wasmtime-wasi preview1 首次匹配，子路径必须在父路径之前
    for dir in write_dirs {
        let abs = resolve_abs(dir, cwd);
        if abs.exists() {
            builder.preopened_dir(
                &abs,
                abs.to_string_lossy(),
                DirPerms::all(),
                FilePerms::all(),
            )?;
        }
    }
    for dir in read_dirs {
        let abs = resolve_abs(dir, cwd);
        if abs.exists() {
            builder.preopened_dir(&abs, abs.to_string_lossy(), DirPerms::READ, FilePerms::READ)?;
        }
    }
    // preopen CWD 到绝对路径（只读），但若 CWD（或其父目录）已在 write_dirs 中则跳过（写权限优先）
    let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let cwd_is_writable = write_dirs.iter().any(|d| {
        let abs = resolve_abs(d, cwd);
        abs.exists() && (abs == canonical_cwd || canonical_cwd.starts_with(&abs))
    });
    if !cwd_is_writable {
        builder.preopened_dir(cwd, cwd.to_string_lossy(), DirPerms::READ, FilePerms::READ)?;
    }

    // 包目录
    let mut pythonpath = Vec::new();
    for (index, dir) in package_dirs.iter().enumerate() {
        let p = Path::new(dir);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        if abs.exists() {
            // Each mount needs its own guest path. Reusing `/packages` makes
            // every PYTHONPATH entry resolve to the first preopen, hiding
            // later configured package directories.
            let guest_path = format!("/packages/{index}");
            builder.preopened_dir(&abs, &guest_path, DirPerms::READ, FilePerms::READ)?;
            pythonpath.push(guest_path);
        }
    }
    if !pythonpath.is_empty() {
        builder.env("PYTHONPATH", pythonpath.join(":"));
    }

    // ── 执行 ──
    let wasi = builder.build_p1();
    let mut store = Store::new(&engine, wasi);
    // Other stores may tick this cached engine. Only this execution's
    // supervisor can ask this store to trap.
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let store_stop = stop.clone();
    store.epoch_deadline_callback(move |_| {
        Ok(if store_stop.load(std::sync::atomic::Ordering::Acquire) {
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

    // 使用非 scoped 线程 + channel 实现可靠超时
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    // 将 store 移入线程，线程负责执行并发送结果
    let handle = std::thread::spawn(move || {
        let result = start.call(&mut store, ());
        let _ = tx.send(result);
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    enum StopCause {
        Timeout,
        Cancelled,
        Disconnected,
    }
    let result = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break Err(StopCause::Timeout);
        }
        if let Some(int) = interrupt
            && int.load(std::sync::atomic::Ordering::SeqCst)
        {
            break Err(StopCause::Cancelled);
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(r) => break Ok(r),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err(StopCause::Disconnected),
        }
    };

    if result.is_err() {
        stop.store(true, std::sync::atomic::Ordering::Release);
        // A concurrent callback may already be renewing its deadline.
        // Keep waking this store until it acknowledges cancellation.
        loop {
            engine.increment_epoch();
            if !matches!(
                rx.recv_timeout(Duration::from_millis(20)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                break;
            }
        }
    }
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("Python sandbox execution thread panicked"))?;

    // 读取输出
    let stdout = String::from_utf8_lossy(&stdout_pipe.contents()).to_string();
    let stderr = String::from_utf8_lossy(&stderr_pipe.contents()).to_string();

    match result {
        Ok(Ok(())) => Ok(SandboxExecution {
            stdout,
            stderr,
            exit_code: Some(0),
            termination: ProcessTermination::Exited(0),
        }),
        Ok(Err(trap)) => {
            let msg = trap.to_string();
            if msg.contains("proc_exit") {
                let code = msg
                    .split("proc_exit(")
                    .nth(1)
                    .and_then(|s| s.split(')').next())
                    .and_then(|s| s.parse::<i32>().ok());
                Ok(SandboxExecution {
                    stdout,
                    stderr,
                    exit_code: code,
                    termination: ProcessTermination::Exited(code.unwrap_or(1)),
                })
            } else if msg.contains("epoch") || msg.contains("interrupt") {
                let timed_out_msg = format!(
                    "[... Python sandbox timed out after {} seconds ...]",
                    timeout_secs
                );
                Ok(SandboxExecution {
                    stdout,
                    stderr: if stderr.is_empty() {
                        timed_out_msg
                    } else {
                        format!("{stderr}\n{timed_out_msg}")
                    },
                    exit_code: None,
                    termination: ProcessTermination::TimedOut,
                })
            } else {
                Ok(SandboxExecution {
                    stdout,
                    stderr,
                    exit_code: Some(1),
                    termination: ProcessTermination::Exited(1),
                })
            }
        }
        Err(cause) => {
            let (exit_code, termination, cancelled_msg) = match cause {
                StopCause::Timeout => (
                    None,
                    ProcessTermination::TimedOut,
                    format!("[... Python sandbox timed out after {timeout_secs} seconds ...]"),
                ),
                StopCause::Cancelled => (
                    None,
                    ProcessTermination::Interrupted,
                    "[... Python sandbox cancelled by user ...]".to_string(),
                ),
                StopCause::Disconnected => (
                    Some(1),
                    ProcessTermination::Exited(1),
                    "[... Python sandbox execution channel disconnected ...]".to_string(),
                ),
            };
            Ok(SandboxExecution {
                stdout,
                stderr: if stderr.is_empty() {
                    cancelled_msg
                } else {
                    format!("{stderr}\n{cancelled_msg}")
                },
                // Timeout/cancel are structured causes, not process exit
                // codes; only the disconnected path carries a real code.
                exit_code,
                termination,
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn execute_in_sandbox_at(
    script: &str,
    wasm_path: &Path,
    stdlib_dir: &Path,
    read_dirs: &[String],
    write_dirs: &[String],
    package_dirs: &[String],
    cwd: &Path,
    max_output: usize,
    timeout_secs: u64,
    interrupt: Option<&AtomicBool>,
) -> Result<(String, String, Option<i32>)> {
    let exec = execute_in_sandbox_detailed(
        script,
        wasm_path,
        stdlib_dir,
        read_dirs,
        write_dirs,
        package_dirs,
        cwd,
        max_output,
        timeout_secs,
        interrupt,
    )?;
    Ok((exec.stdout, exec.stderr, exec.exit_code))
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn execute_in_sandbox(
    script: &str,
    wasm_path: &Path,
    stdlib_dir: &Path,
    read_dirs: &[String],
    write_dirs: &[String],
    package_dirs: &[String],
    timeout_secs: u64,
    interrupt: Option<&AtomicBool>,
) -> Result<(String, String, Option<i32>)> {
    execute_in_sandbox_at(
        script,
        wasm_path,
        stdlib_dir,
        read_dirs,
        write_dirs,
        package_dirs,
        &std::env::current_dir()?,
        100_000,
        timeout_secs,
        interrupt,
    )
}

/// Resolve the effective sandbox timeout: model-provided values must honor the
/// documented 300s ceiling (fail closed; smaller values honored verbatim);
/// 0/absent falls back to the configured default, clamped.
fn resolve_sandbox_timeout(explicit: Option<u64>, configured: u64) -> Result<u64> {
    match explicit {
        Some(t) if t > 0 => {
            anyhow::ensure!(
                t <= 300,
                "Error: timeout must not exceed 300 seconds; got {t}"
            );
            Ok(t)
        }
        _ => Ok(configured.clamp(5, 300)),
    }
}

pub struct PythonSandboxTool;

impl super::runner::ToolExec for PythonSandboxTool {
    fn metadata(&self) -> super::metadata::ToolMetadata {
        super::metadata::ToolMetadata::new(
            "PythonSandbox",
            super::metadata::ApprovalTier::Exec,
            super::metadata::ToolResultKind::Command,
        )
        .storm_exempt()
        // 仅在 enabled_tools 显式列出时进入工具面。
        .explicit_only()
        .mutating()
    }

    fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &crate::context::ToolContext,
    ) -> Result<super::runner::ToolOutcome> {
        let args = super::process::PythonScriptArgs::parse(input)?;
        let sp_cfg = &ctx.tool_config.sandbox_python;
        let script =
            resolve_sandbox_script(&args, &ctx.cwd, &sp_cfg.read_dirs, &sp_cfg.write_dirs)?;
        let timeout = resolve_sandbox_timeout(args.timeout, sp_cfg.timeout)?;

        let wasm_path = Path::new(&sp_cfg.wasm_path);
        if !wasm_path.exists() {
            return Ok(super::runner::ToolOutcome::text(format!(
                "Error: python.wasm not found at {}. Set sandbox_python.wasm_path in .minkrc or ensure the file exists.",
                sp_cfg.wasm_path
            )));
        }

        let stdlib_dir = Path::new(&sp_cfg.stdlib_dir);

        let exec = execute_in_sandbox_detailed(
            &script,
            wasm_path,
            stdlib_dir,
            &sp_cfg.read_dirs,
            &sp_cfg.write_dirs,
            &sp_cfg.package_dirs,
            &ctx.cwd,
            ctx.tool_config.tool_result_max_bytes,
            timeout,
            Some(ctx.interrupt.as_ref()),
        )?;

        let mut content = exec.stdout;
        if !exec.stderr.is_empty() {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&exec.stderr);
        }
        if let Some(code) = exec.exit_code
            && code != 0
        {
            content.push_str(&format!(
                "\n\nPython sandbox script exited with code {code}."
            ));
        }

        Ok(super::runner::ToolOutcome {
            content,
            conversation_content: String::new(),
            is_bash: false,
            exit_code: exec.exit_code,
            success: exec.exit_code == Some(0),
            no_mutation: false,
            memo_candidate: None,
            diagnostics: Vec::new(),
            plan_command: None,
            state_metadata: None,
            presentation: None,
            structured_value: None,
            termination: Some(exec.termination),
        })
    }
}

#[cfg(test)]
#[path = "sandbox_python_tests.rs"]
mod tests;
