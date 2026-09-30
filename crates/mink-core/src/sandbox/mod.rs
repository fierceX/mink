//! Sandboxing via OS-native tools.
//!
//! On Linux, tries nsjail first, then bubblewrap.
//! On macOS, uses the built-in sandbox-exec.
//!
//! The core function is [`reexec_in_sandbox`] which replaces the current
//! process image with the same binary running inside a sandbox.
//! It sets `MINK_SANDBOXED=1` to prevent infinite re-exec loops.

use crate::config::SandboxConfig;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

#[cfg(target_os = "linux")]
mod platform_linux;
#[cfg(target_os = "macos")]
mod platform_macos;

/// Try to re-execute the current process inside a sandbox.
///
/// On success, this function does NOT return — the process is replaced.
/// On failure (sandbox tool not found, unsupported backend, etc.) the
/// process **exits with code 1**: 沙箱不可用时绝不静默降级运行。
///
/// `exe` is the path to the current binary.
/// `args` are the original command-line arguments (including argv[0]).
pub fn reexec_in_sandbox(config: &SandboxConfig, exe: &Path, args: &[String]) {
    // Prevent infinite re-exec loop
    if std::env::var("MINK_SANDBOXED").is_ok() {
        return;
    }

    if !config.is_active() {
        return;
    }

    // Set the guard *before* exec — if exec fails, the env var remains
    // but that's fine because the process continues and exits normally.
    // Safety: setting environment variable before process replacement is safe
    // in single-threaded context (we haven't spawned any threads yet).
    unsafe {
        std::env::set_var("MINK_SANDBOXED", "1");
    }

    // Linux：父进程（宿主 / agent）死亡时让本进程立即收到 SIGKILL，避免沙箱监工
    // （bwrap / nsjail）成为孤儿；与 bwrap 的 `--die-with-parent` 配合可整棵树回收
    // （父死 → 监工死 → 命名空间 PID1 死 → 命名空间内全部进程死）。
    // 边界（不宣称已覆盖全部情况）：
    // - prctl 的“父”是创建本进程的线程；embed 场景由常驻运行时线程创建。
    // - 带 setuid/setgid/capabilities 的 exec 可能清除该设置；本进程只 exec 同用户的
    //   bwrap/nsjail，不依赖对任意外部二进制的继承。
    // - 注册是两步操作，仍有极小竞态窗口；这里做“返回值检查 + 注册后父身份复检”，
    //   已能拦住父在本进程注册前已死亡的常见情形；彻底关闭需启动协议携带 owner 身份。
    #[cfg(target_os = "linux")]
    unsafe {
        let expected_parent = libc::getppid();
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
            eprintln!(
                "[mink] Fatal: PR_SET_PDEATHSIG failed ({}); refusing to run with unknown parent-death behavior",
                std::io::Error::last_os_error()
            );
            std::process::exit(1);
        }
        let current_parent = libc::getppid();
        if current_parent != expected_parent {
            eprintln!(
                "[mink] Fatal: parent changed while PR_SET_PDEATHSIG was being registered; refusing to continue"
            );
            std::process::exit(1);
        }
    }

    let result = try_reexec(config, exe, args);

    // If we get here, sandbox exec failed — hard fail instead of silent fallback
    match result {
        Ok(()) => {
            // exec succeeded and replaced us, so this is unreachable.
            // But if it didn't replace us, it means exec failed silently.
            eprintln!("[mink] Fatal: sandbox exec returned unexpectedly");
        }
        Err(e) => {
            eprintln!("[mink] Fatal: sandbox unavailable ({}), exiting", e);
        }
    }
    std::process::exit(1);
}

#[allow(unused_variables, dead_code)] // 非 Linux/macOS 平台（如 FreeBSD）无 sandbox 实现
fn try_reexec(config: &SandboxConfig, exe: &Path, args: &[String]) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        if config.backend == "nsjail" || config.backend == "auto" {
            match platform_linux::try_nsjail(config, exe, args) {
                Ok(cmd) => {
                    exec_cmd(&cmd)?;
                    return Err("nsjail exec returned unexpectedly".into());
                }
                Err(e) => {
                    if config.backend == "nsjail" {
                        return Err(e); // explicit backend request → hard error
                    }
                    // auto mode: fall through to bwrap
                    eprintln!("[mink] nsjail not available: {e}");
                }
            }
        }

        if config.backend == "bwrap" || config.backend == "auto" {
            match platform_linux::try_bwrap(config, exe, args) {
                Ok(cmd) => {
                    exec_cmd(&cmd)?;
                    return Err("bwrap exec returned unexpectedly".into());
                }
                Err(e) => {
                    return Err(format!("bwrap: {e}"));
                }
            }
        }

        Err("no Linux sandbox backend available (tried nsjail, bwrap)".into())
    }

    #[cfg(target_os = "macos")]
    {
        // macOS 只有 sandbox-exec 一个实现：显式请求其他 backend 是配置
        // 错误，硬失败而不是静默回退（与 Linux 的显式 backend 语义一致）。
        if config.backend != "auto" && config.backend != "sandbox-exec" {
            return Err(format!(
                "sandbox backend '{}' is not available on macOS (only sandbox-exec)",
                config.backend
            ));
        }
        match platform_macos::try_sandbox_exec(config, exe, args) {
            Ok(cmd) => {
                exec_cmd(&cmd)?;
                Err("sandbox-exec returned unexpectedly".into())
            }
            Err(e) => Err(e),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err("sandbox not supported on this platform".into())
    }
}

/// Replace the current process with the given command.
/// Does NOT return on success.
#[allow(dead_code)] // FreeBSD 等无 sandbox 平台不调用
fn exec_cmd(cmd: &[String]) -> Result<(), String> {
    let (prog, args) = cmd
        .split_first()
        .ok_or_else(|| "empty sandbox command".to_string())?;
    let err = Command::new(prog).args(args).exec();
    Err(format!("exec({prog}) failed: {err}"))
}

/// Linux bind mount 路径解析：绝对→原样，相对→与 cwd 拼接。纯函数，
/// 便于跨平台参数测试（macOS 的 sandbox-exec 另有带规范化的实现）。
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn resolve_dir_lexical(dir: &str, cwd: &Path) -> String {
    let p = Path::new(dir);
    if p.is_absolute() {
        p.display().to_string()
    } else {
        cwd.join(p).display().to_string()
    }
}

/// 构造 bwrap 完整 argv（含 "bwrap"）。纯函数：环境值（cwd/HOME/MINK_HOME）由调用方
/// 解析后传入，便于在任何平台做参数构造回归测试（真实执行仍在 Linux）。
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn bwrap_argv(
    config: &SandboxConfig,
    exe: &Path,
    args: &[String],
    cwd: &Path,
    home: Option<&str>,
    mink_home: Option<&str>,
) -> Vec<String> {
    let mut cmd: Vec<String> = vec!["bwrap".into()];

    // ── Minimal filesystem skeleton ──────────────────────────
    cmd.push("--dev".into());
    cmd.push("/dev".into());
    cmd.push("--proc".into());
    cmd.push("/proc".into());
    cmd.push("--tmpfs".into());
    cmd.push("/tmp".into());

    // ── Essential paths (binary + system libraries) ──────────
    // Without these the dynamically-linked binary won't even start.
    if let Some(parent) = exe.parent() {
        let parent_str = parent.display().to_string();
        if !parent_str.is_empty() {
            cmd.push("--ro-bind".into());
            cmd.push(parent_str.clone());
            cmd.push(parent_str);
        }
    }
    for d in ["/usr", "/lib", "/lib64", "/etc", "/run"] {
        cmd.push("--ro-bind".into());
        cmd.push(d.to_string());
        cmd.push(d.to_string());
    }

    // ── User-configured bind mounts ──────────────────────────
    // Write dirs imply read access; skip them in the read-only list
    // to prevent ro-bind from shadowing writable bind mount on
    // systems where mount order behaves unexpectedly.
    let write_paths: Vec<String> = config
        .write_dirs
        .iter()
        .map(|d| resolve_dir_lexical(d, cwd))
        .collect();
    for d in &config.read_dirs {
        let resolved = resolve_dir_lexical(d, cwd);
        if write_paths.iter().any(|w| resolved.starts_with(w)) {
            continue;
        }
        cmd.push("--ro-bind".into());
        cmd.push(resolved.clone());
        cmd.push(resolved);
    }
    for d in &config.write_dirs {
        let resolved = resolve_dir_lexical(d, cwd);
        cmd.push("--bind".into());
        cmd.push(resolved.clone());
        cmd.push(resolved);
    }

    // ── HOME directory (read-only for config access) ─────────
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        cmd.push("--ro-bind".into());
        cmd.push(home.to_string());
        cmd.push(home.to_string());
    }

    // ── MINK_HOME / default ~/.mink (writable for session persistence) ──
    if let Some(mink_home) = mink_home {
        cmd.push("--bind".into());
        cmd.push(mink_home.to_string());
        cmd.push(mink_home.to_string());
    }

    // ── Namespace isolation ──────────────────────────────────
    cmd.push("--unshare-pid".into());
    cmd.push("--unshare-ipc".into());
    cmd.push("--unshare-uts".into());
    // 沙箱随 bwrap 退出：bwrap 被杀（SIGKILL/SIGTERM/父进程死亡）时，内核向命名空间内
    // PID1 投递 SIGKILL ⇒ 整个 PID 命名空间被清空。不加这一项时，父侧只杀 bwrap
    // 杀不到沙箱内的 worker（它会成为孤儿并继续跑）。
    cmd.push("--die-with-parent".into());

    if !config.allow_network {
        cmd.push("--unshare-net".into());
    }

    // ── Working directory (same logic as nsjail) ──────────────
    let work_dir = config
        .write_dirs
        .first()
        .or(config.read_dirs.first())
        .map(|d| resolve_dir_lexical(d, cwd))
        .unwrap_or_else(|| cwd.display().to_string());
    cmd.push("--chdir".into());
    cmd.push(work_dir);

    // ── Target binary ────────────────────────────────────────
    cmd.push("--".into());
    cmd.push(exe.display().to_string());
    if args.len() > 1 {
        cmd.extend(args[1..].iter().cloned());
    }

    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> SandboxConfig {
        SandboxConfig {
            enabled: true,
            backend: "bwrap".into(),
            ..Default::default()
        }
    }

    /// R10：进程树回收依赖的命名空间与父死标志必须存在（Linux 不变量）。
    #[test]
    fn bwrap_argv_pins_namespace_and_parent_death_flags() {
        let argv = bwrap_argv(
            &config(),
            Path::new("/opt/mink/bin/mink"),
            &["mink".into(), "run".into()],
            Path::new("/work"),
            None,
            None,
        );
        assert_eq!(argv[0], "bwrap");
        for flag in [
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--die-with-parent",
        ] {
            assert!(
                argv.iter().any(|arg| arg == flag),
                "missing {flag}: {argv:?}"
            );
        }
        assert!(
            !argv.iter().any(|arg| arg == "--unshare-net"),
            "network is allowed by default: {argv:?}"
        );
        let separator = argv.iter().position(|arg| arg == "--").expect("--");
        assert_eq!(argv[separator + 1], "/opt/mink/bin/mink");
        assert_eq!(&argv[separator + 2..], &["run".to_string()]);
    }

    /// R10：写目录隐含读权限（ro-bind 不得遮蔽 bind）、相对路径按 cwd 解析、
    /// 工作目录取第一个写目录、网络开关生效。
    #[test]
    fn bwrap_argv_write_dirs_imply_read_and_choose_workdir() {
        let cfg = SandboxConfig {
            enabled: true,
            backend: "bwrap".into(),
            read_dirs: vec![
                "/data/read".into(),
                "/data/write/sub".into(),
                "./relative".into(),
            ],
            write_dirs: vec!["/data/write".into()],
            allow_network: false,
            ..Default::default()
        };
        let argv = bwrap_argv(
            &cfg,
            Path::new("/bin/mink"),
            &["mink".into()],
            Path::new("/work"),
            Some("/home/u"),
            Some("/home/u/.mink"),
        );
        let has_triple = |flag: &str, path: &str| {
            argv.windows(3)
                .any(|window| window[0] == flag && window[1] == path && window[2] == path)
        };
        assert!(has_triple("--ro-bind", "/data/read"), "{argv:?}");
        assert!(
            !has_triple("--ro-bind", "/data/write/sub"),
            "a write subtree must not be ro-bound: {argv:?}"
        );
        assert!(has_triple("--bind", "/data/write"), "{argv:?}");
        assert!(has_triple("--ro-bind", "/home/u"), "{argv:?}");
        assert!(has_triple("--bind", "/home/u/.mink"), "{argv:?}");
        assert!(
            argv.iter()
                .any(|arg| arg.starts_with("/work/") && arg.ends_with("relative")),
            "relative read dirs resolve against cwd: {argv:?}"
        );
        let chdir = argv
            .iter()
            .position(|arg| arg == "--chdir")
            .expect("--chdir");
        assert_eq!(argv[chdir + 1], "/data/write");
        assert!(
            argv.iter().any(|arg| arg == "--unshare-net"),
            "allow_network=false must unshare the network: {argv:?}"
        );
    }
}
