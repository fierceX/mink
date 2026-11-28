//! AutoAgents-Bench style concurrent workload runner for `mink` (`mink::runtime`).
//!
//! Simulates the `liquidos-ai/autoagents-bench` workload (default: 50 requests,
//! concurrency 10) in both `llm-only` and `tool` (ReAct trip-data aggregation)
//! modes, across `cold` (start -> turn -> shutdown per request) and `warm`
//! (persistent worker pool) lifecycles, using an in-process mock `LlmBackend`
//! to isolate framework overhead.
//!
//! Run:
//!   cargo run --release -p mink-core --example autoagents_bench -- --mode tool --lifecycle cold

use anyhow::{Result, bail};
use futures::stream;
use mink::prelude::{AgentOptions, AgentRuntime, AgentTool, ToolError, ToolOutput};
use mink::runtime::llm::{
    LlmBackend, LlmEvent, LlmRequest, LlmResponseStream, LlmStopEvent, LlmTextEvent,
    LlmToolCallEvent, LlmUsageEvent,
};
use mink::runtime::tools::{ToolDefinition, ToolExecutionContext};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BenchMode {
    Tool,
    LlmOnly,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    Cold,
    Warm,
}

struct BenchArgs {
    mode: BenchMode,
    lifecycle: Lifecycle,
    requests: usize,
    concurrency: usize,
    log_events: bool,
}

impl BenchArgs {
    fn parse() -> Result<Self> {
        let mut mode = BenchMode::Tool;
        let mut lifecycle = Lifecycle::Cold;
        let mut requests = 50usize;
        let mut concurrency = 10usize;
        let mut log_events = false;

        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--mode" => match iter.next().as_deref() {
                    Some("tool") => mode = BenchMode::Tool,
                    Some("llm-only") => mode = BenchMode::LlmOnly,
                    other => bail!("invalid --mode {other:?}; expected 'tool' or 'llm-only'"),
                },
                "--lifecycle" => match iter.next().as_deref() {
                    Some("cold") => lifecycle = Lifecycle::Cold,
                    Some("warm") => lifecycle = Lifecycle::Warm,
                    other => bail!("invalid --lifecycle {other:?}; expected 'cold' or 'warm'"),
                },
                "--requests" => {
                    let val = iter
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--requests needs a value"))?;
                    requests = val.parse()?;
                }
                "--concurrency" => {
                    let val = iter
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--concurrency needs a value"))?;
                    concurrency = val.parse()?;
                }
                "--log-events" => log_events = true,
                other => bail!("unknown argument {other:?}"),
            }
        }
        Ok(Self {
            mode,
            lifecycle,
            requests: requests.max(1),
            concurrency: concurrency.max(1),
            log_events,
        })
    }
}

struct TripDataTool {
    rows: Arc<Vec<(u64, f64)>>,
}

impl TripDataTool {
    fn new() -> Self {
        let rows = (0..1_000u64)
            .map(|i| (300 + (i % 900), 1.5 + (i % 20) as f64 * 0.4))
            .collect();
        Self {
            rows: Arc::new(rows),
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for TripDataTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "trip_data",
            "Compute trip count, average duration (sec), and total distance from the trip dataset.",
            json!({
                "type": "object",
                "properties": {
                    "metric": { "type": "string" }
                },
                "required": ["metric"],
                "additionalProperties": false
            }),
        )
    }

    async fn execute(
        &self,
        input: Value,
        _ctx: ToolExecutionContext,
    ) -> std::result::Result<ToolOutput, ToolError> {
        let metric = input
            .get("metric")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::argument("metric is required"))?;
        let count = self.rows.len() as u64;
        let total_dur: u64 = self.rows.iter().map(|(d, _)| *d).sum();
        let total_dist: f64 = self.rows.iter().map(|(_, dist)| *dist).sum();
        let avg_dur = total_dur as f64 / count as f64;
        let payload = json!({
            "metric": metric,
            "trips": count,
            "avg_duration_sec": avg_dur,
            "total_distance_km": total_dist,
        });
        Ok(ToolOutput::text(payload.to_string()).with_structured_value(payload))
    }
}

struct BenchMockBackend {
    mode: BenchMode,
    seq: AtomicU64,
}

impl BenchMockBackend {
    fn new(mode: BenchMode) -> Self {
        Self {
            mode,
            seq: AtomicU64::new(1),
        }
    }
}

fn last_user_has_tool_result(messages: &[Value]) -> bool {
    let Some(last) = messages.last() else {
        return false;
    };
    if last.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    last.get("content")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

#[async_trait::async_trait]
impl LlmBackend for BenchMockBackend {
    fn name(&self) -> &str {
        "autoagents-bench-mock"
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        if request.cancel.is_cancelled() {
            bail!("cancelled");
        }
        let events: Vec<Result<LlmEvent>> = match self.mode {
            BenchMode::LlmOnly => vec![
                Ok(LlmEvent::Text(LlmTextEvent {
                    content: r#"{"trips":1000,"avg_duration_sec":749.5}"#.to_string(),
                })),
                Ok(LlmEvent::Usage(LlmUsageEvent {
                    input_tokens: 128,
                    output_tokens: 24,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                })),
                Ok(LlmEvent::Stop(LlmStopEvent {
                    reason: "end_turn".to_string(),
                })),
            ],
            BenchMode::Tool if last_user_has_tool_result(&request.messages) => vec![
                Ok(LlmEvent::Text(LlmTextEvent {
                    content: r#"{"trips":1000,"avg_duration_sec":749.5}"#.to_string(),
                })),
                Ok(LlmEvent::Usage(LlmUsageEvent {
                    input_tokens: 256,
                    output_tokens: 24,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                })),
                Ok(LlmEvent::Stop(LlmStopEvent {
                    reason: "end_turn".to_string(),
                })),
            ],
            BenchMode::Tool => {
                let id = format!("call_trip_{}", self.seq.fetch_add(1, Ordering::Relaxed));
                let input = json!({"metric": "avg_duration"});
                let fields = BTreeMap::from([("metric".to_string(), "avg_duration".to_string())]);
                vec![
                    Ok(LlmEvent::ToolCall(LlmToolCallEvent {
                        name: "trip_data".to_string(),
                        id,
                        input_json: input,
                        fields,
                        parse_error: None,
                        raw_arguments_digest: None,
                    })),
                    Ok(LlmEvent::Usage(LlmUsageEvent {
                        input_tokens: 180,
                        output_tokens: 20,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                    })),
                    Ok(LlmEvent::Stop(LlmStopEvent {
                        reason: "tool_use".to_string(),
                    })),
                ]
            }
        };
        Ok(LlmResponseStream {
            events: Box::pin(stream::iter(events)),
            attempt_count: 1,
        })
    }
}

fn build_options(
    session_dir: PathBuf,
    cwd: PathBuf,
    mode: BenchMode,
    log_events: bool,
    backend: Arc<BenchMockBackend>,
    tool: Arc<TripDataTool>,
) -> AgentOptions {
    let mut opts = AgentOptions::new(session_dir, cwd)
        .with_model("flash")
        .with_log_events(log_events)
        .with_llm_backend(backend);
    opts = match mode {
        BenchMode::LlmOnly => opts.with_enabled_tools(Vec::<String>::new()),
        BenchMode::Tool => opts
            .with_tool(tool)
            .with_enabled_tools(["trip_data".to_string()]),
    };
    opts
}

fn percentile(sorted_ms: &[f64], p: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let idx = ((sorted_ms.len() as f64 - 1.0) * p).round() as usize;
    sorted_ms[idx.min(sorted_ms.len() - 1)]
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = BenchArgs::parse()?;
    let root = std::env::temp_dir().join(format!("mink-autoagents-bench-{}", std::process::id()));
    let cwd = root.join("workspace");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&cwd)?;

    let backend = Arc::new(BenchMockBackend::new(args.mode));
    let tool = Arc::new(TripDataTool::new());
    let prompt = "Compute average trip duration from the trip dataset and return JSON.";

    let mut cold_starts_ms = Vec::new();
    let mut latencies_ms = Vec::with_capacity(args.requests);
    let mut deterministic_ok = 0usize;

    let wall_start = Instant::now();
    match args.lifecycle {
        Lifecycle::Cold => {
            let sem = Arc::new(Semaphore::new(args.concurrency));
            let mut tasks = tokio::task::JoinSet::new();
            for idx in 0..args.requests {
                let permit = sem.clone().acquire_owned().await?;
                let session_dir = root.join(format!("session-{idx}"));
                let cwd_c = cwd.clone();
                let backend_c = backend.clone();
                let tool_c = tool.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let total_start = Instant::now();
                    let start_t = Instant::now();
                    let rt = AgentRuntime::start(build_options(
                        session_dir,
                        cwd_c,
                        args.mode,
                        args.log_events,
                        backend_c,
                        tool_c,
                    ))
                    .await?;
                    let cold_ms = start_t.elapsed().as_secs_f64() * 1000.0;
                    let outcome = rt.run_turn(prompt).await?;
                    rt.shutdown().await?;
                    let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
                    let ok = outcome.error.is_none()
                        && outcome.text.contains("\"avg_duration_sec\":749.5");
                    Ok::<(f64, f64, bool), anyhow::Error>((cold_ms, total_ms, ok))
                });
            }
            while let Some(res) = tasks.join_next().await {
                let (cold_ms, total_ms, ok) = res??;
                cold_starts_ms.push(cold_ms);
                latencies_ms.push(total_ms);
                if ok {
                    deterministic_ok += 1;
                }
            }
        }
        Lifecycle::Warm => {
            let mut runtimes = Vec::with_capacity(args.concurrency);
            for idx in 0..args.concurrency {
                let start_t = Instant::now();
                let rt = AgentRuntime::start(build_options(
                    root.join(format!("worker-{idx}")),
                    cwd.clone(),
                    args.mode,
                    args.log_events,
                    backend.clone(),
                    tool.clone(),
                ))
                .await?;
                cold_starts_ms.push(start_t.elapsed().as_secs_f64() * 1000.0);
                runtimes.push(rt);
            }
            let handles: Vec<_> = runtimes.iter().map(|rt| rt.handle()).collect();
            let mut tasks = tokio::task::JoinSet::new();
            let mut next_req = 0usize;
            while next_req < args.requests {
                let batch_end = (next_req + args.concurrency).min(args.requests);
                for (slot, _) in (next_req..batch_end).enumerate() {
                    let handle = handles[slot].clone();
                    tasks.spawn(async move {
                        let start = Instant::now();
                        let outcome = handle.run_turn(prompt).await?;
                        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                        let ok = outcome.error.is_none()
                            && outcome.text.contains("\"avg_duration_sec\":749.5");
                        Ok::<(f64, bool), anyhow::Error>((elapsed_ms, ok))
                    });
                }
                while let Some(res) = tasks.join_next().await {
                    let (elapsed_ms, ok) = res??;
                    latencies_ms.push(elapsed_ms);
                    if ok {
                        deterministic_ok += 1;
                    }
                }
                next_req = batch_end;
            }
            for rt in runtimes {
                rt.shutdown().await?;
            }
        }
    }
    let wall_secs = wall_start
        .elapsed()
        .max(Duration::from_nanos(1))
        .as_secs_f64();

    cold_starts_ms.sort_by(f64::total_cmp);
    latencies_ms.sort_by(f64::total_cmp);
    let avg_ms = latencies_ms.iter().sum::<f64>() / latencies_ms.len() as f64;
    let avg_cold_ms = cold_starts_ms.iter().sum::<f64>() / cold_starts_ms.len() as f64;
    let throughput_rps = args.requests as f64 / wall_secs;

    println!(
        "{}",
        json!({
            "framework": format!("mink (mink-core {}, mock backend)", env!("CARGO_PKG_VERSION")),
            "mode": match args.mode { BenchMode::Tool => "tool", BenchMode::LlmOnly => "llm-only" },
            "lifecycle": match args.lifecycle { Lifecycle::Cold => "cold", Lifecycle::Warm => "warm" },
            "requests": args.requests,
            "concurrency": args.concurrency,
            "deterministic_ok": deterministic_ok,
            "wall_secs": wall_secs,
            "throughput_rps": throughput_rps,
            "cold_start_avg_ms": avg_cold_ms,
            "cold_start_p50_ms": percentile(&cold_starts_ms, 0.50),
            "latency_avg_ms": avg_ms,
            "latency_p50_ms": percentile(&latencies_ms, 0.50),
            "latency_p95_ms": percentile(&latencies_ms, 0.95),
            "latency_p99_ms": percentile(&latencies_ms, 0.99),
        })
    );

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}
