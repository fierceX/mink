//! Linux 进程树回收集成测试（R10）。
//!
//! 真实执行 bwrap，验证 mink 所依赖的“父死 → 监工死 → PID 命名空间回收”假设：
//! bwrap 带 `--die-with-parent` 时，宿主父进程死亡后命名空间内进程必须消失。
//!
//! 仅在 Linux 上编译/运行。缺少 bwrap 时跳过；CI 把 `MINK_REQUIRE_SANDBOX=1`
//! 置为要求真实执行（跳过即失败），发布门槛至少覆盖一项真实执行。
#![cfg(target_os = "linux")]

use std::process::Command;
use std::time::{Duration, Instant};

/// 唯一标记：保证只匹配本测试启动的进程，不误杀/误判其它进程。
const SENTINEL: &str = "mink-sandbox-tree-sentinel";

fn bwrap_available() -> bool {
    Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// 探测当前内核/运行器是否允许 bwrap 建立 PID 命名空间（受限用户命名空间会失败）。
fn bwrap_namespace_probe() -> bool {
    Command::new("bwrap")
        .args([
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--unshare-pid",
            "--die-with-parent",
            "--",
            "/bin/true",
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn sentinel_alive_excluding(exclude_pid: Option<u32>) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if Some(pid) == exclude_pid {
            continue;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if String::from_utf8_lossy(&cmdline).contains(SENTINEL) {
            return true;
        }
    }
    false
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

fn require_sandbox() -> bool {
    std::env::var("MINK_REQUIRE_SANDBOX").is_ok()
}

#[test]
fn bwrap_die_with_parent_reaps_the_namespace_on_parent_death() {
    if !bwrap_available() {
        if require_sandbox() {
            panic!("bwrap is required by MINK_REQUIRE_SANDBOX but was not found in PATH");
        }
        eprintln!("skip: bwrap not installed");
        return;
    }
    if !bwrap_namespace_probe() {
        if require_sandbox() {
            panic!("bwrap cannot create a PID namespace in this environment");
        }
        eprintln!("skip: bwrap namespace probe failed (restricted unprivileged userns?)");
        return;
    }
    assert!(
        !sentinel_alive_excluding(None),
        "sentinel leaked from an earlier run; refusing a false positive"
    );

    // 父进程是 /bin/sh：它后台启动 bwrap（`--die-with-parent`），自身保持存活等待。
    // 父被 SIGKILL 后，bwrap 收到父死信号并回收自己的 PID 命名空间。
    // 注意：父进程自己的 cmdline 就包含整段脚本（含标记），扫描时必须排除它。
    let script = format!(
        "bwrap --dev /dev --proc /proc --tmpfs /tmp \
         --ro-bind /usr /usr --ro-bind /lib /lib --ro-bind /lib64 /lib64 \
         --ro-bind /bin /bin --ro-bind /sbin /sbin \
         --unshare-pid --die-with-parent -- \
         /bin/sh -c 'sleep 31337; true' #{SENTINEL} & wait"
    );
    let mut parent = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .spawn()
        .expect("spawn parent shell");
    let parent_pid = parent.id();
    assert!(
        wait_until(Duration::from_secs(10), || {
            sentinel_alive_excluding(Some(parent_pid))
        }),
        "the sandboxed process did not start"
    );

    parent.kill().expect("kill the parent shell");
    let _ = parent.wait();
    assert!(
        wait_until(Duration::from_secs(10), || {
            !sentinel_alive_excluding(Some(parent_pid))
        }),
        "the sandboxed process tree survived parent death"
    );
}

/// bwrap 直接被杀（SIGKILL 监工本身）也必须回收命名空间内的进程。
#[test]
fn killing_bwrap_itself_reaps_the_namespace() {
    if !bwrap_available() || !bwrap_namespace_probe() {
        if require_sandbox() {
            panic!("bwrap with PID namespace support is required by MINK_REQUIRE_SANDBOX");
        }
        eprintln!("skip: bwrap unavailable or namespace probe failed");
        return;
    }
    assert!(
        !sentinel_alive_excluding(None),
        "sentinel leaked from an earlier run"
    );

    let mut bwrap = Command::new("bwrap")
        .args([
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
            "--ro-bind",
            "/bin",
            "/bin",
            "--unshare-pid",
            "--die-with-parent",
            "--",
            "/bin/sh",
            "-c",
            "sleep 31337; true",
        ])
        .arg(format!("#{SENTINEL}"))
        .spawn()
        .expect("spawn bwrap");
    assert!(
        wait_until(Duration::from_secs(10), || sentinel_alive_excluding(None)),
        "the sandboxed process did not start"
    );
    bwrap.kill().expect("kill bwrap");
    let _ = bwrap.wait();
    assert!(
        wait_until(Duration::from_secs(10), || !sentinel_alive_excluding(None)),
        "the sandboxed process survived bwrap death"
    );
}
