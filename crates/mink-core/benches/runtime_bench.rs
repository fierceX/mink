//! mink runtime benchmark suite.
//!
//! Entry points:
//! - `cargo bench -p mink-core --bench runtime_bench -- [--case NAME]... [--reps N] [--quick] [--json PATH] [--list]`
//! - `./scripts/bench-runtime.sh` (collects machine/OS/commit/rustc metadata and stores a report)
//!
//! Metric conventions (recorded in the JSON `meta` section):
//! - Latency: P50 / P95 / min / max / mean over a repetition set, unit `ms`.
//! - Memory: current RSS from `VmRSS` (Linux) or `ps -o rss=` (macOS); peak uses
//!   a 20ms sampler on both platforms. On macOS RSS overstates the
//!   real footprint (allocator retention); compare with `vmmap` when in doubt.
//! - All LLM traffic goes through an injected mock backend, so these numbers
//!   measure mink's own overhead rather than provider latency.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::stream;
use mink::prelude::*;
use serde_json::json;

static MOCK_CALLS: AtomicU64 = AtomicU64::new(0);
static SUMMARY_CALLS: AtomicU64 = AtomicU64::new(0);
static SUBAGENT_CALLS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum MockMode {
    Echo,
    OneTool,
    Fanout(usize),
    Stream(usize),
}

struct MockBackend {
    mode: MockMode,
}

#[async_trait::async_trait]
impl LlmBackend for MockBackend {
    fn name(&self) -> &str {
        "bench-mock"
    }

    async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        MOCK_CALLS.fetch_add(1, Ordering::Relaxed);
        if matches!(request.purpose, LlmPurpose::Compaction) {
            SUMMARY_CALLS.fetch_add(1, Ordering::Relaxed);
            return Ok(events(vec![
                LlmEvent::Text(LlmTextEvent {
                    content: "benchmark summary".into(),
                }),
                LlmEvent::Usage(LlmUsageEvent {
                    input_tokens: 800,
                    output_tokens: 20,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                }),
                LlmEvent::Stop(LlmStopEvent {
                    reason: "end_turn".into(),
                }),
            ]));
        }
        if matches!(request.purpose, LlmPurpose::SubAgent { .. }) {
            SUBAGENT_CALLS.fetch_add(1, Ordering::Relaxed);
            return Ok(echo_response());
        }

        let tool_results = request
            .messages
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
            .count();
        let user_text = last_user_text(&request);
        let asked = match self.mode {
            MockMode::OneTool => user_text.contains(".txt"),
            MockMode::Fanout(_) => user_text.contains("fanout benchmark"),
            _ => false,
        };
        let already_called = assistant_tool_round_since_user(&request);

        match self.mode {
            MockMode::OneTool if asked && !already_called => {
                let target =
                    last_user_token(&request, ".txt").unwrap_or_else(|| "bench.txt".into());
                Ok(tool_call_response(
                    "Read",
                    json!({"path": target}),
                    "call-bench-read",
                ))
            }
            MockMode::Fanout(n) if asked && !already_called && tool_results < n => {
                let mut list = Vec::new();
                for i in 0..n {
                    let input = json!({
                        "prompt": format!("benchmark subtask {i}"),
                        "description": format!("fanout {i}"),
                    });
                    list.push(LlmEvent::ToolCall(LlmToolCallEvent {
                        name: "SubAgent".into(),
                        id: format!("call-bench-sub-{i}"),
                        fields: tool_fields(&input),
                        input_json: input,
                        parse_error: None,
                        raw_arguments_digest: None,
                    }));
                }
                list.push(LlmEvent::Stop(LlmStopEvent {
                    reason: "tool_calls".into(),
                }));
                Ok(events(list))
            }
            MockMode::Stream(chunks) => {
                let mut list = Vec::new();
                for i in 0..chunks {
                    list.push(LlmEvent::Text(LlmTextEvent {
                        content: format!("chunk-{i:05}-{}", "x".repeat(52)),
                    }));
                }
                list.push(LlmEvent::Usage(LlmUsageEvent {
                    input_tokens: 1234,
                    output_tokens: (chunks as i64) * 15,
                    cache_read_input_tokens: 1000,
                    cache_creation_input_tokens: 0,
                }));
                list.push(LlmEvent::Stop(LlmStopEvent {
                    reason: "end_turn".into(),
                }));
                Ok(events(list))
            }
            _ => Ok(echo_response()),
        }
    }
}

fn events(events: Vec<LlmEvent>) -> LlmResponseStream {
    let events: Vec<anyhow::Result<LlmEvent>> = events.into_iter().map(Ok).collect();
    LlmResponseStream {
        events: Box::pin(stream::iter(events)),
        attempt_count: 1,
    }
}

fn echo_response() -> LlmResponseStream {
    events(vec![
        LlmEvent::Thinking(LlmThinkingEvent {
            content: "benchmark thinking".into(),
        }),
        LlmEvent::Text(LlmTextEvent {
            content: "benchmark response".into(),
        }),
        LlmEvent::Usage(LlmUsageEvent {
            input_tokens: 1234,
            output_tokens: 12,
            cache_read_input_tokens: 1000,
            cache_creation_input_tokens: 0,
        }),
        LlmEvent::Stop(LlmStopEvent {
            reason: "end_turn".into(),
        }),
    ])
}

fn tool_call_response(name: &str, input: serde_json::Value, id: &str) -> LlmResponseStream {
    events(vec![
        LlmEvent::ToolCall(LlmToolCallEvent {
            name: name.into(),
            id: id.into(),
            fields: tool_fields(&input),
            input_json: input,
            parse_error: None,
            raw_arguments_digest: None,
        }),
        LlmEvent::Stop(LlmStopEvent {
            reason: "tool_calls".into(),
        }),
    ])
}

fn tool_fields(input: &serde_json::Value) -> BTreeMap<String, String> {
    input
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, value)| {
            (
                key.clone(),
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string()),
            )
        })
        .collect()
}

fn last_user_token(request: &LlmRequest, suffix: &str) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .find_map(|m| {
            (m.get("role").and_then(|r| r.as_str()) == Some("user"))
                .then(|| m.get("content").and_then(|c| c.as_str()).unwrap_or(""))
        })
        .and_then(|text| {
            text.split_whitespace()
                .find(|token| token.ends_with(suffix))
                .map(str::to_string)
        })
}

fn last_user_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find_map(|m| {
            (m.get("role").and_then(|r| r.as_str()) == Some("user"))
                .then(|| m.get("content").and_then(|c| c.as_str()).unwrap_or(""))
        })
        .unwrap_or_default()
        .to_string()
}

/// Whether an assistant message after the latest string-content user turn
/// already carries tool calls, i.e. this round has dispatched tools already.
fn assistant_tool_round_since_user(request: &LlmRequest) -> bool {
    for message in request.messages.iter().rev() {
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
        if role == "user" && message.get("content").is_some_and(|c| c.is_string()) {
            return false;
        }
        if role == "assistant" && message.get("tool_calls").is_some() {
            return true;
        }
    }
    false
}

// ── environment helpers ─────────────────────────────────────────────

struct Args {
    cases: Vec<String>,
    reps: Option<usize>,
    quick: bool,
    json: Option<PathBuf>,
    list: bool,
}

fn parse_args(raw: &[String]) -> anyhow::Result<Args> {
    let mut args = Args {
        cases: Vec::new(),
        reps: None,
        quick: false,
        json: None,
        list: false,
    };
    let mut iter = raw.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--case" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--case needs a value"))?;
                args.cases.extend(
                    value
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            }
            "--reps" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--reps needs a value"))?;
                args.reps = Some(value.parse()?);
                anyhow::ensure!(args.reps != Some(0), "--reps must be greater than zero");
            }
            "--quick" => args.quick = true,
            "--json" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--json needs a value"))?;
                args.json = Some(PathBuf::from(value));
            }
            "--list" => args.list = true,
            "--noop" | "--bench" => {}
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(args)
}

fn case_names(quick: bool) -> Vec<String> {
    let mut names = vec![
        "process_start".to_string(),
        "process_start_cli".to_string(),
        "runtime_start".to_string(),
        "session_create".to_string(),
        "idle_1".to_string(),
        "idle_100".to_string(),
    ];
    if !quick {
        names.push("idle_1000".to_string());
    }
    names.extend([
        "mock_turn_no_tool".to_string(),
        "mock_turn_1_tool".to_string(),
        "sse_1k".to_string(),
    ]);
    if !quick {
        names.push("sse_10k".to_string());
    }
    names.push("replay_1k".to_string());
    names.push("replay_10k".to_string());
    if !quick {
        names.push("replay_100k".to_string());
    }
    names.push("turns_500".to_string());
    if !quick {
        names.push("turns_2000".to_string());
    }
    names.push("compact_10".to_string());
    if !quick {
        names.push("compact_100".to_string());
    }
    names.push("concurrent_32".to_string());
    names.push("concurrent_64".to_string());
    if !quick {
        names.push("concurrent_128".to_string());
    }
    names.push("subagent_fanout_2".to_string());
    names.push("subagent_fanout_4".to_string());
    if !quick {
        names.push("subagent_fanout_8".to_string());
    }
    names.push("sandbox_start".to_string());
    names
}

fn select_cases(args: &Args) -> Vec<String> {
    let all = case_names(args.quick);
    if args.cases.is_empty() {
        return all;
    }
    all.into_iter()
        .filter(|name| args.cases.iter().any(|sel| name.starts_with(sel.as_str())))
        .collect()
}

fn reps(args: &Args, normal: usize, quick: usize) -> usize {
    args.reps.unwrap_or(if args.quick { quick } else { normal })
}

fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("mink-bench-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create bench root");
    root
}

fn cleanup(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

fn options(home: &Path, cwd: &Path, mode: MockMode) -> AgentOptions {
    AgentOptions::new(
        home.to_string_lossy().to_string(),
        cwd.to_string_lossy().to_string(),
    )
    .with_api_key("bench-key")
    .with_model("bench-model")
    .with_session(SessionPolicy::New)
    .with_llm_backend(Arc::new(MockBackend { mode }))
}

fn options_for(home: &Path, cwd: &Path, mode: MockMode, policy: SessionPolicy) -> AgentOptions {
    options(home, cwd, mode).with_session(policy)
}

fn context_policy_zero() -> ContextPolicy {
    ContextPolicy {
        max_context_tokens: 0,
        ..ContextPolicy::default()
    }
}

fn bench_file(dir: &Path, lines: usize) -> PathBuf {
    let path = dir.join("bench.txt");
    let content = (0..lines)
        .map(|i| format!("line {i}: benchmark content"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, content).expect("write bench.txt");
    path
}

fn file_kb(path: &Path) -> f64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64 / 1024.0
}

fn rss_metric_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "vmrss_kb"
    } else if cfg!(target_os = "macos") {
        "ps_rss_kb"
    } else {
        "unsupported"
    }
}

#[cfg(target_os = "linux")]
fn read_status_kb(field: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            let value = rest.trim().trim_end_matches(" kB").trim();
            return value.parse().ok();
        }
    }
    None
}

fn rss_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        read_status_kb("VmRSS:")
    }
    #[cfg(target_os = "macos")]
    {
        let pid = std::process::id().to_string();
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

fn thread_count() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        read_status_kb("Threads:")
    }
    #[cfg(target_os = "macos")]
    {
        // macOS `ps` has no `thcount` keyword: count `ps -M` thread rows.
        let pid = std::process::id().to_string();
        let out = std::process::Command::new("ps")
            .args(["-M", "-p", &pid])
            .output()
            .ok()?;
        let rows = String::from_utf8_lossy(&out.stdout).lines().count();
        Some(rows.saturating_sub(1) as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

fn fd_count() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/fd")
            .ok()
            .map(|entries| entries.count() as u64)
    }
    #[cfg(target_os = "macos")]
    {
        let pid = std::process::id().to_string();
        let out = std::process::Command::new("lsof")
            .args(["-p", &pid])
            .output()
            .ok()?;
        let rows = String::from_utf8_lossy(&out.stdout).lines().count();
        Some(rows.saturating_sub(1) as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

struct RssTracker {
    stop: Option<Arc<AtomicBool>>,
    handle: Option<std::thread::JoinHandle<()>>,
    peak: Arc<AtomicU64>,
}

impl RssTracker {
    fn start() -> Self {
        let peak = Arc::new(AtomicU64::new(rss_kb().unwrap_or(0)));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_peak = peak.clone();
        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                if let Some(value) = rss_kb() {
                    thread_peak.fetch_max(value, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Self {
            stop: Some(stop),
            handle: Some(handle),
            peak,
        }
    }

    fn stop(mut self) -> u64 {
        self.finish();
        self.peak.load(Ordering::Relaxed)
    }

    fn finish(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RssTracker {
    fn drop(&mut self) {
        self.finish();
    }
}

fn os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
        {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !version.is_empty() {
                return format!("macOS {version}");
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
            for line in text.lines() {
                if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
                    return value.trim_matches('"').to_string();
                }
            }
        }
    }
    std::env::consts::OS.to_string()
}

// ── result plumbing ─────────────────────────────────────────────────

struct Sample {
    case: String,
    unit: String,
    n: usize,
    p50: f64,
    p95: f64,
    min: f64,
    max: f64,
    mean: f64,
    extra: serde_json::Value,
}

fn summarize(case: &str, unit: &str, durations: &[Duration], extra: serde_json::Value) -> Sample {
    let mut values: Vec<f64> = durations.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pick = |p: f64| -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        let idx = ((values.len() as f64 - 1.0) * p).round() as usize;
        values[idx.min(values.len() - 1)]
    };
    let mean = if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    };
    Sample {
        case: case.to_string(),
        unit: unit.to_string(),
        n: values.len(),
        p50: pick(0.5),
        p95: pick(0.95),
        min: values.first().copied().unwrap_or(0.0),
        max: values.last().copied().unwrap_or(0.0),
        mean,
        extra,
    }
}

fn print_sample(sample: &Sample) {
    let extra = serde_json::to_string(&sample.extra).unwrap_or_default();
    println!(
        "[case] {} n={} p50={:.2}{} p95={:.2}{} min={:.2} max={:.2} mean={:.2} extra={}",
        sample.case,
        sample.n,
        sample.p50,
        sample.unit,
        sample.p95,
        sample.unit,
        sample.min,
        sample.max,
        sample.mean,
        extra
    );
}

async fn timed_start(
    home: &Path,
    cwd: &Path,
    mode: MockMode,
    policy: SessionPolicy,
) -> anyhow::Result<(AgentRuntime, Duration)> {
    let start = Instant::now();
    let runtime = AgentRuntime::start(options_for(home, cwd, mode, policy)).await?;
    Ok((runtime, start.elapsed()))
}

// ── cases ───────────────────────────────────────────────────────────

fn case_process_start(args: &Args) -> anyhow::Result<Sample> {
    let exe = std::env::current_exe()?;
    let count = reps(args, 50, 20);
    let mut durations = Vec::new();
    for _ in 0..count {
        let start = Instant::now();
        let output = std::process::Command::new(&exe).arg("--noop").output()?;
        anyhow::ensure!(output.status.success(), "noop child failed");
        durations.push(start.elapsed());
    }
    Ok(summarize("process_start", "ms", &durations, json!({})))
}

fn case_process_start_cli(args: &Args) -> anyhow::Result<Option<Sample>> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let candidate = std::env::var("MINK_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("../../target/release/mink"));
    let binary = match candidate.canonicalize() {
        Ok(path) if path.is_file() => path,
        _ => return Ok(None),
    };
    let count = reps(args, 50, 20);
    let mut durations = Vec::new();
    for _ in 0..count {
        let start = Instant::now();
        let output = std::process::Command::new(&binary)
            .arg("--version")
            .output()?;
        anyhow::ensure!(output.status.success(), "mink --version failed");
        durations.push(start.elapsed());
    }
    Ok(Some(summarize(
        "process_start_cli",
        "ms",
        &durations,
        json!({"binary": binary.display().to_string()}),
    )))
}

async fn case_runtime_start(args: &Args) -> anyhow::Result<Sample> {
    let root = temp_root("runtime-start");
    let (seed, _) = timed_start(&root, &root, MockMode::Echo, SessionPolicy::New).await?;
    let session_id = seed.session_info().session_id.clone();
    seed.shutdown().await?;

    let count = reps(args, 20, 10);
    let mut durations = Vec::new();
    for _ in 0..count {
        let start = Instant::now();
        let runtime = AgentRuntime::start(options_for(
            &root,
            &root,
            MockMode::Echo,
            SessionPolicy::UseOrCreate(session_id.clone()),
        ))
        .await?;
        durations.push(start.elapsed());
        runtime.shutdown().await?;
    }
    cleanup(&root);
    Ok(summarize("runtime_start", "ms", &durations, json!({})))
}

async fn case_session_create(args: &Args) -> anyhow::Result<Sample> {
    let root = temp_root("session-create");
    let count = reps(args, 20, 10);
    let mut durations = Vec::new();
    for _ in 0..count {
        let start = Instant::now();
        let runtime = AgentRuntime::start(options(&root, &root, MockMode::Echo)).await?;
        durations.push(start.elapsed());
        runtime.shutdown().await?;
    }
    cleanup(&root);
    Ok(summarize("session_create", "ms", &durations, json!({})))
}

async fn case_idle(count: usize) -> anyhow::Result<Sample> {
    let root = temp_root(&format!("idle-{count}"));
    let rss_before = rss_kb().unwrap_or(0);
    let fds_before = fd_count().unwrap_or(0);
    let threads_before = thread_count().unwrap_or(0);
    let tracker = RssTracker::start();
    let mut runtimes = Vec::new();
    let mut durations = Vec::new();
    let mut failure: Option<String> = None;
    let create_start = Instant::now();
    for index in 0..count {
        let home = root.join(format!("s{index}"));
        match timed_start(&home, &root, MockMode::Echo, SessionPolicy::New).await {
            Ok((runtime, elapsed)) => {
                durations.push(elapsed);
                runtimes.push(runtime);
            }
            Err(error) => {
                failure = Some(format!("{error:#}"));
                break;
            }
        }
    }
    let created = runtimes.len();
    let create_total = create_start.elapsed();
    let rss_after = rss_kb().unwrap_or(0);
    let fds_after = fd_count().unwrap_or(0);
    let threads = thread_count().unwrap_or(0);
    let shutdown_start = Instant::now();
    for runtime in runtimes {
        runtime.shutdown().await?;
    }
    let shutdown_total = shutdown_start.elapsed();
    let peak = tracker.stop();
    let extra = json!({
        "requested": count,
        "created": created,
        "failure_at": if failure.is_some() { Some(created) } else { None },
        "failure": failure,
        "rss_before_kb": rss_before,
        "rss_after_kb": rss_after,
        "rss_peak_kb": peak,
        "delta_per_runtime_kb": if created == 0 { 0.0 } else { (rss_after.saturating_sub(rss_before)) as f64 / created as f64 },
        "fds_before": fds_before,
        "fds_after": fds_after,
        "fds_per_runtime": if created == 0 { 0.0 } else { (fds_after.saturating_sub(fds_before)) as f64 / created as f64 },
        "threads_before": threads_before,
        "threads_after": threads,
        "create_total_ms": create_total.as_secs_f64() * 1000.0,
        "shutdown_total_ms": shutdown_total.as_secs_f64() * 1000.0,
        "rss_metric": rss_metric_name(),
    });
    cleanup(&root);
    Ok(summarize(&format!("idle_{count}"), "ms", &durations, extra))
}

async fn case_mock_turn_no_tool(args: &Args) -> anyhow::Result<Sample> {
    let root = temp_root("turn-no-tool");
    let home = root.join("home");
    let (runtime, _) = timed_start(&home, &root, MockMode::Echo, SessionPolicy::New).await?;
    for index in 0..2 {
        runtime.run_turn(format!("warmup {index}")).await?;
    }
    let count = reps(args, 50, 20);
    let tracker = RssTracker::start();
    let mut durations = Vec::new();
    for index in 0..count {
        let start = Instant::now();
        let outcome = runtime.run_turn(format!("bench turn {index}")).await?;
        durations.push(start.elapsed());
        anyhow::ensure!(matches!(outcome.status, TurnStatus::Ok), "turn failed");
    }
    let peak = tracker.stop();
    let info = runtime.session_info();
    let extra = json!({
        "rss_peak_kb": peak,
        "conversation_kb": file_kb(&info.conversation_path),
        "events_kb": file_kb(&info.events_path),
        "usage_kb": file_kb(&info.usage_path),
        "rss_metric": rss_metric_name(),
    });
    runtime.shutdown().await?;
    cleanup(&root);
    Ok(summarize("mock_turn_no_tool", "ms", &durations, extra))
}

async fn case_mock_turn_one_tool(args: &Args) -> anyhow::Result<Sample> {
    let root = temp_root("turn-one-tool");
    let home = root.join("home");
    bench_file(&root, 200);
    let (runtime, _) = timed_start(&home, &root, MockMode::OneTool, SessionPolicy::New).await?;
    runtime.run_turn("warmup").await?;
    let count = reps(args, 30, 15);
    let tracker = RssTracker::start();
    let mut durations = Vec::new();
    for index in 0..count {
        let start = Instant::now();
        let outcome = runtime.run_turn(format!("read bench.txt {index}")).await?;
        durations.push(start.elapsed());
        anyhow::ensure!(matches!(outcome.status, TurnStatus::Ok), "tool turn failed");
        anyhow::ensure!(
            outcome.tool_call_count == 1 && outcome.tool_error_count == 0,
            "expected one successful tool call"
        );
    }
    let peak = tracker.stop();
    let info = runtime.session_info();
    let extra = json!({
        "rss_peak_kb": peak,
        "conversation_kb": file_kb(&info.conversation_path),
        "events_kb": file_kb(&info.events_path),
        "rss_metric": rss_metric_name(),
    });
    runtime.shutdown().await?;
    cleanup(&root);
    Ok(summarize("mock_turn_1_tool", "ms", &durations, extra))
}

async fn case_sse(label: &str, chunks: usize) -> anyhow::Result<Sample> {
    let root = temp_root(label);
    let home = root.join("home");
    let (runtime, _) =
        timed_start(&home, &root, MockMode::Stream(chunks), SessionPolicy::New).await?;
    runtime.run_turn("warmup").await?;
    let tracker = RssTracker::start();
    let rss_before = rss_kb().unwrap_or(0);
    let start = Instant::now();
    let mut stream = runtime
        .stream_turn("sse benchmark")
        .map_err(anyhow::Error::from)?;
    let mut received = 0usize;
    while let Some(event) = stream.recv().await {
        received += 1;
        if matches!(event.kind, AgentEventKind::Final { .. }) {
            break;
        }
    }
    let backlog = stream.progress_backlog();
    let outcome = stream.outcome().await?;
    anyhow::ensure!(
        matches!(outcome.status, TurnStatus::Ok),
        "stream turn failed"
    );
    anyhow::ensure!(
        outcome.text.len() == chunks * 64 && backlog.1 == 0,
        "stream progress was lost"
    );
    let elapsed = start.elapsed();
    let peak = tracker.stop();
    let extra = json!({
        "chunks": chunks,
        "received_events": received,
        "text_bytes": outcome.text.len(),
        "events_per_sec": received as f64 / elapsed.as_secs_f64(),
        "backlog_pending_bytes": backlog.0,
        "backlog_dropped": backlog.1,
        "rss_before_kb": rss_before,
        "rss_peak_kb": peak,
        "rss_metric": rss_metric_name(),
    });
    runtime.shutdown().await?;
    cleanup(&root);
    Ok(summarize(label, "ms", &[elapsed], extra))
}

async fn case_replay(label: &str, messages: usize) -> anyhow::Result<Sample> {
    let root = temp_root(&format!("replay-{messages}"));
    let home = root.join("home");
    let mode = MockMode::Echo;
    let seed = AgentRuntime::start(options(&home, &root, mode)).await?;
    let info = seed.session_info().clone();
    seed.shutdown().await?;

    let mut buffer = String::new();
    for index in 0..messages {
        if index % 2 == 0 {
            buffer.push_str(&format!(
                "{{\"role\":\"user\",\"content\":\"m {index}\"}}\n"
            ));
        } else {
            buffer.push_str(&format!(
                "{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"a {index}\"}}]}}\n"
            ));
        }
    }
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&info.conversation_path)?;
        file.write_all(buffer.as_bytes())?;
    }

    let read_start = Instant::now();
    let rows = mink::runtime::session::SessionReader::new(info.home.clone()).conversation()?;
    let read_elapsed = read_start.elapsed();
    anyhow::ensure!(rows.len() >= messages, "conversation read short");

    let tracker = RssTracker::start();
    let rss_before = rss_kb().unwrap_or(0);
    let start_wall = Instant::now();
    let runtime = AgentRuntime::start(
        options_for(
            &home,
            &root,
            mode,
            SessionPolicy::UseOrCreate(info.session_id.clone()),
        )
        .with_context_policy(context_policy_zero()),
    )
    .await?;
    let start_elapsed = start_wall.elapsed();
    let turn_start = Instant::now();
    let outcome = runtime.run_turn("replay first turn").await?;
    anyhow::ensure!(
        matches!(outcome.status, TurnStatus::Ok),
        "replay turn failed"
    );
    let turn_elapsed = turn_start.elapsed();
    let peak = tracker.stop();
    runtime.shutdown().await?;
    let extra = json!({
        "messages": messages,
        "read_ms": read_elapsed.as_secs_f64() * 1000.0,
        "start_ms": start_elapsed.as_secs_f64() * 1000.0,
        "first_turn_ms": turn_elapsed.as_secs_f64() * 1000.0,
        "rss_before_kb": rss_before,
        "rss_peak_kb": peak,
        "rss_metric": rss_metric_name(),
    });
    cleanup(&root);
    Ok(summarize(label, "ms", &[turn_elapsed], extra))
}

async fn case_turns(total: usize) -> anyhow::Result<Sample> {
    let root = temp_root(&format!("turns-{total}"));
    let home = root.join("home");
    let (runtime, _) = timed_start(&home, &root, MockMode::Echo, SessionPolicy::New).await?;
    for index in 0..3 {
        runtime.run_turn(format!("warmup {index}")).await?;
    }
    let tracker = RssTracker::start();
    let rss_before = rss_kb().unwrap_or(0);
    let sample_every = (total / 4).max(1);
    let mut durations = Vec::new();
    let mut curve = Vec::new();
    for index in 0..total {
        let start = Instant::now();
        let outcome = runtime.run_turn(format!("turn {index}")).await?;
        anyhow::ensure!(
            matches!(outcome.status, TurnStatus::Ok),
            "long-session turn failed"
        );
        durations.push(start.elapsed());
        if (index + 1) % sample_every == 0 {
            curve.push(json!({
                "turn": index + 1,
                "rss_kb": rss_kb().unwrap_or(0),
            }));
        }
    }
    let peak = tracker.stop();
    let info = runtime.session_info();
    let extra = json!({
        "total_turns": total,
        "rss_before_kb": rss_before,
        "rss_peak_kb": peak,
        "rss_curve": curve,
        "conversation_kb": file_kb(&info.conversation_path),
        "events_kb": file_kb(&info.events_path),
        "usage_kb": file_kb(&info.usage_path),
        "rss_metric": rss_metric_name(),
    });
    runtime.shutdown().await?;
    cleanup(&root);
    Ok(summarize(
        &format!("turns_{total}"),
        "ms",
        &durations,
        extra,
    ))
}

async fn case_compact(target: usize) -> anyhow::Result<Sample> {
    let root = temp_root(&format!("compact-{target}"));
    let home = root.join("home");
    let policy = ContextPolicy {
        max_context_tokens: 16_000,
        compact_pct: 50,
        reserve_tokens: 2_000,
        compact_tail_tokens: 2_000,
        compact_max_output_tokens: 1_000,
        ..ContextPolicy::default()
    };
    let start = Instant::now();
    let runtime =
        AgentRuntime::start(options(&home, &root, MockMode::Echo).with_context_policy(policy))
            .await?;
    let start_elapsed = start.elapsed();
    let payload = "规范要求说明：请严格按照规范要求处理本项内容。".repeat(20);
    let base_summaries = SUMMARY_CALLS.load(Ordering::Relaxed);
    let tracker = RssTracker::start();
    let mut turn_count = 0usize;
    let max_turns = target.saturating_mul(40).saturating_add(50);
    let mut ok = 0usize;
    while turn_count < max_turns {
        let outcome = runtime
            .run_turn(format!("{payload} 轮次{turn_count}"))
            .await?;
        turn_count += 1;
        if matches!(outcome.status, TurnStatus::Ok) {
            ok += 1;
        }
        let summaries = SUMMARY_CALLS.load(Ordering::Relaxed) - base_summaries;
        if summaries as usize >= target {
            break;
        }
    }
    let elapsed = start.elapsed();
    let summaries = SUMMARY_CALLS.load(Ordering::Relaxed) - base_summaries;
    let peak = tracker.stop();
    let context_state = home.join("context-state.json");
    let extra = json!({
        "target_compactions": target,
        "summaries": summaries,
        "turns_used": turn_count,
        "ok_turns": ok,
        "start_ms": start_elapsed.as_secs_f64() * 1000.0,
        "ms_per_compaction": if summaries == 0 { 0.0 } else { elapsed.as_secs_f64() * 1000.0 / summaries as f64 },
        "context_state_exists": context_state.exists(),
        "rss_peak_kb": peak,
        "rss_metric": rss_metric_name(),
    });
    runtime.shutdown().await?;
    cleanup(&root);
    Ok(summarize(
        &format!("compact_{target}"),
        "ms",
        &[elapsed],
        extra,
    ))
}

async fn case_concurrent(k: usize) -> anyhow::Result<Sample> {
    let root = temp_root(&format!("concurrent-{k}"));
    let tracker = RssTracker::start();
    let rss_before = rss_kb().unwrap_or(0);
    let wall = Instant::now();
    let mut tasks = Vec::new();
    for index in 0..k {
        let home = root.join(format!("c{index}"));
        let cwd = root.clone();
        tasks.push(tokio::spawn(async move {
            let start = Instant::now();
            let (runtime, _) = timed_start(&home, &cwd, MockMode::Echo, SessionPolicy::New).await?;
            let outcome = runtime.run_turn(format!("concurrent {index}")).await?;
            anyhow::ensure!(
                matches!(outcome.status, TurnStatus::Ok),
                "concurrent turn failed"
            );
            runtime.shutdown().await?;
            anyhow::Ok(start.elapsed())
        }));
    }
    let mut durations = Vec::new();
    for task in tasks {
        durations.push(task.await??);
    }
    let wall = wall.elapsed();
    let peak = tracker.stop();
    let rss_after = rss_kb().unwrap_or(0);
    let extra = json!({
        "concurrency": k,
        "wall_ms": wall.as_secs_f64() * 1000.0,
        "throughput_per_sec": k as f64 / wall.as_secs_f64(),
        "rss_before_kb": rss_before,
        "rss_after_kb": rss_after,
        "rss_peak_kb": peak,
        "rss_metric": rss_metric_name(),
    });
    cleanup(&root);
    Ok(summarize(
        &format!("concurrent_{k}"),
        "ms",
        &durations,
        extra,
    ))
}

async fn case_subagent_fanout(n: usize) -> anyhow::Result<Sample> {
    let root = temp_root(&format!("fanout-{n}"));
    let home = root.join("home");
    let (runtime, _) = timed_start(&home, &root, MockMode::Fanout(n), SessionPolicy::New).await?;
    let tracker = RssTracker::start();
    let subagent_calls = SUBAGENT_CALLS.load(Ordering::Relaxed);
    let start = Instant::now();
    let outcome = runtime
        .run_turn(format!("fanout benchmark {n} subtasks"))
        .await?;
    let elapsed = start.elapsed();
    let peak = tracker.stop();
    let completed_requests = SUBAGENT_CALLS.load(Ordering::Relaxed) - subagent_calls;
    let extra = json!({
        "requested_subagents": n,
        "tool_calls": outcome.tool_call_count,
        "tool_errors": outcome.tool_error_count,
        "subagent_requests": completed_requests,
        "status": format!("{:?}", outcome.status),
        "rss_peak_kb": peak,
        "rss_metric": rss_metric_name(),
    });
    runtime.shutdown().await?;
    cleanup(&root);
    Ok(summarize(
        &format!("subagent_fanout_{n}"),
        "ms",
        &[elapsed],
        extra,
    ))
}

async fn worker_probe(raw: &[String]) -> anyhow::Result<()> {
    let root = raw
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("mink-bench-worker"));
    let inside = raw.iter().any(|arg| arg == "--inside");
    if !inside {
        std::fs::create_dir_all(&root)?;
        let sandbox = SandboxConfig {
            enabled: true,
            backend: "auto".into(),
            read_dirs: vec![root.to_string_lossy().to_string()],
            write_dirs: vec![root.to_string_lossy().to_string()],
            allow_network: false,
            ..Default::default()
        };
        let exe = std::env::current_exe()?;
        let mut args: Vec<String> = std::env::args().collect();
        args.push("--inside".into());
        mink::runtime::reexec_in_sandbox(&sandbox, &exe, &args);
        // Success replaces this process; an unavailable sandbox hard-exits and
        // the parent records the failure under `probe_failures`.
    }
    let home = root.join(format!("worker-home-{}", std::process::id()));
    let stage = |name: &str| {
        println!(
            "{}",
            json!({
                "type": "probe-stage",
                "stage": name,
                "sandboxed": inside,
                "cwd": std::env::current_dir().ok().map(|p| p.display().to_string()),
                "home": home.display().to_string(),
                "tmpdir": std::env::temp_dir().display().to_string(),
            })
        );
    };
    stage("entered");
    std::fs::create_dir_all(&home)?;
    stage("home-created");
    let start = Instant::now();
    let runtime = match AgentRuntime::start(options(&home, &root, MockMode::Echo)).await {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("probe start failed: {error:?}");
            return Err(error.into());
        }
    };
    let start_ms = start.elapsed().as_secs_f64() * 1000.0;
    stage("started");
    let turn_start = Instant::now();
    let outcome = runtime.run_turn("sandbox probe").await?;
    anyhow::ensure!(
        matches!(outcome.status, TurnStatus::Ok),
        "sandbox turn failed"
    );
    let turn_ms = turn_start.elapsed().as_secs_f64() * 1000.0;
    runtime.shutdown().await?;
    println!(
        "{}",
        json!({
            "type": "probe",
            "sandboxed": inside,
            "start_ms": start_ms,
            "turn_ms": turn_ms,
        })
    );
    Ok(())
}

fn case_sandbox(args: &Args) -> anyhow::Result<Sample> {
    let exe = std::env::current_exe()?;
    let root = temp_root("sandbox");
    let count = reps(args, 10, 5);
    let mut durations = Vec::new();
    let mut sandboxed = false;
    let mut inner_start = Vec::new();
    let mut inner_turn = Vec::new();
    let mut diagnostics: Vec<String> = Vec::new();
    let mut successes = 0;
    for _ in 0..count {
        let start = Instant::now();
        let output = std::process::Command::new(&exe)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("TMPDIR", std::env::var("TMPDIR").unwrap_or_default())
            .env("LANG", std::env::var("LANG").unwrap_or_default())
            .arg("--worker-probe")
            .arg(&root)
            .output()?;
        durations.push(start.elapsed());
        let stdout = String::from_utf8_lossy(&output.stdout);
        if output.status.success()
            && let Some(line) = stdout
                .lines()
                .find(|line| line.contains("\"type\":\"probe\""))
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(line)
        {
            sandboxed = value["sandboxed"].as_bool().unwrap_or(false);
            if sandboxed {
                successes += 1;
            }
            if let Some(ms) = value["start_ms"].as_f64() {
                inner_start.push(Duration::from_secs_f64(ms / 1000.0));
            }
            if let Some(ms) = value["turn_ms"].as_f64() {
                inner_turn.push(Duration::from_secs_f64(ms / 1000.0));
            }
        } else if diagnostics.len() < 3 {
            let stderr = String::from_utf8_lossy(&output.stderr);
            diagnostics.push(format!(
                "status={:?} stdout={:?} stderr={:?}",
                output.status.code(),
                stdout.chars().take(200).collect::<String>(),
                stderr.chars().take(300).collect::<String>(),
            ));
        }
    }
    cleanup(&root);
    let extra = json!({
        "sandboxed": sandboxed,
        "probe_successes": successes,
        "probe_requested": count,
        "inner_start_p50_ms": summarize("inner", "ms", &inner_start, json!({})).p50,
        "inner_turn_p50_ms": summarize("inner", "ms", &inner_turn, json!({})).p50,
        "probe_failures": diagnostics,
    });
    Ok(summarize("sandbox_start", "ms", &durations, extra))
}

fn sample_failure(sample: &Sample) -> Option<String> {
    let extra = &sample.extra;
    if sample.n == 0 {
        return Some("no observations".into());
    }
    if let Some(failure) = extra["failure"].as_str() {
        return Some(failure.into());
    }
    if sample.case == "sandbox_start"
        && (extra["probe_successes"] != extra["probe_requested"] || extra["sandboxed"] != true)
    {
        return Some(format!("sandbox probes incomplete: {extra}"));
    }
    if sample.case.starts_with("subagent_fanout_")
        && (extra["status"] != "Ok"
            || extra["tool_errors"] != 0
            || extra["subagent_requests"] != extra["requested_subagents"])
    {
        return Some(format!("subagent execution incomplete: {extra}"));
    }
    if sample.case.starts_with("compact_")
        && (extra["summaries"].as_u64()? < extra["target_compactions"].as_u64()?
            || extra["ok_turns"] != extra["turns_used"]
            || extra["context_state_exists"] != true)
    {
        return Some(format!("compaction workload incomplete: {extra}"));
    }
    None
}

async fn run_case(name: &str, args: &Args) -> anyhow::Result<Option<Sample>> {
    let sample = match name {
        "process_start" => Some(case_process_start(args)?),
        "process_start_cli" => case_process_start_cli(args)?,
        "runtime_start" => Some(case_runtime_start(args).await?),
        "session_create" => Some(case_session_create(args).await?),
        "idle_1" => Some(case_idle(1).await?),
        "idle_100" => Some(case_idle(100).await?),
        "idle_1000" => Some(case_idle(1000).await?),
        "mock_turn_no_tool" => Some(case_mock_turn_no_tool(args).await?),
        "mock_turn_1_tool" => Some(case_mock_turn_one_tool(args).await?),
        "sse_1k" => Some(case_sse("sse_1k", 1_000).await?),
        "sse_10k" => Some(case_sse("sse_10k", 10_000).await?),
        "replay_1k" => Some(case_replay("replay_1k", 1_000).await?),
        "replay_10k" => Some(case_replay("replay_10k", 10_000).await?),
        "replay_100k" => Some(case_replay("replay_100k", 100_000).await?),
        "turns_500" => Some(case_turns(500).await?),
        "turns_2000" => Some(case_turns(2_000).await?),
        "compact_10" => Some(case_compact(10).await?),
        "compact_100" => Some(case_compact(100).await?),
        "concurrent_32" => Some(case_concurrent(32).await?),
        "concurrent_64" => Some(case_concurrent(64).await?),
        "concurrent_128" => Some(case_concurrent(128).await?),
        "subagent_fanout_2" => Some(case_subagent_fanout(2).await?),
        "subagent_fanout_4" => Some(case_subagent_fanout(4).await?),
        "subagent_fanout_8" => Some(case_subagent_fanout(8).await?),
        "sandbox_start" => Some(case_sandbox(args)?),
        _ => None,
    };
    Ok(sample)
}

// ── main ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.first().map(String::as_str) == Some("--worker-probe") {
        return worker_probe(&raw).await;
    }
    if raw.iter().any(|arg| arg == "--noop") {
        return Ok(());
    }

    let args = parse_args(&raw)?;
    if args.list {
        for name in case_names(args.quick) {
            println!("{name}");
        }
        return Ok(());
    }

    let names = select_cases(&args);
    if names.is_empty() {
        anyhow::bail!("no case matched: {:?}", args.cases);
    }
    println!(
        "# mink runtime bench | os={} arch={} cores={} rss_metric={} reps={} quick={}",
        os_version(),
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        rss_metric_name(),
        args.reps
            .map(|r| r.to_string())
            .unwrap_or_else(|| "default".into()),
        args.quick,
    );

    let mut samples = Vec::new();
    let mut failures = Vec::new();
    let mut skipped = Vec::new();
    for name in &names {
        match run_case(name, &args).await {
            Ok(Some(sample)) => {
                print_sample(&sample);
                if let Some(error) = sample_failure(&sample) {
                    eprintln!("[error] {name}: {error}");
                    failures.push(json!({"case": name, "error": error}));
                }
                samples.push(sample);
            }
            Ok(None) => {
                println!("[skip] {name} (not available on this host)");
                skipped.push(name);
            }
            Err(error) => {
                eprintln!("[error] {name}: {error:#}");
                failures.push(json!({"case": name, "error": format!("{error:#}")}));
            }
        }
    }

    if let Some(path) = &args.json {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let report = json!({
            "meta": {
                "report_version": 2,
                "suite": "mink-core/runtime_bench",
                "timestamp_epoch_secs": timestamp,
                "os": os_version(),
                "arch": std::env::consts::ARCH,
                "logical_cores": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
                "rss_metric": rss_metric_name(),
                "fd_limit": std::env::var("MINK_BENCH_FD_LIMIT").ok(),
                "quick": args.quick,
                "reps_override": args.reps,
                "commit": std::env::var("MINK_BENCH_COMMIT").ok(),
                "machine": std::env::var("MINK_BENCH_MACHINE").ok(),
                "kernel": std::env::var("MINK_BENCH_KERNEL").ok(),
                "rustc": std::env::var("MINK_BENCH_RUSTC").ok(),
                "mock_calls": MOCK_CALLS.load(Ordering::Relaxed),
                "summary_calls": SUMMARY_CALLS.load(Ordering::Relaxed),
            },
            "selected_cases": names,
            "failures": failures,
            "skipped_cases": skipped,
            "samples": samples.iter().map(|sample| json!({
                "case": sample.case,
                "unit": sample.unit,
                "n": sample.n,
                "p50": sample.p50,
                "p95": sample.p95,
                "min": sample.min,
                "max": sample.max,
                "mean": sample.mean,
                "extra": sample.extra,
            })).collect::<Vec<_>>(),
        });
        std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
        println!("[report] {}", path.display());
    }
    anyhow::ensure!(
        failures.is_empty(),
        "{} benchmark case(s) failed; see diagnostics/report",
        failures.len()
    );
    Ok(())
}
