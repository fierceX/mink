use crate::agent::belief::BeliefTracker;
use crate::agent::orchestrator::{OrchActor, OrchCmd};
use crate::agent::prefix::PrefixManager;
use crate::agent::sub_coordinator::{SubAgentCoordinator, SubAgentRunner};
use crate::agent::sub_executor::{SubAgentExecutor, SubAgentResult, SubAgentStatus};
use crate::agent::turn::{TurnDecision, TurnExecutor};
use crate::config::{OutputFormat, ResolvedConfig as Config};
use crate::context::{AgentSharedContext, ToolContext};
use crate::guard::collector::{Signal, SignalKind};
use crate::llm::client::{
    LlmBackend, LlmPurpose, LlmRequest, LlmResponseStream, OpenAiCompatibleBackend,
};
use crate::llm::mock::MockLlmBackend;
use crate::protocol::{
    ErrorEvent, Event, RetryEvent, StopEvent, TextEvent, ThinkingEvent, ToolCallEvent, UsageEvent,
};
use crate::session::paths;
use crate::tools::catalog::ToolCatalog;
use crate::tools::runner::{ToolExecution, ToolRunner};
use crate::ui::{Display, StatsSnapshot};
use futures::StreamExt;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct PendingLlmBackend;

#[async_trait::async_trait]
impl LlmBackend for PendingLlmBackend {
    fn name(&self) -> &str {
        "pending"
    }

    async fn stream(&self, _request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::pending()),
            attempt_count: 1,
        })
    }
}

struct IdleAfterTextLlmBackend;

#[async_trait::async_trait]
impl LlmBackend for IdleAfterTextLlmBackend {
    fn name(&self) -> &str {
        "idle-after-text"
    }

    async fn stream(&self, _request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        let stream = futures::stream::iter(vec![Ok(Event::Text(TextEvent {
            content: "partial".into(),
        }))])
        .chain(futures::stream::pending());
        Ok(LlmResponseStream {
            events: Box::pin(stream),
            attempt_count: 1,
        })
    }
}

#[derive(Debug, PartialEq)]
struct CapturedModelTarget {
    model: String,
    alias: Option<String>,
}

struct RecordingCompactionBackend {
    requests: Arc<Mutex<Vec<CapturedModelTarget>>>,
}

#[async_trait::async_trait]
impl LlmBackend for RecordingCompactionBackend {
    fn name(&self) -> &str {
        "recording-compaction"
    }

    async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        assert!(matches!(request.purpose, LlmPurpose::Compaction));
        self.requests.lock().unwrap().push(CapturedModelTarget {
            model: request.model,
            alias: request.model_alias,
        });
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(Event::Text(TextEvent {
                    content: "Current objective and completed work retained.".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

struct FailingCompactionBackend;

#[async_trait::async_trait]
impl LlmBackend for FailingCompactionBackend {
    fn name(&self) -> &str {
        "failing-compaction"
    }

    async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        assert!(matches!(request.purpose, LlmPurpose::Compaction));
        anyhow::bail!("planned compaction failure")
    }
}

#[derive(Debug, PartialEq)]
struct CapturedRoutedRequest {
    purpose: &'static str,
    model: String,
    alias: Option<String>,
}

struct ActiveModelRoutingBackend {
    requests: Arc<Mutex<Vec<CapturedRoutedRequest>>>,
    agent_request_count: AtomicU64,
}

#[async_trait::async_trait]
impl LlmBackend for ActiveModelRoutingBackend {
    fn name(&self) -> &str {
        "active-model-routing"
    }

    async fn stream(&self, request: LlmRequest) -> anyhow::Result<LlmResponseStream> {
        let purpose = match &request.purpose {
            LlmPurpose::Agent => "agent",
            LlmPurpose::SubAgent { .. } => "sub_agent",
            LlmPurpose::Compaction => "compaction",
        };
        self.requests.lock().unwrap().push(CapturedRoutedRequest {
            purpose,
            model: request.model,
            alias: request.model_alias,
        });

        let events = match request.purpose {
            LlmPurpose::Agent if self.agent_request_count.fetch_add(1, Ordering::SeqCst) == 0 => {
                vec![
                    Ok(Event::ToolCall(tool_call(
                        "SubAgent",
                        "call_sub_agent",
                        json!({"prompt":"complete the child task","fork":false}),
                    ))),
                    Ok(Event::Stop(StopEvent {
                        reason: "tool_use".into(),
                    })),
                ]
            }
            LlmPurpose::Agent => vec![
                Ok(Event::Text(TextEvent {
                    content: "parent done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            LlmPurpose::SubAgent { .. } => vec![
                Ok(Event::Text(TextEvent {
                    content: "child done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            LlmPurpose::Compaction => unreachable!("test does not trigger compaction"),
        };
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(events)),
            attempt_count: 1,
        })
    }
}

struct NoopDisplay {
    info: Mutex<Vec<String>>,
    title_models: Mutex<Vec<String>>,
}

impl NoopDisplay {
    fn new() -> Self {
        Self {
            info: Mutex::new(Vec::new()),
            title_models: Mutex::new(Vec::new()),
        }
    }
}

impl Display for NoopDisplay {
    fn render_thinking(&self, _content: &str) {}
    fn render_text(&self, _content: &str) {}
    fn render_tool_call(&self, _call: &crate::ui::ToolCallDisplay<'_>) {}
    fn render_tool_result(&self, _result: &crate::ui::PresentedToolResultDisplay<'_>) {}
    fn render_stop(&self, _reason: &str) {}
    fn render_signal(&self, _kind: &str, _severity: f64, _message: &str) {}
    fn render_error(&self, message: &str) {
        self.info.lock().unwrap().push(format!("error:{message}"));
    }
    fn render_retry(&self) {}
    fn render_info(&self, msg: &str) {
        self.info.lock().unwrap().push(msg.to_string());
    }
    fn render_title_update(&self, model: &str, _stats: &StatsSnapshot) {
        self.title_models.lock().unwrap().push(model.to_string());
    }
    fn render_sub_agent_status(&self, _sid: &str, _st: &str, _it: u64, _ot: u64) {}
    fn render_sub_agent_output(
        &self,
        _sid: &str,
        _st: &str,
        _th: &str,
        _tx: &str,
        _it: u64,
        _ot: u64,
    ) {
    }
    fn render_prompt(&self) {}
    fn render_clear_line(&self) {}
}

struct TestHarness {
    pub(crate) ctx: Arc<AgentSharedContext>,
    cwd: PathBuf,
    display: Arc<NoopDisplay>,
}

/// Test harness whose context (agent requests and compaction alike) is built
/// with `llm_backend` through the production assembly: no post-build field
/// copying, so both exits share one backend.
async fn harness_with_backend(
    name: &str,
    llm_backend: Arc<dyn crate::llm::client::LlmBackend>,
) -> anyhow::Result<TestHarness> {
    harness_with_config(name, false, 300, |_| {}, Some(llm_backend)).await
}

async fn harness(name: &str) -> anyhow::Result<TestHarness> {
    harness_with(name, false, 300).await
}

pub(crate) async fn test_context_for_agent(name: &str) -> anyhow::Result<Arc<AgentSharedContext>> {
    Ok(harness(name).await?.ctx)
}

pub(crate) async fn test_context_for_agent_with_config(
    name: &str,
    configure: impl FnOnce(&mut Config),
) -> anyhow::Result<Arc<AgentSharedContext>> {
    Ok(harness_with_config(name, false, 300, configure, None)
        .await?
        .ctx)
}

/// Test context with a custom API endpoint and the client-test API key.
pub(crate) async fn test_context_for_agent_with_api_url(
    name: &str,
    api_url: &str,
) -> anyhow::Result<Arc<AgentSharedContext>> {
    Ok(harness_with_config(
        name,
        false,
        300,
        |cfg| {
            cfg.base_url = api_url.to_string();
            cfg.api_key = "secret-key".into();
        },
        None,
    )
    .await?
    .ctx)
}

pub(crate) async fn test_context_for_agent_with_config_and_backend(
    name: &str,
    configure: impl FnOnce(&mut Config),
    backend: Arc<dyn LlmBackend>,
) -> anyhow::Result<Arc<AgentSharedContext>> {
    Ok(
        harness_with_config(name, false, 300, configure, Some(backend))
            .await?
            .ctx,
    )
}

async fn harness_with(
    name: &str,
    is_sub_agent: bool,
    sub_agent_timeout_secs: i32,
) -> anyhow::Result<TestHarness> {
    harness_with_config(name, is_sub_agent, sub_agent_timeout_secs, |_| {}, None).await
}

async fn harness_with_config(
    name: &str,
    is_sub_agent: bool,
    sub_agent_timeout_secs: i32,
    configure: impl FnOnce(&mut Config),
    llm_backend: Option<Arc<dyn LlmBackend>>,
) -> anyhow::Result<TestHarness> {
    harness_inner(
        name,
        is_sub_agent,
        sub_agent_timeout_secs,
        configure,
        llm_backend,
        None,
    )
    .await
}

/// Internal test hook: build the harness with an injected event-log writer so
/// real writer failures (missing directory, stalled queue) can be exercised
/// without touching production APIs.
pub(crate) async fn context_with_event_log(
    name: &str,
    writer: crate::session::event_log::EventLogWriter,
) -> anyhow::Result<Arc<AgentSharedContext>> {
    Ok(harness_inner(name, false, 300, |_| {}, None, Some(writer))
        .await?
        .ctx)
}

async fn harness_inner(
    name: &str,
    is_sub_agent: bool,
    sub_agent_timeout_secs: i32,
    configure: impl FnOnce(&mut Config),
    llm_backend: Option<Arc<dyn LlmBackend>>,
    event_log_writer: Option<crate::session::event_log::EventLogWriter>,
) -> anyhow::Result<TestHarness> {
    static CNT: AtomicU64 = AtomicU64::new(0);
    let n = CNT.fetch_add(1, Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!(
        "mink-regression-{}-{}-{n}",
        std::process::id(),
        name
    ));
    let home = root.join("home");
    let cwd = root.join("workspace");
    tokio::fs::create_dir_all(&home).await?;
    tokio::fs::create_dir_all(&cwd).await?;

    let mut cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: "https://example.invalid/v1".into(),
        max_context_tokens: 1_000_000,
        context_compact_pct: 100,
        sub_agent_timeout_secs,
        output_format: OutputFormat::Human,
        log_events: true,
        ..Default::default()
    };
    cfg.prompt.clear();
    configure(&mut cfg);
    let api_url = crate::config::api_url(&cfg);

    // Single assembly authority: test contexts go through the same production
    // path as real runtimes. Tests supply only config, temp dirs and doubles.
    let display = Arc::new(NoopDisplay::new());
    let llm_backend = llm_backend.unwrap_or_else(|| {
        Arc::new(crate::llm::client::OpenAiCompatibleBackend::deepseek_defaults())
    });
    let build = crate::runtime::context_build::build_agent_context(
        crate::runtime::context_build::AgentContextBuild {
            config: cfg,
            home: home.clone(),
            cwd: cwd.clone(),
            session_id: "regression".into(),
            session_layout: paths::SessionLayout::ProjectScoped,
            resolved_paths: None,
            api_url,
            display: display.clone(),
            sub_stream_tx: None,
            cancel: crate::cancel::CancellationToken::new(),
            interrupt: Arc::new(AtomicBool::new(false)),
            is_sub_agent,
            usage_journal: None,
            read_only_fs: None,
            resource_session_id: "regression".into(),
            resource_handlers: Vec::new(),
            skill_providers: Vec::new(),
            runtime_skills: Vec::new(),
            skill_discovery_policy: crate::capabilities::SkillDiscoveryPolicy::Defaults,
            llm_backend,
            resource_router: None,
            capability_snapshot: None,
            custom_tools: Vec::new(),
            prefix_source: None,
        },
    );
    let built = match event_log_writer {
        Some(writer) => {
            crate::runtime::context_build::TEST_EVENT_LOG_WRITER
                .scope(Some(writer), build)
                .await?
        }
        None => build.await?,
    };
    Ok(TestHarness {
        ctx: built.ctx,
        cwd,
        display,
    })
}

fn tool_call(name: &str, id: &str, input: serde_json::Value) -> ToolCallEvent {
    let fields = input
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| {
                    let value = v
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| v.to_string());
                    (k.clone(), value)
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    ToolCallEvent {
        name: name.into(),
        id: id.into(),
        input_json: input,
        fields,
        parse_error: None,
    }
}

async fn run_orchestrator_user_input(
    ctx: Arc<AgentSharedContext>,
    input: &str,
) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let actor = OrchActor::new(ctx, rx);
    let handle = tokio::spawn(actor.run());
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let turn_id = crate::runtime::TurnId::new("test-turn");
    let emitter = std::sync::Arc::new(crate::runtime::TurnEventEmitter::new(
        turn_id.clone(),
        None,
        None,
    ));
    tx.send(OrchCmd::UserInput {
        input: input.to_string(),
        turn_id,
        emitter,
        done: done_tx,
    })?;
    done_rx.await?;
    drop(tx);
    handle.await??;
    Ok(())
}

/// Parse typed `signal` events from a flushed events.jsonl snapshot.
pub(crate) fn parsed_signal_events(events_jsonl: &str) -> Vec<serde_json::Value> {
    events_jsonl
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value.get("type").and_then(serde_json::Value::as_str) == Some("signal"))
        .collect()
}

fn internal_result(name: &str) -> ToolExecution {
    ToolExecution {
        tool_use_id: format!("call_{name}"),
        tool_name: name.into(),
        tool_args: BTreeMap::new(),
        content: String::new(),
        conv_content: String::new(),
        spawns_sub_agent: false,
        sub_agent_prompt: None,
        sub_agent_fork: false,
        exit_code: None,
        status: crate::tools::metadata::ToolStatus::Succeeded,
        result_kind: crate::tools::metadata::ToolResultKind::Control,
        presentation: None,
        artifacts: Vec::new(),
        signals: Vec::new(),
        plan_command: None,
        needs_finalization: false,
        state_metadata: None,
        image_attachment: None,
    }
}

#[path = "regression/fixture.rs"]
mod fixture;
#[path = "regression/orchestrator_session.rs"]
mod orchestrator_session;
#[path = "regression/signal.rs"]
mod signal;
#[path = "regression/sub_agent.rs"]
mod sub_agent;
#[path = "regression/tool_boundary.rs"]
mod tool_boundary;
#[path = "regression/tool_file.rs"]
mod tool_file;
#[path = "regression/turn.rs"]
mod turn;

#[tokio::test]
async fn harness_backend_is_used_by_the_compaction_engine() -> anyhow::Result<()> {
    let summary = "Task focus: backend
Latest request: reuse
Progress: none\nTool evidence: none\nReflections: none";
    let llm = Arc::new(MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Text(TextEvent {
                content: summary.into(),
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]],
    ));
    let h = harness_with_config(
        "backend-reaches-compaction",
        false,
        300,
        |config| {
            config.max_context_tokens = 64_000;
            config.context_reserve_tokens = 12_000;
            config.context_compact_tail_tokens = 16_000;
            config.context_compact_max_output_tokens = 2_048;
        },
        Some(llm.clone()),
    )
    .await?;
    for index in 0..4 {
        h.ctx
            .store
            .add_user(&format!("request {index}: {}", "x".repeat(8_000)))
            .await?;
        h.ctx
            .store
            .add_assistant(&format!("progress {index}: {}", "y".repeat(8_000)), "", &[])
            .await?;
    }

    let resolved = crate::config::model_resolver(&h.ctx.config).resolve(&h.ctx.config.model);
    let (compacted, _) = h
        .ctx
        .compaction
        .evaluate_and_compact(
            "manual",
            50_000,
            crate::llm::client::LlmModelTarget::new(&resolved.actual, resolved.alias.as_deref()),
        )
        .await?;

    assert!(
        compacted,
        "compaction through the harness must use the injected backend"
    );
    Ok(())
}
