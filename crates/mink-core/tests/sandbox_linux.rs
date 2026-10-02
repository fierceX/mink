//! Linux 进程树回收集成测试。
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

/// 探测挂载可执行文件与库；失败时保留 stderr/退出状态，区分
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
        panic!(
            "MINK_REQUIRE_SANDBOX is set but the sandbox is unusable: {reason}\n\
             hint: Ubuntu24.04+ restricts unprivileged user namespaces via AppArmor \
             (`kernel.apparmor_restrict_unprivileged_userns`); allow them or run on a \
             runner without the restriction"
        );
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

fn proc_environ(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        raw.split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

/// 按「argv[0] == 测试二进制 + environ 携带本次唯一 token」定位宿主可见的进程。
/// 环境变量是唯一区分并发测试实例的可靠标记（沙箱内 PID 在宿主不可用，
/// NSpid 在沙箱内挂载的 procfs 里只列出最内层）。
fn pids_with_token_and_argv0(argv0: &str, token: &str) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let needle = format!("MINK_TEST_SANDBOX_TOKEN={token}");
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(args) = proc_cmdline(pid) else {
            continue;
        };
        if args.first().map(String::as_str) != Some(argv0) {
            continue;
        }
        if proc_environ(pid).is_some_and(|env| env.iter().any(|entry| entry == &needle)) {
            found.push(pid);
        }
    }
    found
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

fn proc_alive(pid: u32) -> bool {
    proc_start_time(pid).is_some()
}

/// 列出 ppid 等于 `parent` 的进程。宿主视角下 `children` 文件只列同一 PID 命名空间
/// 的子进程（沙箱内的孙进程不出现），必须改用 `/proc/<pid>/stat` 的 PPid 扫描。
fn pids_with_ppid(parent: u32) -> Vec<u32> {
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
        if proc_ppid(pid) == Some(parent) {
            found.push(pid);
        }
    }
    found
}

/// `/proc/<pid>/status` 中某个字段的值（如 `NSpid`）。
fn proc_status_field(pid: u32, field: &str) -> Option<String> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(value) = line
            .strip_prefix(field)
            .and_then(|rest| rest.strip_prefix(':'))
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// 父进程 PID（`/proc/<pid>/stat` 第 4 个字段）。
fn proc_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    fields.get(1).and_then(|value| value.parse::<u32>().ok())
}

/// 解析探针报告行：`MINK_PROBE_INSIDE ns_pid=<n> ns_child=<n> token=<t>`。
/// 只用于**就绪握手**；宿主可见 PID 由 [`pids_with_token_and_argv0`] 定位。
fn parse_probe_marker(line: &str) -> Option<(u32, u32)> {
    let mut pid = None;
    let mut child = None;
    for part in line.split_whitespace() {
        if let Some(value) = part.strip_prefix("ns_pid=") {
            pid = value.parse::<u32>().ok();
        }
        if let Some(value) = part.strip_prefix("ns_child=") {
            child = value.parse::<u32>().ok();
        }
    }
    Some((pid?, child?))
}

// ── 进程身份与清理 ────────────────────────────────────────────────

/// `/proc/<pid>/stat` 的进程状态字符（第三个字段；comm 可能含空格/括号）。
fn proc_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    stat[close + 1..].split_whitespace().next()?.chars().next()
}

/// 记录 PID + starttime：清理时先复核身份，避免误杀 PID 重用后的无关进程。
#[derive(Clone, Debug)]
struct ProcessRef {
    pid: u32,
    start_time: String,
}

impl ProcessRef {
    fn capture(pid: u32) -> Option<Self> {
        // PID 1/2 是 init/kthreadd：命名空间内 PID 在宿主不可用时可能误指向它们，
        // 绝不登记、绝不下杀手。
        if pid <= 2 {
            eprintln!("[cleanup] refusing to track kernel pid {pid}");
            return None;
        }
        proc_start_time(pid).map(|start_time| Self { pid, start_time })
    }

    fn is_same_process(&self) -> bool {
        proc_start_time(self.pid).as_deref() == Some(self.start_time.as_str())
    }

    /// 僵尸（Z）已停止运行，只是等待父进程回收：不计为“仍存活”，否则会把
    /// 未 reaped 的子进程误报为泄漏（测试进程自己就是这些子进程的父进程）。
    fn alive(&self) -> bool {
        self.is_same_process() && proc_state(self.pid) != Some('Z')
    }

    fn kill(&self, label: &str) {
        if !self.is_same_process() {
            return;
        }
        let killed = Command::new("kill")
            .arg("-9")
            .arg(self.pid.to_string())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if killed {
            eprintln!("[cleanup:{label}] signalled pid={}", self.pid);
        }
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
    let mut parent = ChildGuard::spawn("readiness-timeout parent", parent_command);
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
    // 父 shell 是本测试进程的子进程：先 kill+wait 回收，避免僵尸干扰断言。
    parent.kill_and_wait();
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

// ── 失败注入（panic 路径）────────────────────────────────────

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

// ── 生产入口（runtime::reexec_in_sandbox）端到端 ────────────────────

/// 生产入口探针：仅当 driver 设置 `MINK_TEST_SANDBOX_TOKEN` + `MINK_TEST_SANDBOX_MODE`
/// 时生效。宿主侧调用 `mink::runtime::reexec_in_sandbox`（成功即不返回）；沙箱内
/// （`MINK_SANDBOXED=1`）报告自身与孙进程 PID 后保持运行，等待宿主侧父死回收。
#[test]
fn production_sandbox_probe() {
    let Ok(token) = std::env::var("MINK_TEST_SANDBOX_TOKEN") else {
        return; // 普通测试运行：不触发
    };
    if std::env::var("MINK_TEST_SANDBOX_MODE").is_err() {
        return;
    }
    if std::env::var("MINK_SANDBOXED").is_ok() {
        // 用 ChildGuard 持有孙进程：Drop 会 kill+wait，避免僵尸（也满足 clippy 的
        // spawn-without-wait 检查）。
        let mut grandchild_command = Command::new("sleep");
        grandchild_command.arg("31337");
        let grandchild = ChildGuard::spawn("sandbox grandchild sleep", grandchild_command);
        // 沙箱内 `process::id()` 是命名空间内 PID，宿主不可直接使用；宿主侧靠
        // argv[0] + 本次唯一 token 的 environ 定位本进程。
        println!(
            "MINK_PROBE_INSIDE ns_pid={} ns_child={} token={token}",
            std::process::id(),
            grandchild.pid(),
        );
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_secs(300));
        return;
    }
    let exe = std::env::current_exe().expect("current_exe");
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string();
    let config = mink::runtime::SandboxConfig {
        enabled: true,
        backend: "bwrap".into(),
        read_dirs: vec![cwd.clone()],
        write_dirs: vec![cwd],
        allow_network: false,
        ..Default::default()
    };
    let args: Vec<String> = std::env::args().collect();
    mink::runtime::reexec_in_sandbox(&config, &exe, &args);
    unreachable!("reexec_in_sandbox must not return");
}

/// 生产入口端到端：宿主父死 → `reexec_in_sandbox` 注册的 PDEATHSIG 杀掉沙箱监工 →
/// bwrap `--die-with-parent` + PID 命名空间回收整棵树（含不带 token 的孙进程）。
#[test]
fn production_reexec_reaps_namespace_on_host_parent_death() {
    if let Err(reason) = probe_bwrap() {
        skip_or_panic(&reason);
        return;
    }
    let exe = std::env::current_exe().expect("current_exe");
    let exe_str = exe.display().to_string();
    let token = unique_token("production-reexec");
    let mut cleanup = TreeCleanup::new("production-reexec tree");

    // 宿主父 shell：后台启动本测试二进制（走生产 reexec），自身等待成为被杀的父。
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("\"$1\" --exact production_sandbox_probe --nocapture & wait")
        .arg("sh")
        .arg(&exe_str)
        .env("MINK_TEST_SANDBOX_TOKEN", &token)
        .env("MINK_TEST_SANDBOX_MODE", "1")
        .stdout(Stdio::piped());
    let mut parent = ChildGuard::spawn("production-reexec host parent", command);
    cleanup.track(parent.pid());

    let stdout = parent.child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut marker = None;
    while marker.is_none() && Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_secs(5))
            && line.contains("MINK_PROBE_INSIDE")
        {
            marker = Some(line);
        }
    }
    let marker =
        marker.unwrap_or_else(|| panic!("the sandboxed probe never reported in ({token})"));
    let (ns_pid, ns_child) =
        parse_probe_marker(&marker).unwrap_or_else(|| panic!("bad probe marker: {marker}"));
    let _ = (ns_pid, ns_child);
    // 宿主可见 PID：argv[0] 是测试二进制且 environ 携带本次唯一 token（沙箱内
    // PID 与 NSpid 在宿主/沙箱两侧视角不同，不能相互换算）。
    let mut payloads = Vec::new();
    wait_until(WAIT_TIMEOUT, || {
        payloads = pids_with_token_and_argv0(&exe_str, &token);
        !payloads.is_empty()
    });
    assert!(
        !payloads.is_empty(),
        "the sandboxed payload is not visible on the host: {marker}"
    );
    let inside_pid = payloads[0];
    cleanup.track(inside_pid);
    // 孙进程从宿主侧按 PPid 扫描发现（宿主看不到沙箱命名空间内的 children 列表）。
    let grandchild_pid = *pids_with_ppid(inside_pid)
        .iter()
        .find(|pid| process_named(**pid, "sleep"))
        .unwrap_or_else(|| panic!("no sandbox grandchild under pid {inside_pid}: {marker}"));
    cleanup.track(grandchild_pid);
    let monitors = cleanup.track_matching(&exe_str, Some("bwrap"), &[]);
    assert!(
        !monitors.is_empty(),
        "the production reexec must exec the bwrap monitor"
    );

    // 宿主视角复验：该进程确实处于嵌套 PID 命名空间，孙进程挂在它下面。
    let nspid_host = proc_status_field(inside_pid, "NSpid").unwrap_or_default();
    assert!(
        nspid_host.split_whitespace().count() >= 2,
        "expected a nested PID namespace for host pid {inside_pid}: NSpid={nspid_host:?}"
    );
    assert_eq!(proc_ppid(grandchild_pid), Some(inside_pid));

    // 宿主父死 → PDEATHSIG → 监工死 → 命名空间回收（不能只杀得到父 shell）。
    parent.kill_and_wait();
    assert!(
        wait_until(WAIT_TIMEOUT, || {
            !proc_alive(inside_pid)
                && !proc_alive(grandchild_pid)
                && pids_matching(&exe_str, None, &[std::process::id()]).is_empty()
        }),
        "the production sandbox tree survived host parent death: {marker}"
    );
    assert!(cleanup.wait_gone(WAIT_TIMEOUT), "cleanup recorded leaks");
}
