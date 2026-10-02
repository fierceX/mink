use crate::context::AgentSharedContext;
use crate::llm::recovery::{self, LlmUpstreamError, UpstreamFailureKind};
use crate::protocol::Event;
use crate::session::usage::{UsageCapture, UsageGuard, UsageKind};
use crate::sse::openai::OpenAIParser;
use anyhow::Result;
use futures::StreamExt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Bound on the provider error body kept for diagnostics.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct LlmModelTarget<'a> {
    pub model: &'a str,
    pub alias: Option<&'a str>,
}

impl<'a> LlmModelTarget<'a> {
    pub fn new(model: &'a str, alias: Option<&'a str>) -> Self {
        Self { model, alias }
    }
}

pub type LlmEvent = Event;
pub type LlmTextEvent = crate::protocol::TextEvent;
pub type LlmThinkingEvent = crate::protocol::ThinkingEvent;
pub type LlmToolCallEvent = crate::protocol::ToolCallEvent;
pub type LlmUsageEvent = crate::protocol::UsageEvent;
pub type LlmStopEvent = crate::protocol::StopEvent;
pub type LlmErrorEvent = crate::protocol::ErrorEvent;
pub type LlmRetryEvent = crate::protocol::RetryEvent;
pub type LlmCancelToken = crate::cancel::CancellationToken;
pub type LlmEventStream = Pin<Box<dyn futures::Stream<Item = Result<LlmEvent>> + Send>>;

pub struct LlmResponseStream {
    pub events: LlmEventStream,
    pub attempt_count: u32,
}

#[derive(Debug)]
pub struct LlmRequestFailure {
    pub attempt_count: u32,
    pub error: anyhow::Error,
}

impl std::fmt::Display for LlmRequestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for LlmRequestFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Expose the wrapped error itself so `root_cause()`/`chain()` from a
        // caller can still reach the structured upstream root (for example an
        // `LlmUpstreamError` returned by the built-in backend).
        Some(self.error.as_ref())
    }
}

impl LlmRequestFailure {
    /// Structured upstream classification carried by this failure, when the
    /// wrapped error (or its chain) provides one.
    pub fn upstream_kind(&self) -> Option<UpstreamFailureKind> {
        recovery::find_upstream_failure(&self.error).map(|error| error.kind())
    }
}

#[derive(Debug, Clone)]
pub enum LlmPurpose {
    Agent,
    SubAgent { session_id: String },
    Compaction,
}

/// Provider-visible logical request prefix used to preserve prompt-cache
/// alignment across internal requests such as compaction.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmCacheProjection {
    pub model: String,
    pub system_prompt: String,
    pub tools: Vec<serde_json::Value>,
    pub messages: Vec<serde_json::Value>,
}

/// One logical LLM request as built by the runtime.
///
/// Retries of one logical request reuse the *same* projection (including
/// materialized images); only the per-attempt cancel token changes. Cloning
/// this value is how each attempt gets its own request object.
#[derive(Clone)]
pub struct LlmRequest {
    pub purpose: LlmPurpose,
    pub model: String,
    pub model_alias: Option<String>,
    pub api_url: String,
    pub api_key: String,
    pub system_prompt: String,
    pub messages: Vec<serde_json::Value>,
    pub tools: Vec<serde_json::Value>,
    pub max_tokens: i32,
    pub cancel: LlmCancelToken,
    pub verbose: bool,
    pub display: Arc<dyn crate::ui::Display>,
}

// async_trait expands a bare `#[must_use]` into generated code; the lint
// cannot be satisfied there (macro output cannot carry a reason message).
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait LlmBackend: Send + Sync {
    fn name(&self) -> &str;

    /// Open one streaming request.
    ///
    /// Cancellation contract: implementations should observe `request.cancel`
    /// and stop any work they spawn themselves. The runtime additionally
    /// drops this future when a turn is interrupted while the response is
    /// not yet established, but dropping a future does not by itself
    /// guarantee that backend-spawned tasks, connections, or threads are
    /// cleaned up.
    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream>;

    /// Whether provider prompt usage can be adjusted by the source-request
    /// token delta. Backends which inject or remove provider-visible content
    /// dynamically must return false unless they provide an equivalent stable
    /// projection accounting model.
    fn prompt_usage_calibration_safe(&self) -> bool {
        true
    }

    /// Return the provider-visible prefix corresponding to the first
    /// `source_prefix_len` source messages. Backends which cannot prove that
    /// boundary mapping retain source compatibility through the default
    /// `None` implementation.
    fn cache_projection(
        &self,
        request: &LlmRequest,
        source_prefix_len: usize,
    ) -> Option<LlmCacheProjection> {
        let _ = (request, source_prefix_len);
        None
    }

    /// Declared image-input capability for one resolved model.
    ///
    /// The default is `Unsupported` (fail closed): custom backends that do
    /// not override this method keep their existing source compatibility and
    /// never receive image parts (v7 §3.1).
    fn image_input_capability(
        &self,
        model: &str,
    ) -> crate::capabilities::model_capabilities::ImageInputCapability {
        let _ = model;
        crate::capabilities::model_capabilities::ImageInputCapability::Unsupported
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenParamKind {
    MaxTokens,
    MaxCompletionTokens,
}

impl TokenParamKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "max_tokens" | "max-tokens" | "max_tokens_param" => Some(Self::MaxTokens),
            "max_completion_tokens" | "max-completion-tokens" => Some(Self::MaxCompletionTokens),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleOptions {
    pub send_reasoning_effort: bool,
    pub reasoning_effort: Option<String>,
    pub include_usage: bool,
    pub token_param: TokenParamKind,
    pub parallel_tool_calls: Option<bool>,
    /// provider HTTP 请求总超时（秒，`0` = 不设总超时）。默认 600 秒；长流式
    /// 生成（如文档生成）应调大或设 `0`，由首事件/空闲期限兜底。
    pub http_timeout_secs: u64,
}

impl Default for OpenAiCompatibleOptions {
    fn default() -> Self {
        Self {
            send_reasoning_effort: true,
            reasoning_effort: Some("max".to_string()),
            include_usage: true,
            token_param: TokenParamKind::MaxTokens,
            parallel_tool_calls: None,
            http_timeout_secs: crate::config::DEFAULT_PROVIDER_HTTP_TIMEOUT_SECS,
        }
    }
}

pub struct OpenAiCompatibleBackend {
    options: OpenAiCompatibleOptions,
    tool_choice: Option<serde_json::Value>,
    extra_body: std::collections::BTreeMap<String, serde_json::Value>,
    client: Mutex<Option<reqwest::Client>>,
    /// Resolved model ids that declare image input. Defaults to the built-in
    /// vision catalog entry; an explicit `ResolvedConfig::image_input` still
    /// wins over this declaration (v7 §3.1 priority).
    vision_models: Vec<String>,
}

impl OpenAiCompatibleBackend {
    pub fn new(options: OpenAiCompatibleOptions) -> Self {
        Self {
            options,
            tool_choice: None,
            extra_body: std::collections::BTreeMap::new(),
            client: Mutex::new(None),
            vision_models: Vec::new(),
        }
    }

    /// Restrict the backend-declared image capability to these model ids.
    pub fn with_vision_models(
        mut self,
        models: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.vision_models = models.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_tool_choice(mut self, tool_choice: impl Into<serde_json::Value>) -> Self {
        self.tool_choice = Some(tool_choice.into());
        self
    }

    pub fn with_extra_body(
        mut self,
        extra_body: std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Self {
        self.extra_body = extra_body;
        self
    }

    /// Override the provider HTTP request timeout in seconds (`0` disables the
    /// total timeout; long streaming generations then rely on the first-event /
    /// idle deadlines only). Drops any cached HTTP client so the new value
    /// applies to subsequent requests even after the backend was used once.
    pub fn with_http_timeout_secs(mut self, seconds: u64) -> Self {
        self.options.http_timeout_secs = seconds;
        self.client = Mutex::new(None);
        self
    }

    fn http_client(&self) -> Result<reqwest::Client> {
        let mut slot = self
            .client
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(client) = slot.as_ref() {
            return Ok(client.clone());
        }
        let client = build_http_client(self.options.http_timeout_secs)?;
        *slot = Some(client.clone());
        Ok(client)
    }

    pub fn from_config(config: &crate::config::ResolvedConfig) -> Self {
        let reasoning_effort = config
            .openai_reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|value| {
                !value.is_empty()
                    && !matches!(
                        value.to_ascii_lowercase().as_str(),
                        "off" | "none" | "false" | "disabled"
                    )
            })
            .map(str::to_string);
        Self::new(OpenAiCompatibleOptions {
            send_reasoning_effort: reasoning_effort.is_some(),
            reasoning_effort,
            include_usage: config.openai_include_usage,
            token_param: config.openai_token_param,
            parallel_tool_calls: None,
            http_timeout_secs: config.provider_http_timeout_secs,
        })
        .with_extra_body(config.openai_extra_body.clone())
        .with_optional_tool_choice(config.openai_tool_choice.clone())
        .with_vision_models(config.vision_models.iter().cloned())
    }

    #[cfg(test)]
    pub fn deepseek_defaults() -> Self {
        Self::new(OpenAiCompatibleOptions::default())
    }

    fn with_optional_tool_choice(mut self, tool_choice: Option<serde_json::Value>) -> Self {
        self.tool_choice = tool_choice;
        self
    }
}

#[async_trait::async_trait]
impl LlmBackend for OpenAiCompatibleBackend {
    fn name(&self) -> &str {
        "openai-compatible"
    }

    fn image_input_capability(
        &self,
        model: &str,
    ) -> crate::capabilities::model_capabilities::ImageInputCapability {
        if self.vision_models.iter().any(|vision| vision == model) {
            return crate::capabilities::model_capabilities::ImageInputCapability::OpenAiChatImageUrl(
                crate::capabilities::model_capabilities::OpenAiChatImageUrlLimits::default(),
            );
        }
        crate::capabilities::model_capabilities::ImageInputCapability::Unsupported
    }

    fn cache_projection(
        &self,
        request: &LlmRequest,
        source_prefix_len: usize,
    ) -> Option<LlmCacheProjection> {
        if source_prefix_len > request.messages.len() {
            return None;
        }
        Some(LlmCacheProjection {
            model: request.model.clone(),
            system_prompt: request.system_prompt.clone(),
            tools: request.tools.clone(),
            messages: request.messages[..source_prefix_len].to_vec(),
        })
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmResponseStream> {
        let client = AsyncLlClient::from_client(
            self.http_client()?.clone(),
            &request.api_key,
            &request.api_url,
        );
        let body = crate::llm::transport::build_openai_body_with_options_and_extensions(
            &request.model,
            &request.messages,
            &request.tools,
            &request.system_prompt,
            request.max_tokens,
            &self.options,
            self.tool_choice.as_ref(),
            &self.extra_body,
        )?;
        // Final body cap (v7 §9.4): fail before any bytes hit the wire. The
        // cap applies only to multimodal requests — plain text requests keep
        // their pre-existing behavior (review fix).
        let has_materialized_images = request.messages.iter().any(|message| {
            message
                .get("content")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block.get("type").and_then(serde_json::Value::as_str) == Some("image_url")
                    })
                })
        });
        if has_materialized_images
            && body.len() as u64 > crate::capabilities::model_capabilities::MAX_REQUEST_BODY_BYTES
        {
            anyhow::bail!(
                "request body exceeds the {} byte limit ({} bytes); reduce image count or size and retry",
                crate::capabilities::model_capabilities::MAX_REQUEST_BODY_BYTES,
                body.len()
            );
        }

        if request.verbose {
            let preview = String::from_utf8_lossy(&body);
            let truncated: String = preview.chars().take(200).collect();
            request.display.render_info(&format!(
                "Request body ({}KB): {}...",
                body.len() / 1024,
                truncated
            ));
        }

        // One `stream()` call is exactly one physical request: request-level
        // retries are owned by the runtime attempt loop, so retry counts can
        // never multiply.
        let resp = client
            .send_once(body, &request.cancel)
            .await
            .map_err(anyhow::Error::new)?;
        let (tx, rx) = mpsc::channel(SSE_QUEUE_CAPACITY);
        let cancel = request.cancel.linked_child_token();
        AsyncLlClient::spawn_stream_task(resp, cancel.clone(), tx);
        Ok(LlmResponseStream {
            events: Box::pin(SseEventStream { rx, cancel }),
            attempt_count: 1,
        })
    }
}

pub struct AsyncLlClient {
    client: reqwest::Client,
    api_url: String,
    api_key: String,
}

/// Bounded producer→consumer queue for one SSE response.
///
/// The producer (`spawn_stream_task`) uses async sends: when the consumer is
/// slow the network read backpressures instead of accumulating unbounded
/// events. Dropping the consumer (cancel/abandon) makes sends fail and the
/// task exits.
const SSE_QUEUE_CAPACITY: usize = 1024;

/// Outcome of forwarding buffered SSE events.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SseSendStatus {
    Delivered,
    /// Cancelled: the caller must stop without waiting for capacity.
    Stopped,
}

fn build_http_client(http_timeout_secs: u64) -> reqwest::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .user_agent(concat!("mink/", env!("CARGO_PKG_VERSION")));
    // `0` disables the total request timeout: a long but healthy streaming
    // generation is then bounded only by the first-event / idle deadlines in
    // the runtime attempt loop instead of a fixed wall-clock cap.
    if http_timeout_secs > 0 {
        builder = builder.timeout(Duration::from_secs(http_timeout_secs));
    }
    builder.build()
}

impl AsyncLlClient {
    #[cfg(test)]
    pub fn new(api_key: &str, api_url: &str) -> Result<Self> {
        Ok(Self::from_client(
            build_http_client(crate::config::DEFAULT_PROVIDER_HTTP_TIMEOUT_SECS)?,
            api_key,
            api_url,
        ))
    }

    pub fn from_client(client: reqwest::Client, api_key: &str, api_url: &str) -> Self {
        Self {
            client,
            api_url: api_url.to_string(),
            api_key: api_key.to_string(),
        }
    }

    fn spawn_stream_task(
        resp: reqwest::Response,
        cancel: crate::cancel::CancellationToken,
        tx: mpsc::Sender<Result<Event>>,
    ) {
        tokio::spawn(async move {
            let mut byte_stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            let mut clean_end = false;
            let mut parser = OpenAIParser::new();
            let mut pending: Vec<Event> = Vec::new();

            'outer: loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        AsyncLlClient::try_send_interrupted(&tx);
                        clean_end = true;
                        break 'outer;
                    }
                    chunk = byte_stream.next() => {
                        match chunk {
                            Some(Ok(bytes)) => {
                                buf.extend_from_slice(&bytes);
                                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                                    let line = String::from_utf8_lossy(&buf[..=pos]).to_string();
                                    buf.drain(..=pos);
                                    let l = line.trim();
                                    if l.is_empty() { continue; }
                                    let full = format!("{l}\n");
                                    let parsed = parser.process_line(&full, &mut |e| {
                                        pending.push(e);
                                        Ok(())
                                    });
                                    if AsyncLlClient::send_events(&tx, &mut pending, &cancel).await == SseSendStatus::Stopped {
                                        clean_end = true;
                                        break 'outer;
                                    }
                                    match parsed {
                                        Ok(true) => {
                                            clean_end = true;
                                            break 'outer;
                                        }
                                        Ok(false) => {}
                                        // Frame-level damage: the whole attempt is
                                        // untrustworthy. Frames are never silently
                                        // skipped anymore; the runtime discards
                                        // the candidate and retries the request.
                                        Err(e) => {
                                            let failure = protocol_damage(e);
                                            let _ = AsyncLlClient::send_one(&tx, Err(anyhow::Error::new(failure)), &cancel).await;
                                            clean_end = true;
                                            break 'outer;
                                        }
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                let failure = transport_failure(e);
                                let _ = AsyncLlClient::send_one(&tx, Err(anyhow::Error::new(failure)), &cancel).await;
                                clean_end = true;
                                break 'outer;
                            }
                            None => {
                                // 传输 EOF：缓冲可能残留未换行的最后一行
                                //（连接在帧尾被切断但 JSON 完整）。先送入
                                // parser 再 finish_eof，否则末帧事件丢失。
                                if !buf.is_empty() {
                                    let line = String::from_utf8_lossy(&buf).to_string();
                                    let l = line.trim();
                                    if !l.is_empty() {
                                        let full = format!("{l}\n");
                                        let parsed = parser.process_line(&full, &mut |e| {
                                            pending.push(e);
                                            Ok(())
                                        });
                                        if AsyncLlClient::send_events(&tx, &mut pending, &cancel).await == SseSendStatus::Stopped {
                                            clean_end = true;
                                            break 'outer;
                                        }
                                        if let Err(e) = parsed {
                                            let failure = protocol_damage(e);
                                            let _ = AsyncLlClient::send_one(&tx, Err(anyhow::Error::new(failure)), &cancel).await;
                                            clean_end = true;
                                            break 'outer;
                                        }
                                    }
                                }
                                break 'outer;
                            }
                        }
                    }
                }
            }

            if !clean_end
                && let Err(e) = parser.finish_eof(&mut |e| {
                    pending.push(e);
                    Ok(())
                })
            {
                // An EOF without a terminal frame is response damage, not a
                // permanent request failure: the runtime may retry it.
                let failure = protocol_damage(e);
                let _ =
                    AsyncLlClient::send_one(&tx, Err(anyhow::Error::new(failure)), &cancel).await;
            }
            parser
                .flush(&mut |e| {
                    pending.push(e);
                    Ok(())
                })
                .ok();
            let _ = AsyncLlClient::send_events(&tx, &mut pending, &cancel).await;
        });
    }

    /// Forward buffered parser events; every blocking send also observes the
    /// cancel token, so a stalled (but alive) consumer cannot pin the task.
    async fn send_events(
        tx: &mpsc::Sender<Result<Event>>,
        events: &mut Vec<Event>,
        cancel: &crate::cancel::CancellationToken,
    ) -> SseSendStatus {
        for event in events.drain(..) {
            if !AsyncLlClient::send_one(tx, Ok(event), cancel).await {
                return SseSendStatus::Stopped;
            }
        }
        SseSendStatus::Delivered
    }

    /// Send one item while observing cancel; `false` means stop the task.
    async fn send_one(
        tx: &mpsc::Sender<Result<Event>>,
        item: Result<Event>,
        cancel: &crate::cancel::CancellationToken,
    ) -> bool {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            result = tx.send(item) => result.is_ok(),
        }
    }

    /// Best-effort interruption notice: never waits for capacity (the
    /// consumer may be stalled), so cancel handling cannot deadlock.
    fn try_send_interrupted(tx: &mpsc::Sender<Result<Event>>) {
        let _ = tx.try_send(Ok(Event::Stop(crate::protocol::StopEvent {
            reason: "interrupted".into(),
        })));
    }

    /// Establish exactly one physical request.
    ///
    /// Transient/provider/permanent classification is carried by the returned
    /// [`LlmUpstreamError`]; the runtime attempt loop owns retries, backoff and
    /// `Retry-After` waits.
    async fn send_once(
        &self,
        body: Vec<u8>,
        cancel: &crate::cancel::CancellationToken,
    ) -> std::result::Result<reqwest::Response, LlmUpstreamError> {
        if cancel.is_cancelled() {
            return Err(LlmUpstreamError::recoverable("request cancelled"));
        }
        let req = self
            .client
            .post(&self.api_url)
            .body(body)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", self.api_key));
        let resp = tokio::select! {
            result = req.send() => match result {
                Ok(resp) => resp,
                Err(error) => return Err(transport_failure(error)),
            },
            _ = cancel.cancelled() => {
                return Err(LlmUpstreamError::recoverable("request cancelled"));
            }
        };
        let status = resp.status().as_u16();
        if status < 400 {
            return Ok(resp);
        }
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(recovery::parse_retry_after);
        let body_text = read_error_body(resp, cancel).await;
        Err(recovery::http_failure(status, &body_text, retry_after))
    }
}

/// One round's fixed request projection.
///
/// Built once per round: every attempt clones the template with a fresh cancel
/// token, so images/attachments are materialized exactly once and the
/// immutable prefix never changes across retries.
pub(crate) struct PreparedLlmRequest {
    request: LlmRequest,
    local_tokens: usize,
    cache_projection: Option<LlmCacheProjection>,
    source_fingerprint: String,
    source_system_prompt: String,
    source_tools: Vec<serde_json::Value>,
    is_agent_request: bool,
    backend_name: String,
    model_name: String,
}

impl PreparedLlmRequest {
    pub(crate) fn request_for(&self, cancel: LlmCancelToken) -> LlmRequest {
        let mut request = self.request.clone();
        request.cancel = cancel;
        request
    }
}

/// Build one round's fixed projection (image materialization included).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare_llm_request(
    backend: &Arc<dyn LlmBackend>,
    ctx: &AgentSharedContext,
    model_name: &str,
    model_alias: Option<&str>,
    messages_json: &[serde_json::Value],
    tools_json: &[serde_json::Value],
    system_prompt: &str,
) -> Result<PreparedLlmRequest> {
    let purpose = if ctx.is_sub_agent {
        LlmPurpose::SubAgent {
            session_id: ctx.config.session_id.clone(),
        }
    } else {
        LlmPurpose::Agent
    };
    let is_agent_request = matches!(purpose, LlmPurpose::Agent);
    let local_tokens = crate::llm::transport::estimate_openai_context_tokens(
        messages_json,
        tools_json,
        system_prompt,
    )?;
    // Core Agent request projection (v7 §9.2): resolve `tool_attachment`
    // blocks into data-URL image parts against the home image cache.
    // Compaction bypasses this path: it may retain Agent tool schemas for
    // cache alignment, but persisted attachments are degraded to deterministic
    // text markers instead of materializing or resending image pixels.
    // Runs on the blocking pool: reads + base64 of multi-MB images must not
    // stall async workers.
    let mut messages = messages_json.to_vec();
    let this_turn_ids = ctx
        .this_turn_image_ids
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let cache = ctx.image_cache.clone();
    let display = ctx.display.clone();
    let limits = ctx.model_capabilities.image_input.limits().cloned();
    let warned = ctx.warned_image_ids.clone();
    let messages = tokio::task::spawn_blocking(move || {
        let mut warned = warned.lock().unwrap_or_else(|error| error.into_inner());
        crate::llm::image_projection::materialize_images_with(
            &mut messages,
            &cache,
            display.as_ref(),
            limits.as_ref(),
            &this_turn_ids,
            &mut warned,
        )?;
        Ok::<_, anyhow::Error>(messages)
    })
    .await??;
    let request = LlmRequest {
        purpose,
        model: model_name.to_string(),
        model_alias: model_alias.map(str::to_string),
        api_url: ctx.api_url.clone(),
        api_key: ctx.api_key().to_string(),
        system_prompt: system_prompt.to_string(),
        messages,
        tools: tools_json.to_vec(),
        max_tokens: crate::session::compaction::effective_max_tokens(&ctx.config),
        cancel: ctx.cancel.clone(),
        verbose: ctx.verbose(),
        display: ctx.display.clone(),
    };
    Ok(PreparedLlmRequest {
        cache_projection: backend.cache_projection(&request, request.messages.len()),
        source_fingerprint: crate::session::compaction::prefix_fingerprint(
            &request.system_prompt,
            &request.tools,
        ),
        source_system_prompt: request.system_prompt.clone(),
        source_tools: request.tools.clone(),
        backend_name: backend.name().to_string(),
        model_name: model_name.to_string(),
        local_tokens,
        is_agent_request,
        request,
    })
}

/// Open one attempt of a prepared logical request.
///
/// Each attempt settles its own usage capture (reported on success, unreported
/// on failure) and records the successful agent request for cache alignment.
pub(crate) async fn open_llm_stream(
    backend: &Arc<dyn LlmBackend>,
    ctx: &AgentSharedContext,
    prepared: &PreparedLlmRequest,
    cancel: LlmCancelToken,
) -> Result<LlmResponseStream> {
    let request = prepared.request_for(cancel);
    let capture = ctx.usage.capture(
        ctx.usage_scope(if ctx.is_sub_agent {
            UsageKind::SubAgent
        } else {
            UsageKind::Agent
        }),
        prepared.model_name.clone(),
    );
    let response = match guarded_open_stream(backend, request, capture).await {
        Ok(response) => response,
        Err(error) => return Err(error),
    };
    if prepared.is_agent_request {
        ctx.compaction.record_agent_request(
            &prepared.model_name,
            &prepared.source_fingerprint,
            prepared.local_tokens,
            prepared.backend_name.clone(),
            prepared.source_system_prompt.clone(),
            prepared.source_tools.clone(),
            prepared.cache_projection.clone(),
        );
    }
    Ok(response)
}

/// Open one backend request with exactly-once usage settlement: the capture is
/// reported by the returned metered stream on success and recorded as
/// unreported on failure.
pub(crate) async fn guarded_open_stream(
    backend: &Arc<dyn LlmBackend>,
    request: LlmRequest,
    capture: UsageCapture,
) -> Result<LlmResponseStream> {
    let mut usage_guard = UsageGuard::new(capture);
    match backend.stream(request).await {
        Ok(response) => {
            let capture = usage_guard
                .take()
                .expect("usage guard transferred exactly once");
            Ok(LlmResponseStream {
                events: Box::pin(MeteredStream::new(
                    response.events,
                    capture,
                    response.attempt_count,
                )),
                attempt_count: response.attempt_count,
            })
        }
        Err(error) => {
            let attempt_count = request_failure_attempt_count(&error);
            usage_guard.record_unreported(attempt_count, format!("request_failed: {error}"));
            Err(error)
        }
    }
}

/// One-shot open used by unit tests and simple embedders; production agent
/// turns use `prepare_llm_request` + `open_llm_stream` so retries keep the
/// same fixed projection.
#[cfg(test)]
pub(crate) async fn stream_backend(
    backend: &Arc<dyn LlmBackend>,
    ctx: &AgentSharedContext,
    model_name: &str,
    model_alias: Option<&str>,
    messages_json: &[serde_json::Value],
    tools_json: &[serde_json::Value],
    system_prompt: &str,
) -> Result<LlmEventStream> {
    let prepared = prepare_llm_request(
        backend,
        ctx,
        model_name,
        model_alias,
        messages_json,
        tools_json,
        system_prompt,
    )
    .await?;
    let stream = open_llm_stream(backend, ctx, &prepared, ctx.cancel.clone()).await?;
    Ok(stream.events)
}

/// Attempt-level response stream damage: the candidate is untrustworthy and
/// the runtime may retry the identical request while counting one format error.
fn protocol_damage(error: impl std::fmt::Display) -> LlmUpstreamError {
    LlmUpstreamError::protocol_damaged(format!("response stream damaged: {error}"))
}

/// Transport failure classification for one physical request.
fn transport_failure(error: reqwest::Error) -> LlmUpstreamError {
    let kind = recovery::classify_transport_error(&error);
    LlmUpstreamError::new(kind, format!("request failed: {error}"))
        .with_source(anyhow::Error::new(error))
}

/// Read a bounded, cancellable provider error body.
async fn read_error_body(
    resp: reqwest::Response,
    cancel: &crate::cancel::CancellationToken,
) -> String {
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    loop {
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = cancel.cancelled() => break,
        };
        let Some(chunk) = chunk else { break };
        match chunk {
            Ok(chunk) => {
                if bytes.len() >= MAX_ERROR_BODY_BYTES {
                    break;
                }
                let take = (MAX_ERROR_BODY_BYTES - bytes.len()).min(chunk.len());
                bytes.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

pub struct SseEventStream {
    rx: mpsc::Receiver<Result<Event>>,
    cancel: crate::cancel::CancellationToken,
}

pub(crate) struct MeteredStream<S> {
    inner: S,
    capture: UsageCapture,
    attempt_count: u32,
    completed: bool,
    /// Extra Usage events after the first are recorded once as unreported
    /// instead of being silently dropped.
    extra_usage_recorded: bool,
}

impl<S> MeteredStream<S> {
    pub(crate) fn new(inner: S, capture: UsageCapture, attempt_count: u32) -> Self {
        Self {
            inner,
            capture,
            attempt_count,
            completed: false,
            extra_usage_recorded: false,
        }
    }

    fn finish_unreported(&mut self, reason: impl Into<String>) {
        if self.completed {
            return;
        }
        self.completed = true;
        record_unreported(&self.capture, self.attempt_count, reason);
    }
}

impl<S> Drop for MeteredStream<S> {
    fn drop(&mut self) {
        self.finish_unreported("stream_dropped_before_usage");
    }
}

impl<S> futures::Stream for MeteredStream<S>
where
    S: futures::Stream<Item = Result<Event>> + Unpin,
{
    type Item = Result<Event>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(Event::Usage(usage)))) => {
                if !self.completed {
                    self.completed = true;
                    if let Err(error) = self.capture.reported(&usage, self.attempt_count) {
                        record_unreported(
                            &self.capture,
                            self.attempt_count,
                            format!("invalid_provider_usage: {error}"),
                        );
                    }
                } else if !self.extra_usage_recorded {
                    self.extra_usage_recorded = true;
                    record_unreported(
                        &self.capture,
                        self.attempt_count,
                        "provider_sent_extra_usage_event".to_string(),
                    );
                }
                std::task::Poll::Ready(Some(Ok(Event::Usage(usage))))
            }
            std::task::Poll::Ready(Some(Ok(Event::UsageUnavailable))) => {
                self.finish_unreported("provider_usage_missing");
                std::task::Poll::Ready(Some(Ok(Event::UsageUnavailable)))
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                self.finish_unreported(format!("stream_error: {error}"));
                std::task::Poll::Ready(Some(Err(error)))
            }
            std::task::Poll::Ready(None) => {
                self.finish_unreported("stream_ended_without_usage");
                std::task::Poll::Ready(None)
            }
            other => other,
        }
    }
}

fn record_unreported(capture: &UsageCapture, attempt_count: u32, reason: impl Into<String>) {
    if let Err(error) = capture.unreported(attempt_count, reason) {
        eprintln!("[mink] Warning: failed to record LLM usage: {error}");
    }
}

pub(crate) fn request_failure_attempt_count(error: &anyhow::Error) -> u32 {
    error
        .downcast_ref::<LlmRequestFailure>()
        .map(|failure| failure.attempt_count)
        .unwrap_or(1)
}

impl Drop for SseEventStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl futures::Stream for SseEventStream {
    type Item = Result<Event>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
