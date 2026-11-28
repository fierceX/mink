use super::*;
use crate::capabilities::skills::{LoadContext, LoadedSkill, SkillCapability, SkillProvider};
use crate::capabilities::{CapabilityExposure, SourceLevel, SourceMeta};
use crate::config::ResolvedConfig as Config;
use crate::resources::{
    ResourceHandler,
    router::{Resource, ResourceRequest},
};
use crate::runtime::{
    AgentEvent, AgentOptions, AgentTool, EventSink, SkillDiscoveryPolicy, ToolDefinition,
    ToolError, ToolExecutionContext, ToolExecutionMode, ToolOutput,
};
use crate::runtime::{runtime_skills_from_sdk_request, skill_discovery_policy_from_sdk_request};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// Wall-clock nanoseconds alone are not unique: the system clock is coarse
// enough that parallel tests read identical values and then share one session
// directory (which also shares its advisory lock). Pair it with a monotonic
// per-process sequence.
static TEMP_DIR_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn unique_temp_dir(name: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let sequence = TEMP_DIR_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mink-runtime-{name}-{}-{nanos}-{sequence}",
        std::process::id()
    ))
}

fn read_skill_resource(url: &str, ctx: &crate::context::ToolContext) -> anyhow::Result<String> {
    let selection = crate::resources::selector::split_read_path_selection(url)?;
    ctx.resource_router
        .resolve(&selection, ctx)
        .map(|resource| resource.content)
}

async fn write_session_metadata(dir: &std::path::Path, id: &str, cwd: &std::path::Path) {
    tokio::fs::create_dir_all(dir).await.unwrap();
    let metadata = crate::session::metadata::SessionMetadata {
        id: id.into(),
        alias: Some("legacy-alias".into()),
        title: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        cwd: cwd.display().to_string(),
        parent: None,
        first_prompt: None,
        untitled: false,
        summary: None,
        subagent: None,
    };
    tokio::fs::write(
        dir.join("session.json"),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .await
    .unwrap();
}

struct RuntimeTestSkillProvider;

impl SkillProvider for RuntimeTestSkillProvider {
    fn id(&self) -> &'static str {
        "runtime-test-skills"
    }

    fn display_name(&self) -> &'static str {
        "runtime test skills"
    }

    fn priority(&self) -> i32 {
        250
    }

    fn load_skills(&self, _ctx: &LoadContext<'_>) -> Result<Vec<LoadedSkill>> {
        Ok(vec![LoadedSkill {
            skill: SkillCapability {
                name: "provider-guide".to_string(),
                description: "Provider guide".to_string(),
                content: "provider body".to_string(),
                base_dir: "<provider>".to_string(),
                disable_model_invocation: false,
            },
            source: SourceMeta {
                provider_id: self.id().to_string(),
                provider_name: self.display_name().to_string(),
                level: SourceLevel::Runtime,
                source_path: None,
                display_label: Some("provider".to_string()),
            },
            exposure: CapabilityExposure::ModelDiscoverable,
            revision: "provider-rev-1".to_string(),
        }])
    }
}

struct KbResourceHandler;

impl ResourceHandler for KbResourceHandler {
    fn scheme(&self) -> &'static str {
        "kb"
    }

    fn resolve(
        &self,
        req: &ResourceRequest,
        _ctx: &crate::context::ToolContext,
    ) -> Result<Resource> {
        Ok(Resource {
            canonical_url: req.resource_url.clone(),
            content: format!("kb authority={} path={}", req.authority, req.path),
        })
    }
}

#[derive(Default)]
struct RecordingSink {
    pub(super) events: Mutex<Vec<AgentEvent>>,
}

#[async_trait::async_trait]
impl EventSink for RecordingSink {
    async fn on_event(&self, event: AgentEvent) -> Result<(), String> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

fn tool_call_event(
    name: &str,
    id: &str,
    input: serde_json::Value,
) -> crate::protocol::ToolCallEvent {
    let fields: std::collections::BTreeMap<String, String> = input
        .as_object()
        .map(|obj| {
            obj.iter()
                .map(|(key, value)| (key.clone(), value.to_string()))
                .collect()
        })
        .unwrap_or_default();
    crate::protocol::ToolCallEvent {
        name: name.into(),
        id: id.into(),
        input_json: input,
        fields,
        parse_error: None,
        raw_arguments_digest: None,
    }
}

// ── Mock LLM runtime integration tests ──────────────────────────

/// Build a minimal mock LLM that returns Text + Stop for each call.
fn mock_llm_hello() -> crate::llm::mock::MockLlmBackend {
    use crate::protocol::{Event, StopEvent, TextEvent};
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::Text(TextEvent {
                    content: "Hello, world!".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "second".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "third".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    )
}

fn mock_llm_with_usage() -> crate::llm::mock::MockLlmBackend {
    use crate::protocol::{Event, StopEvent, TextEvent, UsageEvent};
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![vec![
            Ok(Event::Text(TextEvent {
                content: "measured".into(),
            })),
            Ok(Event::Usage(UsageEvent {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_input_tokens: 40,
                cache_creation_input_tokens: 0,
            })),
            Ok(Event::Stop(StopEvent {
                reason: "end_turn".into(),
            })),
        ]],
    )
}

fn runtime_config_with_mock(
    home: &std::path::Path,
    cwd: &std::path::Path,
    mock: crate::llm::mock::MockLlmBackend,
) -> AgentRuntimeConfig {
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: "https://example.invalid/v1".into(),
        max_context_tokens: 1_000_000,
        log_events: true,
        ..Config::default()
    };
    let mut rt_config = AgentRuntimeConfig::from_config(cfg, home.to_path_buf(), cwd.to_path_buf());
    rt_config.llm_backend = Some(Arc::new(mock));
    rt_config
}

type SeenModels = Arc<Mutex<Vec<(String, Option<String>)>>>;

struct RecordingBackend {
    pub(super) seen: SeenModels,
}

#[async_trait::async_trait]
impl crate::runtime::LlmBackend for RecordingBackend {
    fn name(&self) -> &str {
        "recording"
    }

    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        self.seen
            .lock()
            .unwrap()
            .push((request.model, request.model_alias));
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(crate::runtime::LlmEvent::Text(
                    crate::runtime::LlmTextEvent {
                        content: "ok".into(),
                    },
                )),
                Ok(crate::runtime::LlmEvent::Stop(
                    crate::runtime::LlmStopEvent {
                        reason: "end_turn".into(),
                    },
                )),
            ])),
            attempt_count: 1,
        })
    }
}

type SeenMessages = Arc<Mutex<Vec<Vec<serde_json::Value>>>>;

struct MessageCaptureBackend {
    pub(super) seen: SeenMessages,
}

#[async_trait::async_trait]
impl crate::runtime::LlmBackend for MessageCaptureBackend {
    fn name(&self) -> &str {
        "message-capture"
    }

    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        self.seen.lock().unwrap().push((*request.messages).clone());
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(crate::runtime::LlmEvent::Text(
                    crate::runtime::LlmTextEvent {
                        content: "ok".into(),
                    },
                )),
                Ok(crate::runtime::LlmEvent::Stop(
                    crate::runtime::LlmStopEvent {
                        reason: "end_turn".into(),
                    },
                )),
            ])),
            attempt_count: 1,
        })
    }
}

// ── Interrupt test with blocking mock LLM ──────────────────────

/// A mock LLM used to test interrupt: while `block` is set, every `stream()`
/// call returns a never-yielding stream; once released, calls return a normal
/// Text+Stop. The explicit release flag replaces the old call-count ordering,
/// which depended on whether an already-interrupted turn still opened its
/// first request.
struct InterruptTestMockLlmBackend {
    pub(super) block: Arc<std::sync::atomic::AtomicBool>,
    /// Signalled when the turn actually reaches the LLM stream call.
    pub(super) entered: Arc<tokio::sync::Notify>,
}

struct InterruptibleCompactionBackend;

/// 摘要请求始终返回不可用摘要（空正文 + stop），主请求正常。
struct FailingSummaryBackend;

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for FailingSummaryBackend {
    fn name(&self) -> &str {
        "failing-summary"
    }

    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        use crate::protocol::{Event, StopEvent, TextEvent};
        if matches!(request.purpose, crate::runtime::LlmPurpose::Compaction) {
            return Ok(crate::runtime::LlmResponseStream {
                events: Box::pin(futures::stream::iter(vec![Ok(Event::Stop(StopEvent {
                    reason: "stop".into(),
                }))])),
                attempt_count: 1,
            });
        }
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(Event::Text(TextEvent {
                    content: "done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for InterruptibleCompactionBackend {
    fn name(&self) -> &str {
        "interruptible-compaction"
    }

    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        if matches!(request.purpose, crate::runtime::LlmPurpose::Compaction) {
            return Ok(crate::runtime::LlmResponseStream {
                events: Box::pin(futures::stream::pending()),
                attempt_count: 1,
            });
        }
        use crate::protocol::{Event, StopEvent, TextEvent};
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(Event::Text(TextEvent {
                    content: "after compact interrupt".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for InterruptTestMockLlmBackend {
    fn name(&self) -> &str {
        "interrupt-test"
    }
    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        self.entered.notify_one();
        if self.block.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(crate::runtime::LlmResponseStream {
                events: Box::pin(futures::stream::pending()),
                attempt_count: 1,
            });
        }
        use crate::protocol::{Event, StopEvent, TextEvent};
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(Event::Text(TextEvent {
                    content: "recovered".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

fn runtime_config_with_blocking_mock(
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> (
    AgentRuntimeConfig,
    Arc<tokio::sync::Notify>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: "https://example.invalid/v1".into(),
        max_context_tokens: 1_000_000,
        log_events: true,
        ..Config::default()
    };
    let mut rt_config = AgentRuntimeConfig::from_config(cfg, home.to_path_buf(), cwd.to_path_buf());
    let entered = Arc::new(tokio::sync::Notify::new());
    let block = Arc::new(std::sync::atomic::AtomicBool::new(true));
    rt_config.llm_backend = Some(Arc::new(InterruptTestMockLlmBackend {
        block: block.clone(),
        entered: entered.clone(),
    }));
    (rt_config, entered, block)
}

/// First `stream()` call never returns: exercises interrupt during request
/// establishment. Later calls return a normal stream so recovery can be
/// asserted.
#[derive(Default)]
struct PendingEstablishBackend {
    pub(super) calls: std::sync::Mutex<u32>,
    /// Signalled when `stream()` is actually entered.
    pub(super) entered: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for PendingEstablishBackend {
    fn name(&self) -> &str {
        "pending-establish"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        use crate::protocol::{Event, StopEvent, TextEvent};
        self.entered.notify_one();
        let call = {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        if call == 1 {
            futures::future::pending::<()>().await;
        }
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(Event::Text(TextEvent {
                    content: "after establishment interrupt".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

/// Consumes most of the first-event budget before returning a stream that
/// never yields an event.
struct SlowEstablishBackend;

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for SlowEstablishBackend {
    fn name(&self) -> &str {
        "slow-establish"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        tokio::time::sleep(tokio::time::Duration::from_millis(700)).await;
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::pending()),
            attempt_count: 1,
        })
    }
}

fn runtime_config_with_backend(
    home: &std::path::Path,
    cwd: &std::path::Path,
    backend: Arc<dyn crate::llm::client::LlmBackend>,
    mutate: impl FnOnce(&mut Config),
) -> AgentRuntimeConfig {
    let mut cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        base_url: "https://example.invalid/v1".into(),
        max_context_tokens: 1_000_000,
        log_events: true,
        ..Config::default()
    };
    mutate(&mut cfg);
    AgentRuntimeConfig::from_config(cfg, home.to_path_buf(), cwd.to_path_buf())
        .with_llm_backend(backend)
}

/// Runtime whose compaction request never returns until interrupted, with
/// enough history for a manual compaction to actually run.
async fn seeded_compaction_runtime(
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> crate::runtime::AgentRuntime {
    let cfg = Config {
        model: "flash".into(),
        api_key: "test-key".into(),
        max_context_tokens: 64_000,
        context_reserve_tokens: 8_000,
        context_compact_tail_tokens: 1,
        ..Config::default()
    };
    let runtime = build_runtime(
        AgentRuntimeConfig::from_config(cfg, home.to_path_buf(), cwd.to_path_buf())
            .with_llm_backend(Arc::new(InterruptibleCompactionBackend)),
    )
    .await
    .unwrap();
    for index in 0..3 {
        runtime
            .ctx
            .stores
            .store
            .add_user(&format!("request {index}: {}", "x".repeat(2_000)))
            .await
            .unwrap();
        runtime
            .ctx
            .stores
            .store
            .add_assistant(&format!("progress {index}: {}", "y".repeat(2_000)), "", &[])
            .await
            .unwrap();
    }
    runtime
}

// ── Mock LLM with tool-use test ───────────────────────────────

/// Mock LLM that exercises the full tool-execution pipeline:
/// first turn returns a Bash tool call, second turn returns Text+Stop.
fn mock_llm_tool_use() -> crate::llm::mock::MockLlmBackend {
    use crate::protocol::{Event, StopEvent, TextEvent, ToolCallEvent};
    use serde_json::json;
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("command".into(), "echo hello".into());
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            // First LLM call: request a Bash tool execution
            vec![
                Ok(Event::ToolCall(ToolCallEvent {
                    name: "Bash".into(),
                    id: "call_bash_1".into(),
                    input_json: json!({"command": "echo hello"}),
                    fields,
                    parse_error: None,
                    raw_arguments_digest: None,
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            // Second LLM call: text response after tool execution
            vec![
                Ok(Event::Text(TextEvent {
                    content: "all done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    )
}

struct AsyncEchoTool;

#[async_trait::async_trait]
impl AgentTool for AsyncEchoTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = ToolDefinition::new(
            "AsyncEcho",
            "Echo text asynchronously",
            serde_json::json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false
            }),
        );
        definition.execution = ToolExecutionMode::ParallelReadOnly;
        definition
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        if ctx.is_cancelled() {
            return Err(ToolError::new("cancelled"));
        }
        Ok(ToolOutput::text(format!(
            "echo:{}@{}",
            input["text"].as_str().unwrap_or_default(),
            ctx.cwd().display()
        )))
    }
}

struct MutatingEchoTool;

#[async_trait::async_trait]
impl AgentTool for MutatingEchoTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = ToolDefinition::new(
            "MutatingEcho",
            "A mutating test tool",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        );
        definition.execution = ToolExecutionMode::Sequential;
        definition.mutating = true;
        definition
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("mutated"))
    }
}

struct CancellableTool;

struct NonCooperativeTool;

#[async_trait::async_trait]
impl AgentTool for NonCooperativeTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = ToolDefinition::new(
            "NonCooperative",
            "Never completes or checks cancellation",
            serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        );
        definition.execution = ToolExecutionMode::Sequential;
        definition
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        futures::future::pending().await
    }
}

#[async_trait::async_trait]
impl AgentTool for CancellableTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = ToolDefinition::new(
            "Cancellable",
            "Wait until cancelled",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        );
        definition.execution = ToolExecutionMode::Sequential;
        definition
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        while !ctx.is_cancelled() {
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }
        Err(ToolError::new("cancelled"))
    }
}

struct DriftingDefinitionTool {
    pub(super) calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl AgentTool for DriftingDefinitionTool {
    fn definition(&self) -> ToolDefinition {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ToolDefinition::new(
            if call == 0 {
                "FrozenTool"
            } else {
                "DriftedTool"
            },
            "Definition must be frozen at startup",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        )
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("frozen"))
    }
}

fn mock_llm_single_tool(name: &str) -> crate::llm::mock::MockLlmBackend {
    use crate::protocol::{Event, StopEvent, TextEvent, ToolCallEvent};
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::ToolCall(ToolCallEvent {
                    name: name.into(),
                    id: format!("call_{}", name.to_ascii_lowercase()),
                    input_json: serde_json::json!({}),
                    fields: Default::default(),
                    parse_error: None,
                    raw_arguments_digest: None,
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    )
}

fn mock_llm_custom_tool_use() -> crate::llm::mock::MockLlmBackend {
    use crate::protocol::{Event, StopEvent, TextEvent, ToolCallEvent};
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("text".into(), "hello".into());
    crate::llm::mock::MockLlmBackend::new(
        "flash",
        vec![
            vec![
                Ok(Event::ToolCall(ToolCallEvent {
                    name: "AsyncEcho".into(),
                    id: "call_echo_1".into(),
                    input_json: serde_json::json!({"text": "hello"}),
                    fields,
                    parse_error: None,
                    raw_arguments_digest: None,
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "tool_use".into(),
                })),
            ],
            vec![
                Ok(Event::Text(TextEvent {
                    content: "custom done".into(),
                })),
                Ok(Event::Stop(StopEvent {
                    reason: "end_turn".into(),
                })),
            ],
        ],
    )
}

type SeenSystemPrompts = Arc<Mutex<Vec<String>>>;

struct SystemPromptCaptureBackend {
    pub(super) seen: SeenSystemPrompts,
}

#[async_trait::async_trait]
impl crate::runtime::LlmBackend for SystemPromptCaptureBackend {
    fn name(&self) -> &str {
        "system-prompt-capture"
    }

    async fn stream(
        &self,
        request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        self.seen
            .lock()
            .unwrap()
            .push(request.system_prompt.to_string());
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(crate::runtime::LlmEvent::Text(
                    crate::runtime::LlmTextEvent {
                        content: "ok".into(),
                    },
                )),
                Ok(crate::runtime::LlmEvent::Stop(
                    crate::runtime::LlmStopEvent {
                        reason: "end_turn".into(),
                    },
                )),
            ])),
            attempt_count: 1,
        })
    }
}

struct StaticPrefixSource {
    pub(super) system_prompt: String,
}

impl crate::runtime::PrefixSource for StaticPrefixSource {
    fn prefix(&self, _events_path: &std::path::Path) -> Option<(String, Vec<serde_json::Value>)> {
        Some((self.system_prompt.clone(), Vec::new()))
    }
}

type SeenHookViews = Arc<Mutex<Option<(String, usize, usize, String, String, String, String)>>>;

struct RecordingPostInitHook {
    pub(super) seen: SeenHookViews,
}

impl crate::runtime::PostInitHook for RecordingPostInitHook {
    fn run(&self, ctx: &crate::runtime::PostInitContext<'_>) -> anyhow::Result<()> {
        *self.seen.lock().unwrap() = Some((
            ctx.system_prompt().to_string(),
            ctx.tools().len(),
            ctx.workflow_ids().len(),
            ctx.workflow_fingerprint().to_string(),
            ctx.tool_surface_fingerprint().to_string(),
            ctx.tool_capabilities_fingerprint().to_string(),
            ctx.dependency_fingerprint().to_string(),
        ));
        assert!(!ctx.cwd().as_os_str().is_empty());
        assert!(!ctx.capabilities().skills.discoverable.is_empty());
        ctx.log_event(serde_json::json!({ "type": "hook_test_event" }))?;
        Ok(())
    }
}

struct RewritingHook;

impl crate::runtime::PostInitHook for RewritingHook {
    fn run(&self, ctx: &crate::runtime::PostInitContext<'_>) -> anyhow::Result<()> {
        // Simulate a host that rewrites session files before the first turn
        // (session-restructuring hook): append a user line to the
        // conversation and record a prefix_snapshot event.
        let mut conversation =
            std::fs::read_to_string(&ctx.session_paths().conversation).unwrap_or_default();
        conversation.push_str("{\"role\":\"user\",\"content\":\"HOOK-INJECTED-LINE\"}\n");
        std::fs::write(&ctx.session_paths().conversation, conversation)?;
        ctx.log_event(serde_json::json!({
            "type": "prefix_snapshot",
            "version": 1,
            "fingerprint": "hook-test",
            "dependency_fingerprint": "hook-test",
            "system_prompt": "HOOK-PROMPT",
            "tools_json": [],
        }))?;
        Ok(())
    }
}

/// Latched sessions must be refused before any model request.
struct PanicIfCalledBackend;

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for PanicIfCalledBackend {
    fn name(&self) -> &str {
        "panic-if-called"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        panic!("backend must not be called while the session is latched");
    }
}

/// Emits `retries` control notifications and then stays pending forever.
struct RetryThenPendingBackend {
    pub(super) retries: usize,
}

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for RetryThenPendingBackend {
    fn name(&self) -> &str {
        "retry-then-pending"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        use crate::protocol::{Event, RetryEvent};
        use futures::StreamExt as _;
        let retries =
            futures::stream::iter((0..self.retries).map(|_| Ok(Event::Retry(RetryEvent {}))));
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(retries.chain(futures::stream::pending())),
            attempt_count: 1,
        })
    }
}

/// Emits `chunks` text deltas of `chunk_bytes` each (bounded-stream tests).
struct TextStormBackend {
    pub(super) chunks: usize,
    pub(super) chunk_bytes: usize,
    pub(super) emitted: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) finished: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for TextStormBackend {
    fn name(&self) -> &str {
        "text-storm"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        use crate::protocol::{Event, StopEvent, TextEvent};
        use futures::StreamExt as _;
        let emitted = self.emitted.clone();
        let finished = self.finished.clone();
        let total = self.chunks;
        let chunk = "x".repeat(self.chunk_bytes);
        let storm = futures::stream::iter(0..self.chunks).map(move |_| {
            if emitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == total {
                finished.notify_waiters();
            }
            Ok(Event::Text(TextEvent {
                content: chunk.clone(),
            }))
        });
        let stop = futures::stream::iter(vec![Ok(Event::Stop(StopEvent {
            reason: "end_turn".into(),
        }))]);
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(storm.chain(stop)),
            attempt_count: 1,
        })
    }
}

/// Emits a Retry control notification every 5ms forever: the event cadence is
/// faster than the consumer's 25ms tick, which used to starve deadline checks.
struct HighFrequencyRetryBackend;

#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for HighFrequencyRetryBackend {
    fn name(&self) -> &str {
        "high-frequency-retry"
    }

    async fn stream(
        &self,
        _request: crate::runtime::LlmRequest,
    ) -> anyhow::Result<crate::runtime::LlmResponseStream> {
        use crate::protocol::{Event, RetryEvent};
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<anyhow::Result<Event>>();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(5));
            loop {
                interval.tick().await;
                if tx.send(Ok(Event::Retry(RetryEvent {}))).is_err() {
                    break;
                }
            }
        });
        let events = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Ok(crate::runtime::LlmResponseStream {
            events: Box::pin(events),
            attempt_count: 1,
        })
    }
}

/// Custom tool that panics inside its spawned task: this is the deterministic
/// infrastructure failure for the partial-results contract.
struct PanicTool(tokio::sync::Notify);

#[async_trait::async_trait]
impl AgentTool for PanicTool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = ToolDefinition::new(
            "PanicTool",
            "Always panics (test fixture)",
            serde_json::json!({
                "type": "object",
                "properties": {"release":{"type":"boolean"},"mark":{"type":"boolean"},"stall":{"type":"boolean"}},
                "additionalProperties": false
            }),
        );
        definition.execution = ToolExecutionMode::ParallelReadOnly;
        definition
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        if input["release"] == true {
            self.0.notify_one();
            return Ok(ToolOutput::text("later sibling finished"));
        }
        if input["mark"] == true {
            std::fs::write(ctx.cwd().join("attempted"), "once").unwrap();
            if input["stall"] == true {
                return futures::future::pending().await;
            }
        } else {
            self.0.notified().await;
        }
        panic!("panic tool fixture");
    }
}

struct GuidanceBoundaryBackend {
    pub(super) entered: Arc<tokio::sync::Notify>,
    pub(super) release: Arc<tokio::sync::Notify>,
    pub(super) requests: Arc<Mutex<Vec<Vec<serde_json::Value>>>>,
}
#[async_trait::async_trait]
impl crate::llm::client::LlmBackend for GuidanceBoundaryBackend {
    fn name(&self) -> &str {
        "guidance-boundary-test"
    }
    async fn stream(
        &self,
        request: crate::llm::client::LlmRequest,
    ) -> anyhow::Result<crate::llm::client::LlmResponseStream> {
        let first = {
            let mut requests = self.requests.lock().unwrap();
            requests.push((*request.messages).clone());
            requests.len() == 1
        };
        if first {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(crate::llm::client::LlmResponseStream {
            attempt_count: 1,
            events: Box::pin(futures::stream::iter(vec![
                Ok(crate::llm::client::LlmEvent::Text(
                    crate::llm::client::LlmTextEvent {
                        content: if first {
                            "first answer"
                        } else {
                            "guided answer"
                        }
                        .into(),
                    },
                )),
                Ok(crate::llm::client::LlmEvent::Stop(
                    crate::llm::client::LlmStopEvent {
                        reason: "end_turn".into(),
                    },
                )),
            ])),
        })
    }
}

struct BoundaryTool {
    pub(super) entered: Arc<tokio::sync::Notify>,
    pub(super) release: Arc<tokio::sync::Notify>,
}
#[async_trait::async_trait]
impl AgentTool for BoundaryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "BoundaryTool",
            "Wait for test release",
            serde_json::json!({"type":"object","additionalProperties":false}),
        )
    }
    async fn execute(
        &self,
        _: serde_json::Value,
        _: ToolExecutionContext,
    ) -> Result<ToolOutput, ToolError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ToolOutput::text("complete tool result"))
    }
}

mod lifecycle;
mod streaming;
mod tools_and_images;
