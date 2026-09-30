//! Linux 进程树回收集成测试（R10 / F01 / F02）。
//!
//! 真实执行 bwrap，验证 mink 所依赖的「父死 → 监工死 → PID 命名空间回收」假设：
//! bwrap 带 `--die-with-parent` 时，宿主父进程死亡后命名空间内进程必须消失；
//! 撤掉该标志的负例必须仍存活（证明正例不是空断言）。
//!
//! 仅在 Linux 上编译/运行。缺少 bwrap 或内核不允许 PID 命名空间时跳过；
//! CI 把 `MINK_REQUIRE_SANDBOX=1` 置为要求真实执行（跳过即失败）。
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 沙箱内必须挂载的最小根文件系统（只保留宿主上真实存在的路径）。
fn minimal_mount_args() -> Vec<String> {
    let mut args: Vec<String> = vec!["--dev".into(), "/dev".into()];
    if Path::new("/proc").exists() {
        args.extend(["--proc".to_string(), "/proc".to_string()]);
    }
    args.extend(["--tmpfs".to_string(), "/tmp".to_string()]);
    for path in ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"] {
        if Path::new(path).exists() {
            args.extend(["--ro-bind".to_string(), path.to_string(), path.to_string()]);
        }
    }
    args
}

/// 构造带最小文件系统 + PID 命名空间的 bwrap 命令（不含 `--` 之后的负载）。
fn bwrap_command() -> Command {
    let mut cmd = Command::new("bwrap");
    cmd.args(minimal_mount_args());
    cmd.arg("--unshare-pid");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd
}

fn bwrap_available() -> bool {
    Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// F01：探测必须挂载可执行文件与库；失败时保留 stderr/退出状态用于区分
/// 「环境不允许命名空间」与「脚本构造错误」。
fn probe_bwrap() -> Result<(), String> {
    if !bwrap_available() {
        return Err("bwrap binary not found in PATH".to_string());
    }
    let output = bwrap_command()
        .stderr(Stdio::piped())
        .arg("--die-with-parent")
        .arg("--")
        .arg("/bin/true")
        .output()
        .map_err(|error| format!("failed to run bwrap probe: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "bwrap probe failed: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn require_sandbox() -> bool {
    std::env::var("MINK_REQUIRE_SANDBOX").is_ok()
}

/// 返回 `None` 表示前置条件满足；`Some(reason)` 表示环境不可用。
fn sandbox_precondition() -> Option<String> {
    probe_bwrap().err()
}

fn skip_or_panic(reason: &str) {
    if require_sandbox() {
        panic!("MINK_REQUIRE_SANDBOX is set but the sandbox is unusable: {reason}");
    }
    eprintln!("skip: {reason}");
}

fn proc_cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

/// 找到 cmdline 含 `token` 的进程；`first_arg` 用于区分 bwrap 监工与沙箱内 shell。
fn pids_matching(token: &str, first_arg: Option<&str>, exclude: &[u32]) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if exclude.contains(&pid) {
            continue;
        }
        let Some(args) = proc_cmdline(pid) else {
            continue;
        };
        if !args.iter().any(|arg| arg.contains(token)) {
            continue;
        }
        if let Some(first) = first_arg
            && args.first().map(String::as_str) != Some(first)
        {
            continue;
        }
        found.push(pid);
    }
    found
}

fn proc_children(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .ok()
        .map(|raw| {
            raw.split_whitespace()
                .filter_map(|value| value.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn proc_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn wait_until(deadline: Duration, mut check: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check()
}

/// RAII 清理：测试失败/成功都必须 kill+wait，不遗留长跑子进程。
struct ChildGuard {
    label: &'static str,
    child: Option<Child>,
}

impl ChildGuard {
    fn spawn(label: &'static str, mut command: Command) -> (Self, Child) {
        let child = command
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {label} failed: {error}"));
        (Self { label, child: None }, child)
    }

    fn arm(&mut self, child: Child) {
        self.child = Some(child);
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!("cleaned up {}", self.label);
        }
    }
}

/// shell 脚本里安全引用 token 形式：作为 inner shell 的 `$0` 参数传入，
/// 避免裸 `#` 注释与参数展开歧义（F02）。
fn inner_command(token: &str) -> String {
    format!("/bin/sh -c 'sleep 31337; true' {token}")
}

fn wait_for_inner(token: &str, parents: &[u32], timeout: Duration) -> Option<(u32, Vec<u32>)> {
    let mut found = None;
    wait_until(timeout, || {
        let pids = pids_matching(token, Some("/bin/sh"), parents);
        if let Some(pid) = pids.first().copied() {
            found = Some((pid, proc_children(pid)));
            true
        } else {
            false
        }
    });
    found
}

/// 断言 token 关联的整个进程树已经消失（沙箱内 shell、其子进程、bwrap 监工）。
fn wait_for_tree_gone(token: &str, inner_pid: u32, children: &[u32], timeout: Duration) -> bool {
    wait_until(timeout, || {
        pids_matching(token, Some("/bin/sh"), &[]).is_empty()
            && pids_matching(token, Some("bwrap"), &[]).is_empty()
            && !proc_alive(inner_pid)
            && children.iter().all(|pid| !proc_alive(*pid))
    })
}

#[test]
fn bwrap_die_with_parent_reaps_the_namespace_on_parent_death() {
    let token = "mink-sandbox-parent-death-token";
    if let Some(reason) = sandbox_precondition() {
        skip_or_panic(&reason);
        return;
    }
    assert!(
        pids_matching(token, None, &[]).is_empty(),
        "token leaked from an earlier run"
    );

    // 父进程是 /bin/sh：后台启动 bwrap（--die-with-parent），自身保持存活等待。
    let mounts = minimal_mount_args().join(" ");
    let script = format!(
        "bwrap {mounts} --unshare-pid --die-with-parent -- {} & wait",
        inner_command(token)
    );
    let mut parent_command = Command::new("/bin/sh");
    parent_command.arg("-c").arg(script);
    let (mut guard, mut parent) = ChildGuard::spawn("parent shell", parent_command);
    let parent_pid = parent.id();

    let (inner_pid, children) = wait_for_inner(token, &[parent_pid], Duration::from_secs(10))
        .unwrap_or_else(|| {
            let _ = parent.kill();
            let _ = parent.wait();
            panic!("the sandboxed process did not become ready")
        });

    // 父死：bwrap 应随父死亡回收 PID 命名空间。
    let _ = parent.kill();
    let _ = parent.wait();
    guard.arm(parent);
    assert!(
        wait_for_tree_gone(token, inner_pid, &children, Duration::from_secs(10)),
        "the sandboxed process tree survived parent death"
    );
}

/// 负例：撤掉 `--die-with-parent` 时，父死不得直接回收命名空间（证明正例非空断言）。
/// 断言后主动清理：杀掉 bwrap 监工并等待整棵树消失。
#[test]
fn without_die_with_parent_the_tree_survives_parent_death() {
    let token = "mink-sandbox-no-flag-token";
    if let Some(reason) = sandbox_precondition() {
        skip_or_panic(&reason);
        return;
    }
    assert!(
        pids_matching(token, None, &[]).is_empty(),
        "token leaked from an earlier run"
    );

    let mounts = minimal_mount_args().join(" ");
    let script = format!(
        "bwrap {mounts} --unshare-pid -- {} & wait",
        inner_command(token)
    );
    let mut parent_command = Command::new("/bin/sh");
    parent_command.arg("-c").arg(script);
    let (mut guard, mut parent) =
        ChildGuard::spawn("parent shell (negative control)", parent_command);
    let parent_pid = parent.id();

    let (inner_pid, children) = wait_for_inner(token, &[parent_pid], Duration::from_secs(10))
        .unwrap_or_else(|| {
            let _ = parent.kill();
            let _ = parent.wait();
            panic!("the sandboxed process did not become ready")
        });

    let _ = parent.kill();
    let _ = parent.wait();
    guard.arm(parent);
    // 给内核一个宽限窗口：没有 die-with-parent 时树应仍然存活。
    std::thread::sleep(Duration::from_millis(1_000));
    assert!(
        proc_alive(inner_pid) && children.iter().all(|pid| proc_alive(*pid)),
        "without --die-with-parent the namespace must survive parent death"
    );

    // 清理：杀掉 bwrap 监工（其 PID 命名空间随 PID1 一起回收）。
    let bwrap_pids = pids_matching(token, Some("bwrap"), &[]);
    assert!(
        !bwrap_pids.is_empty(),
        "the orphaned bwrap launcher should still be discoverable"
    );
    for pid in &bwrap_pids {
        let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
    }
    assert!(
        wait_for_tree_gone(token, inner_pid, &children, Duration::from_secs(10)),
        "killing the launcher must still reap the namespace"
    );
}

/// 直接杀 bwrap 监工本身也必须回收命名空间内的进程（不使用 `--die-with-parent`
/// 之外的手段，只依赖 PID 命名空间 PID1 死亡）。
#[test]
fn killing_bwrap_itself_reaps_the_namespace() {
    let token = "mink-sandbox-kill-bwrap-token";
    if let Some(reason) = sandbox_precondition() {
        skip_or_panic(&reason);
        return;
    }
    assert!(
        pids_matching(token, None, &[]).is_empty(),
        "token leaked from an earlier run"
    );

    let mut command = bwrap_command();
    command
        .arg("--die-with-parent")
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg("sleep 31337; true")
        .arg(token);
    let (mut guard, mut bwrap) = ChildGuard::spawn("bwrap launcher", command);
    let bwrap_pid = bwrap.id();

    let (inner_pid, children) = wait_for_inner(token, &[bwrap_pid], Duration::from_secs(10))
        .unwrap_or_else(|| {
            let _ = bwrap.kill();
            let _ = bwrap.wait();
            panic!("the sandboxed process did not become ready")
        });

    let _ = bwrap.kill();
    let _ = bwrap.wait();
    guard.arm(bwrap);
    assert!(
        wait_for_tree_gone(token, inner_pid, &children, Duration::from_secs(10)),
        "the sandboxed process survived bwrap death"
    );
}
