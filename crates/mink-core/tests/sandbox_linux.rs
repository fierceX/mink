//! Linux 进程树回收集成测试（R10 / F01 / F02 / G02）。
//!
//! 真实执行 bwrap，验证 mink 所依赖的「父死 → 监工死 → PID 命名空间回收」假设，
//! 并用“撤掉 `--die-with-parent` 树仍存活”的负例证明正例不是空断言。
//!
//! 仅在 Linux 上编译/运行。缺少 bwrap 或内核不允许 PID 命名空间时跳过；
//! CI 把 `MINK_REQUIRE_SANDBOX=1` 置为要求真实执行（跳过即失败）。
#![cfg(target_os = "linux")]

use std::panic::{AssertUnwindSafe, catch_unwind, panic_any};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 清理与就绪的宽限窗口：任何一步都不允许无限等待。
const WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// 每次运行唯一的 token：并发执行的两份测试不会互相匹配或互相清理。
fn unique_token(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("mink-sbx-{name}-{}-{nanos}", std::process::id())
}

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

/// F01：探测挂载可执行文件与库；失败时保留 stderr/退出状态，区分
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

fn skip_or_panic(reason: &str) {
    if require_sandbox() {
        panic!("MINK_REQUIRE_SANDBOX is set but the sandbox is unusable: {reason}");
    }
    eprintln!("skip: {reason}");
}

// ── /proc 读取：身份、命令行与子进程 ────────────────────────────────

fn proc_cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

/// `/proc/<pid>/stat` 第 22 个字段（starttime），用于识别 PID 重用。
fn proc_start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    fields.get(19).map(|value| (*value).to_string())
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

fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check()
}

// ── 进程身份与清理 ────────────────────────────────────────────────

/// 记录 PID + starttime：清理时先复核身份，避免误杀 PID 重用后的无关进程。
#[derive(Clone, Debug)]
struct ProcessRef {
    pid: u32,
    start_time: String,
}

impl ProcessRef {
    fn capture(pid: u32) -> Option<Self> {
        proc_start_time(pid).map(|start_time| Self { pid, start_time })
    }

    fn is_same_process(&self) -> bool {
        proc_start_time(self.pid).as_deref() == Some(self.start_time.as_str())
    }

    fn alive(&self) -> bool {
        self.is_same_process()
    }

    fn kill(&self, label: &str) {
        if !self.is_same_process() {
            return;
        }
        let _ = Command::new("kill")
            .arg("-9")
            .arg(self.pid.to_string())
            .status();
        eprintln!("[cleanup:{label}] killed pid={}", self.pid);
    }
}

/// 记录并保证回收测试期间发现的进程；Drop 始终生效（含 panic 路径）。
struct TreeCleanup {
    label: &'static str,
    pids: Vec<ProcessRef>,
}

impl TreeCleanup {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            pids: Vec::new(),
        }
    }

    fn track(&mut self, pid: u32) {
        if let Some(process) = ProcessRef::capture(pid)
            && !self.pids.iter().any(|known| known.pid == process.pid)
        {
            self.pids.push(process);
        }
    }

    /// 记录当前与 token 匹配的进程（按 first_arg 分类）；返回新记录的 pid。
    fn track_matching(
        &mut self,
        token: &str,
        first_arg: Option<&str>,
        exclude: &[u32],
    ) -> Vec<u32> {
        let pids = pids_matching(token, first_arg, exclude);
        for pid in &pids {
            self.track(*pid);
        }
        pids
    }

    fn all_gone(&self) -> bool {
        self.pids.iter().all(|process| !process.alive())
    }

    /// 已登记的进程快照（用于在捕获区外验证 teardown）。
    fn pids(&self) -> &[ProcessRef] {
        &self.pids
    }

    fn wait_gone(&self, timeout: Duration) -> bool {
        wait_until(timeout, || self.all_gone())
    }

    fn kill_all(&self) {
        for process in &self.pids {
            process.kill(self.label);
        }
    }
}

impl Drop for TreeCleanup {
    fn drop(&mut self) {
        if self.all_gone() {
            return;
        }
        eprintln!("[cleanup:{}] reaping leaked processes", self.label);
        self.kill_all();
        if !self.wait_gone(WAIT_TIMEOUT) {
            eprintln!(
                "[cleanup:{}] warning: some tracked processes are still alive after {WAIT_TIMEOUT:?}",
                self.label
            );
        }
    }
}

/// 始终持有 Child 的 guard：正常路径可显式 kill/wait，Drop 兜底且对已回收子进程安全。
struct ChildGuard {
    label: &'static str,
    child: Child,
}

impl ChildGuard {
    fn spawn(label: &'static str, mut command: Command) -> Self {
        let child = command
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {label} failed: {error}"));
        Self { label, child }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        eprintln!("[cleanup:{}] child reaped", self.label);
    }
}

// ── 就绪握手 ─────────────────────────────────────────────────────

/// inner shell 以 `$0` 参数携带 token（避免裸 `#` 注释与参数展开歧义）。
fn inner_command(token: &str) -> String {
    format!("/bin/sh -c 'sleep 31337; true' {token}")
}

/// 保持运行、但故意不启动名为 `sleep` 的子进程（用于就绪超时路径）。
fn inner_command_without_sleep_child(token: &str) -> String {
    format!("/bin/sh -c 'tail -f /dev/null; true' {token}")
}

fn process_named(pid: u32, name: &str) -> bool {
    proc_cmdline(pid)
        .and_then(|args| args.first().cloned())
        .is_some_and(|first| first == name)
}

fn has_sleep_child(pid: u32) -> bool {
    proc_children(pid)
        .iter()
        .any(|child| process_named(*child, "sleep"))
}

/// 发现并**立即登记**当前可见的整棵树（bwrap 监工、inner shell、现有子进程），
/// 再返回 inner shell 列表。登记与就绪判定分离，失败/超时路径也能拿到清理句柄。
fn discover_tree(token: &str, parents: &[u32], cleanup: &mut TreeCleanup) -> Vec<u32> {
    cleanup.track_matching(token, Some("bwrap"), parents);
    let inners = cleanup.track_matching(token, Some("/bin/sh"), parents);
    for inner in &inners {
        for child in proc_children(*inner) {
            cleanup.track(child);
        }
    }
    inners
}

/// 等待沙箱内 shell **及其至少一个 `sleep` 子进程**就绪。每轮先登记已发现的
/// 进程，再判断就绪；超时出口仍会做最后一次发现/登记，确保失败清理覆盖已启动的树。
fn wait_for_tree_ready(
    token: &str,
    parents: &[u32],
    cleanup: &mut TreeCleanup,
    timeout: Duration,
) -> Option<(u32, Vec<u32>)> {
    let deadline = Instant::now() + timeout;
    loop {
        let inners = discover_tree(token, parents, cleanup);
        if let Some(inner) = inners.iter().find(|pid| has_sleep_child(**pid)) {
            return Some((*inner, proc_children(*inner)));
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // 超时出口：最后一次发现/登记，保证 TreeCleanup 能独立回收已启动的树。
    let _ = discover_tree(token, parents, cleanup);
    None
}

/// 断言 token 关联的整个进程树已经消失（沙箱内 shell、其子进程、bwrap 监工）。
fn wait_for_tree_gone(token: &str, timeout: Duration) -> bool {
    wait_until(timeout, || pids_matching(token, None, &[]).is_empty())
}

// ── 测试 ────────────────────────────────────────────────────────

#[test]
fn bwrap_die_with_parent_reaps_the_namespace_on_parent_death() {
    if let Err(reason) = probe_bwrap() {
        skip_or_panic(&reason);
        return;
    }
    let token = unique_token("parent-death");
    let mut cleanup = TreeCleanup::new("parent-death tree");
    let mounts = minimal_mount_args().join(" ");
    let script = format!(
        "bwrap {mounts} --unshare-pid --die-with-parent -- {} & wait",
        inner_command(&token)
    );
    let mut parent_command = Command::new("/bin/sh");
    parent_command.arg("-c").arg(script);
    let mut parent = ChildGuard::spawn("parent shell", parent_command);
    let parent_pid = parent.pid();
    cleanup.track(parent_pid);

    let (inner_pid, children) =
        wait_for_tree_ready(&token, &[parent_pid], &mut cleanup, WAIT_TIMEOUT)
            .unwrap_or_else(|| panic!("the sandboxed process did not become ready"));
    assert!(inner_pid > 0 && !children.is_empty());

    // 父死：bwrap 应随父死亡回收 PID 命名空间（TreeCleanup 兜底任何残留）。
    parent.kill_and_wait();
    assert!(
        cleanup.wait_gone(WAIT_TIMEOUT),
        "the sandboxed process tree survived parent death"
    );
    assert!(
        wait_for_tree_gone(&token, WAIT_TIMEOUT),
        "token-matched processes leaked after parent death"
    );
}

/// 负例：撤掉 `--die-with-parent` 时，父死不得直接回收命名空间（证明正例非空断言）；
/// 断言后由 cleanup/显式清理收回整棵树。
#[test]
fn without_die_with_parent_the_tree_survives_parent_death() {
    if let Err(reason) = probe_bwrap() {
        skip_or_panic(&reason);
        return;
    }
    let token = unique_token("no-flag");
    let mut cleanup = TreeCleanup::new("no-flag tree");
    let mounts = minimal_mount_args().join(" ");
    let script = format!(
        "bwrap {mounts} --unshare-pid -- {} & wait",
        inner_command(&token)
    );
    let mut parent_command = Command::new("/bin/sh");
    parent_command.arg("-c").arg(script);
    let mut parent = ChildGuard::spawn("parent shell (negative control)", parent_command);
    let parent_pid = parent.pid();
    cleanup.track(parent_pid);

    let (inner_pid, children) =
        wait_for_tree_ready(&token, &[parent_pid], &mut cleanup, WAIT_TIMEOUT)
            .unwrap_or_else(|| panic!("the sandboxed process did not become ready"));

    parent.kill_and_wait();
    // 给内核一个宽限窗口：没有 die-with-parent 时树应仍然存活。
    std::thread::sleep(Duration::from_millis(1_000));
    assert!(
        cleanup
            .pids
            .iter()
            .filter(|process| process.pid == inner_pid || children.contains(&process.pid))
            .all(|process| process.alive()),
        "without --die-with-parent the namespace must survive parent death"
    );

    // 显式清理：杀掉 bwrap 监工（命名空间 PID1 死亡后整棵树回收）。
    let bwrap_pids = cleanup.track_matching(&token, Some("bwrap"), &[parent_pid]);
    assert!(
        !bwrap_pids.is_empty(),
        "the orphaned bwrap launcher should still be discoverable"
    );
    for pid in &bwrap_pids {
        if let Some(process) = ProcessRef::capture(*pid) {
            process.kill("no-flag launcher");
        }
    }
    assert!(
        cleanup.wait_gone(WAIT_TIMEOUT) && wait_for_tree_gone(&token, WAIT_TIMEOUT),
        "killing the launcher must still reap the namespace"
    );
}

/// 直接杀 bwrap 监工本身也必须回收命名空间内的进程。
#[test]
fn killing_bwrap_itself_reaps_the_namespace() {
    if let Err(reason) = probe_bwrap() {
        skip_or_panic(&reason);
        return;
    }
    let token = unique_token("kill-bwrap");
    let mut cleanup = TreeCleanup::new("kill-bwrap tree");
    let mut command = bwrap_command();
    command
        .arg("--die-with-parent")
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg("sleep 31337; true")
        .arg(&token);
    let mut bwrap = ChildGuard::spawn("bwrap launcher", command);
    let bwrap_pid = bwrap.pid();
    cleanup.track(bwrap_pid);

    let (_inner_pid, _children) =
        wait_for_tree_ready(&token, &[bwrap_pid], &mut cleanup, WAIT_TIMEOUT)
            .unwrap_or_else(|| panic!("the sandboxed process did not become ready"));

    bwrap.kill_and_wait();
    assert!(
        cleanup.wait_gone(WAIT_TIMEOUT) && wait_for_tree_gone(&token, WAIT_TIMEOUT),
        "the sandboxed process survived bwrap death"
    );
}

/// 就绪超时（工作负载故意不启动 `sleep` 子进程）也必须能独立回收已启动的树：
/// 登记与就绪判定分离，失败出口不依赖父死联动。
#[test]
fn readiness_timeout_still_registers_and_reaps_the_started_tree() {
    if let Err(reason) = probe_bwrap() {
        skip_or_panic(&reason);
        return;
    }
    let token = unique_token("readiness-timeout");
    let mut cleanup = TreeCleanup::new("readiness-timeout tree");
    let mounts = minimal_mount_args().join(" ");
    let script = format!(
        "bwrap {mounts} --unshare-pid -- {} & wait",
        inner_command_without_sleep_child(&token)
    );
    let mut parent_command = Command::new("/bin/sh");
    parent_command.arg("-c").arg(script);
    let parent = ChildGuard::spawn("readiness-timeout parent", parent_command);
    cleanup.track(parent.pid());

    let ready = wait_for_tree_ready(
        &token,
        &[parent.pid()],
        &mut cleanup,
        Duration::from_secs(2),
    );
    assert!(
        ready.is_none(),
        "a workload without a sleep child is not ready"
    );
    // 超时出口已登记 bwrap/inner/子进程：清理不能只依靠父 shell。
    assert!(
        cleanup.pids().len() >= 2,
        "timeout exit must register the started tree: {:?}",
        cleanup.pids()
    );
    cleanup.kill_all();
    assert!(
        cleanup.wait_gone(WAIT_TIMEOUT),
        "recorded processes leaked after readiness timeout"
    );
    assert!(
        wait_for_tree_gone(&token, WAIT_TIMEOUT),
        "token processes leaked after readiness timeout"
    );
}

// ── G02：失败注入（panic 路径）────────────────────────────────────

/// 专用 panic payload：外部据此区分「到达注入点」与「准备阶段就失败」。
struct InjectedPanic;

/// 在 catch_unwind 内启动沙箱树、确认就绪、杀掉父 shell，然后 panic。返回捕获区外
/// 保留的进程身份，供 teardown 后逐个验证（含不带 token 的 `sleep` 子进程）。
fn run_panic_cleanup_probe(token: &str, die_with_parent: bool) -> Vec<ProcessRef> {
    let mounts = minimal_mount_args().join(" ");
    let flag = if die_with_parent {
        "--die-with-parent "
    } else {
        ""
    };
    let script = format!(
        "bwrap {mounts} --unshare-pid {flag}-- {} & wait",
        inner_command(token)
    );
    let mut parent_command = Command::new("/bin/sh");
    parent_command.arg("-c").arg(script);
    let mut tracked: Vec<ProcessRef> = Vec::new();

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut cleanup = TreeCleanup::new("panic-probe tree");
        let mut parent = ChildGuard::spawn("panic-probe parent", parent_command);
        cleanup.track(parent.pid());
        let ready = wait_for_tree_ready(token, &[parent.pid()], &mut cleanup, WAIT_TIMEOUT)
            .expect("the sandboxed process did not become ready");
        let (inner_pid, children) = ready;
        assert!(children.iter().any(|pid| process_named(*pid, "sleep")));
        tracked.extend(cleanup.pids().iter().cloned());
        assert!(
            tracked.iter().any(|process| process.pid == inner_pid),
            "the inner shell must be registered before the injected panic"
        );
        // 父死（或不撤父死联动）之后、正常清理之前触发注入点。
        parent.kill_and_wait();
        panic_any(InjectedPanic);
    }));

    match outcome {
        Ok(()) => panic!("the probe must panic at the injection point"),
        Err(payload) => assert!(
            payload.downcast_ref::<InjectedPanic>().is_some(),
            "preparation failed before the injection point; the test must not pass"
        ),
    }
    tracked
}

#[test]
fn child_guard_reaps_sandbox_tree_on_panic_path_with_die_with_parent() {
    if let Err(reason) = probe_bwrap() {
        skip_or_panic(&reason);
        return;
    }
    let token = unique_token("panic-dwp");
    let tracked = run_panic_cleanup_probe(&token, true);
    assert!(!tracked.is_empty());
    assert!(
        wait_until(WAIT_TIMEOUT, || tracked
            .iter()
            .all(|process| !process.alive())),
        "panic path leaked recorded processes: {tracked:?}"
    );
    assert!(
        wait_for_tree_gone(&token, WAIT_TIMEOUT),
        "panic path leaked token processes"
    );
}

/// 无父死联动时，父 shell 退出不影响沙箱树：回收只能依靠 TreeCleanup（独立树清理）。
#[test]
fn child_guard_reaps_sandbox_tree_on_panic_path_without_die_with_parent() {
    if let Err(reason) = probe_bwrap() {
        skip_or_panic(&reason);
        return;
    }
    let token = unique_token("panic-no-dwp");
    let tracked = run_panic_cleanup_probe(&token, false);
    assert!(!tracked.is_empty());
    assert!(
        wait_until(WAIT_TIMEOUT, || tracked
            .iter()
            .all(|process| !process.alive())),
        "panic path (no die-with-parent) leaked recorded processes: {tracked:?}"
    );
    assert!(
        wait_for_tree_gone(&token, WAIT_TIMEOUT),
        "panic path (no die-with-parent) leaked token processes"
    );
}
