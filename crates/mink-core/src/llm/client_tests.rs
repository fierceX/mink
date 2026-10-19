use super::*;
use crate::cancel::CancellationToken;
use crate::context::AgentSharedContext;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn response_body_decode_error_is_a_recoverable_transport_failure() -> anyhow::Result<()> {
    let (api_url, _, server) =
        start_http_server(vec![http_response(200, &[], "invalid json")]).await?;
    let response = AsyncLlClient::new("secret-key", &api_url)?
        .send_once(br#"{"ping":true}"#.to_vec(), &CancellationToken::new())
        .await?;
    let error = response.json::<serde_json::Value>().await.unwrap_err();
    server.abort();
    assert!(error.is_decode(), "{error}");
    assert_eq!(
        crate::llm::recovery::classify_transport_error(&error),
        UpstreamFailureKind::Recoverable
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn send_once_classifies_429_and_keeps_structured_retry_after() -> anyhow::Result<()> {
    let responses = vec![
        http_response(429, &[("retry-after", "0")], "rate limited"),
        http_response(200, &[], "ok"),
    ];
    let (api_url, seen, _server) = start_http_server(responses).await?;
    let client = AsyncLlClient::new("secret-key", &api_url)?;

    let error = client
        .send_once(
            br#"{"ping":true}"#.to_vec(),
            &crate::cancel::CancellationToken::new(),
        )
        .await
        .expect_err("429 must surface as a typed failure");
    assert_eq!(error.kind(), UpstreamFailureKind::Recoverable);
    assert_eq!(error.status(), Some(429));
    assert_eq!(error.retry_after(), Some(Duration::from_secs(0)));
    // One call = exactly one physical request; retries belong to the runtime
    // attempt loop, never to the backend.
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn send_once_marks_400_as_permanent() -> anyhow::Result<()> {
    let responses = vec![http_response(400, &[], "bad request")];
    let (api_url, seen, _server) = start_http_server(responses).await?;
    let client = AsyncLlClient::new("secret-key", &api_url)?;

    let error = client
        .send_once(
            br#"{"ping":true}"#.to_vec(),
            &crate::cancel::CancellationToken::new(),
        )
        .await
        .expect_err("400 must surface as a typed failure");
    assert_eq!(error.kind(), UpstreamFailureKind::Permanent);
    assert_eq!(error.status(), Some(400));
    assert!(error.to_string().contains("HTTP 400"), "{error}");
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn send_once_reports_retry_after_without_waiting() -> anyhow::Result<()> {
    let responses = vec![http_response(
        429,
        &[("retry-after", "10000")],
        "rate limited",
    )];
    let (api_url, seen, _server) = start_http_server(responses).await?;
    let client = AsyncLlClient::new("secret-key", &api_url)?;

    let start = std::time::Instant::now();
    let error = client
        .send_once(
            br#"{"ping":true}"#.to_vec(),
            &crate::cancel::CancellationToken::new(),
        )
        .await
        .expect_err("429 must surface as a typed failure");
    // A valid Retry-After is surfaced verbatim (a floor for the runtime wait),
    // never awaited inside the backend and never truncated to the backoff cap.
    assert_eq!(error.retry_after(), Some(Duration::from_secs(10_000)));
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn send_once_observes_cancel() -> anyhow::Result<()> {
    let responses = vec![http_response(200, &[], "ok")];
    let (api_url, _seen, _server) = start_http_server(responses).await?;
    let client = AsyncLlClient::new("secret-key", &api_url)?;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = client
        .send_once(br#"{"ping":true}"#.to_vec(), &cancel)
        .await
        .expect_err("a cancelled request must not be sent");
    assert_eq!(error.kind(), UpstreamFailureKind::Recoverable);
    assert_eq!(error.message(), "request cancelled");
    Ok(())
}
#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn stream_parses_sse_text_usage_and_stop() -> anyhow::Result<()> {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"pong"}}]}),
        json!({"choices":[{"finish_reason":"stop","delta":{}}],"usage":{"prompt_tokens":7,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":3}}})
    );
    let responses = vec![http_response(
        200,
        &[("content-type", "text/event-stream")],
        &body,
    )];
    let (api_url, seen, _server) = start_http_server(responses).await?;
    let ctx = test_context("client-stream", &api_url).await?;
    let response = OpenAiCompatibleBackend::deepseek_defaults()
        .stream(LlmRequest {
            purpose: LlmPurpose::Agent,
            model: "deepseek-v4-flash".into(),
            model_alias: Some("flash".into()),
            api_url,
            api_key: "secret-key".into(),
            system_prompt: "system".into(),
            messages: vec![json!({"role":"user","content":"ping"})].into(),
            tools: Vec::new().into(),
            max_tokens: ctx.max_tokens(),
            cancel: ctx.cancel.clone(),
            verbose: ctx.verbose(),
            display: ctx.display.clone(),
        })
        .await?;
    let mut stream = response.events;

    let mut text = String::new();
    let mut usage = None;
    let mut stop = None;
    while let Some(event) = stream.next().await {
        match event? {
            Event::Text(t) => text.push_str(&t.content),
            Event::Usage(u) => usage = Some(u),
            Event::Stop(s) => {
                stop = Some(s.reason);
                break;
            }
            _ => {}
        }
    }
    let usage = usage.expect("usage event");
    assert_eq!(text, "pong");
    assert_eq!(usage.input_tokens, 4);
    assert_eq!(usage.cache_read_input_tokens, 3);
    assert_eq!(usage.output_tokens, 2);
    assert_eq!(stop.as_deref(), Some("stop"));
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    Ok(())
}

struct FailingBackend;

#[async_trait::async_trait]
impl LlmBackend for FailingBackend {
    fn name(&self) -> &str {
        "failing"
    }

    async fn stream(&self, _request: LlmRequest) -> Result<LlmResponseStream> {
        anyhow::bail!("backend unavailable")
    }
}

struct NeverEstablishingBackend;

#[async_trait::async_trait]
impl LlmBackend for NeverEstablishingBackend {
    fn name(&self) -> &str {
        "never-establishing"
    }

    async fn stream(&self, _request: LlmRequest) -> Result<LlmResponseStream> {
        futures::future::pending::<()>().await;
        unreachable!()
    }
}

struct AttemptFailingBackend;

#[async_trait::async_trait]
impl LlmBackend for AttemptFailingBackend {
    fn name(&self) -> &str {
        "attempt-failing"
    }

    async fn stream(&self, _request: LlmRequest) -> Result<LlmResponseStream> {
        Err(LlmRequestFailure {
            attempt_count: 3,
            error: anyhow::anyhow!("transport unavailable"),
        }
        .into())
    }
}

#[tokio::test]
async fn backend_request_records_unreported_usage_when_request_fails() -> anyhow::Result<()> {
    let ctx = test_context("backend-request-failed", "https://example.invalid/v1").await?;
    let result = stream_backend(
        &(Arc::new(FailingBackend) as Arc<dyn LlmBackend>),
        &ctx,
        "custom-model",
        None,
        &[json!({"role":"user","content":"ping"})],
        &[],
        "system",
    )
    .await;
    let err = match result {
        Ok(_) => anyhow::bail!("expected backend failure"),
        Err(error) => error.to_string(),
    };
    assert!(err.contains("backend unavailable"), "{err}");
    let records = ctx.usage.all_records()?;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].status,
        crate::session::usage::UsageStatus::Unreported
    );
    assert_eq!(records[0].kind, crate::session::usage::UsageKind::Agent);
    assert_eq!(records[0].model, "custom-model");
    assert_eq!(records[0].attempt_count, 1);
    assert!(
        records[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("request_failed: backend unavailable")),
        "{:?}",
        records[0].reason
    );
    Ok(())
}

#[tokio::test]
async fn backend_request_preserves_request_failure_attempt_count() -> anyhow::Result<()> {
    let ctx = test_context("backend-attempt-failed", "https://example.invalid/v1").await?;
    let result = stream_backend(
        &(Arc::new(AttemptFailingBackend) as Arc<dyn LlmBackend>),
        &ctx,
        "custom-model",
        None,
        &[json!({"role":"user","content":"ping"})],
        &[],
        "system",
    )
    .await;
    let err = match result {
        Ok(_) => anyhow::bail!("expected backend failure"),
        Err(error) => error.to_string(),
    };
    assert!(err.contains("transport unavailable"), "{err}");
    let records = ctx.usage.all_records()?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].attempt_count, 3);
    assert!(
        records[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("request_failed: transport unavailable")),
        "{:?}",
        records[0].reason
    );
    Ok(())
}

async fn test_context(name: &str, api_url: &str) -> anyhow::Result<Arc<AgentSharedContext>> {
    crate::regression::test_context_for_agent_with_api_url(name, api_url).await
}

struct TrimmingCacheBackend;

#[async_trait::async_trait]
impl LlmBackend for TrimmingCacheBackend {
    fn name(&self) -> &str {
        "trimming-cache"
    }

    fn cache_projection(
        &self,
        request: &LlmRequest,
        source_prefix_len: usize,
    ) -> Option<LlmCacheProjection> {
        let len = source_prefix_len.saturating_sub(1);
        Some(LlmCacheProjection {
            model: request.model.clone(),
            system_prompt: request.system_prompt.clone(),
            tools: request.tools.clone(),
            messages: Arc::new(request.messages[..len].to_vec()),
        })
    }

    async fn stream(&self, _request: LlmRequest) -> Result<LlmResponseStream> {
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![Ok(Event::Stop(LlmStopEvent {
                reason: "end_turn".into(),
            }))])),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn cache_projection_hashes_follow_provider_visible_prefix() -> anyhow::Result<()> {
    let ctx = test_context("cache-projection-hashes", "https://example.invalid/v1").await?;
    let messages = vec![
        json!({"role":"user","content":"first"}),
        json!({"role":"assistant","content":"answer"}),
        json!({"role":"user","content":"second"}),
    ];
    let projection = crate::llm::request::RequestProjection::build(&messages, "system", &[])?;
    let backend: Arc<dyn LlmBackend> = Arc::new(TrimmingCacheBackend);
    let prepared = prepare_llm_request(&backend, &ctx, "test-model", None, &projection).await?;
    let cached = prepared
        .cache_projection
        .as_ref()
        .expect("cache projection");
    assert_eq!(cached.messages.len(), messages.len() - 1);
    assert_eq!(
        prepared.cache_projection_hashes.len(),
        cached.messages.len(),
        "hashes must be taken from the provider-visible cache projection"
    );
    assert_eq!(
        *prepared.cache_projection_hashes,
        crate::session::compaction::message_hashes(&cached.messages)
    );
    Ok(())
}

struct PrebuiltBodyBackend;

#[async_trait::async_trait]
impl LlmBackend for PrebuiltBodyBackend {
    fn name(&self) -> &str {
        "prebuilt-body"
    }

    fn supports_prebuilt_body(&self) -> bool {
        true
    }

    fn build_body(
        &self,
        _request: &LlmRequest,
        _openai_messages: &[serde_json::Value],
        _openai_tools: &[serde_json::Value],
    ) -> Result<Option<Arc<Vec<u8>>>> {
        Ok(Some(Arc::new(b"prebuilt".to_vec())))
    }

    async fn stream(&self, _request: LlmRequest) -> Result<LlmResponseStream> {
        Ok(LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![Ok(Event::Stop(LlmStopEvent {
                reason: "end_turn".into(),
            }))])),
            attempt_count: 1,
        })
    }

    async fn stream_prepared(
        &self,
        request: LlmRequest,
        body: Option<Arc<Vec<u8>>>,
    ) -> Result<LlmResponseStream> {
        anyhow::ensure!(body.is_some(), "prepared body missing");
        self.stream(request).await
    }
}

#[tokio::test]
async fn prebuilt_body_requires_matching_stream_prepared() -> anyhow::Result<()> {
    let ctx = test_context("prebuilt-body-contract", "https://example.invalid/v1").await?;
    let projection = crate::llm::request::RequestProjection::build(&[], "system", &[])?;
    let backend: Arc<dyn LlmBackend> = Arc::new(PrebuiltBodyBackend);
    let prepared = prepare_llm_request(&backend, &ctx, "test-model", None, &projection).await?;
    assert!(prepared.body.is_some());

    let default_backend: Arc<dyn LlmBackend> = Arc::new(TrimmingCacheBackend);
    let error = match default_backend
        .stream_prepared(
            prepared.request_for(ctx.cancel.clone()),
            Some(Arc::new(Vec::new())),
        )
        .await
    {
        Ok(_) => panic!("a body without stream_prepared must fail"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("does not implement stream_prepared")
    );
    Ok(())
}

#[tokio::test]
async fn request_clone_shares_payload_allocation() -> anyhow::Result<()> {
    let ctx = test_context("request-clone", "https://example.invalid/v1").await?;
    let request = timeout_test_request(&ctx, "https://example.invalid/v1".into());
    let cloned = request.clone();
    assert!(Arc::ptr_eq(&request.messages, &cloned.messages));
    assert!(Arc::ptr_eq(&request.tools, &cloned.tools));
    assert!(Arc::ptr_eq(&request.system_prompt, &cloned.system_prompt));
    Ok(())
}

#[tokio::test]
async fn prepared_body_matches_the_ordinary_build() -> anyhow::Result<()> {
    let ctx = test_context("prepared-body", "https://example.invalid/v1").await?;
    let request = timeout_test_request(&ctx, "https://example.invalid/v1".into());
    let backend = OpenAiCompatibleBackend::deepseek_defaults();
    let projection = crate::llm::request::RequestProjection::build(
        &request.messages,
        &request.system_prompt,
        &request.tools,
    )?;
    let dyn_backend: Arc<dyn LlmBackend> = Arc::new(OpenAiCompatibleBackend::deepseek_defaults());
    let prepared = prepare_llm_request(&dyn_backend, &ctx, "test-model", None, &projection).await?;
    assert!(
        prepared.body.is_some(),
        "the built-in backend must opt into the prebuilt body path"
    );
    for messages in [
        (*request.messages).clone(),
        vec![json!({"role":"user","content":[
            {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
        ]})],
    ] {
        let mut request = request.clone();
        request.messages = Arc::new(messages);
        let converted = crate::llm::transport::convert_messages_to_openai(&request.messages)?;
        let tools = crate::llm::transport::convert_tools_to_openai(&request.tools);
        let prepared = backend
            .build_body(&request, &converted, &tools)?
            .expect("built-in backend must build a body");
        let ordinary = crate::llm::transport::build_openai_body_with_options_and_extensions(
            &request.model,
            &request.messages,
            &request.tools,
            &request.system_prompt,
            request.max_tokens,
            &OpenAiCompatibleOptions::default(),
            None,
            &Default::default(),
        )?;
        assert_eq!(&*prepared, &ordinary);
        assert!(
            !serde_json::to_string(&*request.messages)?.contains("image_url")
                || String::from_utf8_lossy(&prepared).contains("data:image/png;base64,AAAA")
        );
    }
    Ok(())
}

fn http_response(status: u16, headers: &[(&str, &str)], body: &str) -> String {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        429 => "Too Many Requests",
        _ => "Status",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\n",
        body.len()
    );
    for (key, value) in headers {
        response.push_str(&format!("{key}: {value}\r\n"));
    }
    response.push_str("connection: close\r\n\r\n");
    response.push_str(body);
    response
}

async fn start_http_server(
    responses: Vec<String>,
) -> anyhow::Result<(String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_server = seen.clone();
    let responses = Arc::new(Mutex::new(responses.into_iter()));
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let idx = seen_server.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![0u8; 8192];
            let Ok(n) = socket.read(&mut buf).await else {
                return;
            };
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.contains("POST /chat/completions HTTP/1.1"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer secret-key"),
                "{request}"
            );
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains(&format!("user-agent: mink/{}", env!("CARGO_PKG_VERSION"))),
                "{request}"
            );
            let response = {
                let mut responses = responses.lock().unwrap();
                responses
                    .next()
                    .unwrap_or_else(|| panic!("missing mock response for request {idx}"))
            };
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    Ok((format!("http://{addr}/chat/completions"), seen, handle))
}

#[tokio::test]
async fn stream_eof_residual_frame_without_trailing_newline_is_parsed() -> anyhow::Result<()> {
    // 连接在最后一帧后被切断：帧 JSON 完整但没有尾部换行、也没有 [DONE]。
    // 传输层 EOF 时必须把缓冲残留送入 parser，否则 finish_reason/usage 丢失。
    let body = format!(
        "data: {}\n\ndata: {}",
        json!({"choices":[{"delta":{"content":"tail"}}]}),
        json!({"choices":[{"finish_reason":"stop","delta":{}}],"usage":{"prompt_tokens":5,"completion_tokens":1}})
    );
    let responses = vec![http_response(
        200,
        &[("content-type", "text/event-stream")],
        &body,
    )];
    let (api_url, _seen, _server) = start_http_server(responses).await?;
    let ctx = test_context("client-eof-residual", &api_url).await?;
    let response = OpenAiCompatibleBackend::deepseek_defaults()
        .stream(LlmRequest {
            purpose: LlmPurpose::Agent,
            model: "deepseek-v4-flash".into(),
            model_alias: Some("flash".into()),
            api_url,
            api_key: "secret-key".into(),
            system_prompt: "system".into(),
            messages: vec![json!({"role":"user","content":"ping"})].into(),
            tools: Vec::new().into(),
            max_tokens: ctx.max_tokens(),
            cancel: ctx.cancel.clone(),
            verbose: ctx.verbose(),
            display: ctx.display.clone(),
        })
        .await?;
    let mut stream = response.events;

    let mut text = String::new();
    let mut stop = None;
    while let Some(event) = stream.next().await {
        match event? {
            Event::Text(t) => text.push_str(&t.content),
            Event::Stop(s) => {
                stop = Some(s.reason);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(text, "tail");
    assert_eq!(stop.as_deref(), Some("stop"));
    Ok(())
}

#[tokio::test]
async fn dropped_establishment_future_records_unknown_usage_once() -> anyhow::Result<()> {
    let ctx = test_context("backend-establish-cancelled", "https://example.invalid/v1").await?;
    let backend: Arc<dyn LlmBackend> = Arc::new(NeverEstablishingBackend);
    let result = tokio::time::timeout(
        tokio::time::Duration::from_millis(50),
        stream_backend(
            &backend,
            &ctx,
            "custom-model",
            None,
            &[json!({"role":"user","content":"ping"})],
            &[],
            "system",
        ),
    )
    .await;
    assert!(
        result.is_err(),
        "the establishment future must still be pending"
    );

    let records = ctx.usage.all_records()?;
    assert_eq!(
        records.len(),
        1,
        "a cancelled request must record exactly one unknown-usage entry: {records:?}"
    );
    assert_eq!(
        records[0].status,
        crate::session::usage::UsageStatus::Unreported
    );
    assert_eq!(records[0].kind, crate::session::usage::UsageKind::Agent);
    assert_eq!(records[0].model, "custom-model");
    assert!(records[0].tokens.is_none());
    assert!(
        records[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("cancelled")),
        "{:?}",
        records[0].reason
    );
    Ok(())
}

#[tokio::test]
async fn sse_producer_backpressures_instead_of_buffering_unbounded() {
    use crate::protocol::{Event, TextEvent};

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let mut pending = vec![
        Event::Text(TextEvent {
            content: "a".into(),
        }),
        Event::Text(TextEvent {
            content: "b".into(),
        }),
    ];

    let cancel = crate::cancel::CancellationToken::new();
    let blocked = tokio::time::timeout(
        tokio::time::Duration::from_millis(50),
        super::AsyncLlClient::send_events(&tx, &mut pending, &cancel),
    )
    .await;
    assert!(
        blocked.is_err(),
        "a bounded producer must wait for capacity instead of buffering"
    );

    // The cancelled send drained the in-flight event; refill and verify the
    // producer resumes once the consumer frees capacity.
    let first = rx.recv().await.expect("first event queued");
    assert!(first.is_ok());
    pending.push(Event::Text(TextEvent {
        content: "c".into(),
    }));
    let finished = tokio::time::timeout(
        tokio::time::Duration::from_secs(1),
        super::AsyncLlClient::send_events(&tx, &mut pending, &cancel),
    )
    .await
    .expect("capacity must unblock the producer");
    assert_eq!(finished, super::SseSendStatus::Delivered);
    assert!(rx.recv().await.is_some());
}

#[tokio::test]
async fn sse_producer_stops_when_consumer_is_dropped() {
    use crate::protocol::{Event, StopEvent};

    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    let mut pending = vec![Event::Stop(StopEvent {
        reason: "end_turn".into(),
    })];
    let cancel = crate::cancel::CancellationToken::new();
    assert_eq!(
        super::AsyncLlClient::send_events(&tx, &mut pending, &cancel).await,
        super::SseSendStatus::Stopped,
        "a dropped consumer must stop the producer instead of accumulating"
    );
}

#[tokio::test]
async fn sse_send_events_observes_cancel_with_a_live_receiver() {
    use crate::protocol::{Event, TextEvent};

    // Receiver stays alive but never consumes: only the cancel token can
    // release the blocked send (drop-the-receiver is a different path).
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let cancel = crate::cancel::CancellationToken::new();
    let task_cancel = cancel.clone();
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task_finished = finished.clone();
    let handle = tokio::spawn(async move {
        let mut pending = vec![
            Event::Text(TextEvent {
                content: "a".into(),
            }),
            Event::Text(TextEvent {
                content: "b".into(),
            }),
        ];
        let status = super::AsyncLlClient::send_events(&tx, &mut pending, &task_cancel).await;
        task_finished.store(true, std::sync::atomic::Ordering::SeqCst);
        status
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;
    assert!(
        !finished.load(std::sync::atomic::Ordering::SeqCst),
        "the producer is expected to be blocked on the full queue"
    );

    cancel.cancel();
    let status = tokio::time::timeout(tokio::time::Duration::from_secs(1), handle)
        .await
        .expect("cancel must release a full-queue send")
        .unwrap();
    assert_eq!(status, super::SseSendStatus::Stopped);
}

#[tokio::test]
async fn sse_cancelled_interruption_notice_never_waits_for_capacity() {
    use crate::protocol::{Event, TextEvent};

    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(Ok(Event::Text(TextEvent {
        content: "filler".into(),
    })))
    .unwrap();

    let started = std::time::Instant::now();
    super::AsyncLlClient::try_send_interrupted(&tx);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(50),
        "interruption notice must not wait for capacity: {:?}",
        started.elapsed()
    );
}

struct DoubleUsageBackend;

#[async_trait::async_trait]
impl LlmBackend for DoubleUsageBackend {
    fn name(&self) -> &str {
        "double-usage"
    }

    async fn stream(
        &self,
        _request: crate::llm::client::LlmRequest,
    ) -> anyhow::Result<crate::llm::client::LlmResponseStream> {
        let usage = |input: i64, output: i64| crate::protocol::UsageEvent {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        Ok(crate::llm::client::LlmResponseStream {
            events: Box::pin(futures::stream::iter(vec![
                Ok(Event::Usage(usage(1, 1))),
                Ok(Event::Usage(usage(2, 2))),
                Ok(Event::Stop(crate::protocol::StopEvent {
                    reason: "end_turn".into(),
                })),
            ])),
            attempt_count: 1,
        })
    }
}

#[tokio::test]
async fn extra_usage_event_does_not_open_a_second_ledger_entry() -> anyhow::Result<()> {
    let ctx = test_context("double-usage", "https://example.invalid/v1").await?;
    let mut stream = stream_backend(
        &(Arc::new(DoubleUsageBackend) as Arc<dyn LlmBackend>),
        &ctx,
        "custom-model",
        None,
        &[json!({"role":"user","content":"ping"})],
        &[],
        "system",
    )
    .await?;
    while let Some(event) = stream.next().await {
        let _ = event?;
    }

    // One physical attempt settles exactly one record; the second Usage frame
    // is a duplicate for the same request, not a new attempt.
    let records = ctx.usage.all_records()?;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0].status,
        crate::session::usage::UsageStatus::Reported
    );
    Ok(())
}

#[test]
fn wrapped_request_failure_keeps_attempt_count() {
    let failure = LlmRequestFailure {
        attempt_count: 7,
        error: anyhow::anyhow!("transport unavailable"),
    };
    let wrapped = anyhow::Error::new(failure).context("aggregate backend failure");
    assert_eq!(super::request_failure_attempt_count(&wrapped), 7);
}

#[test]
fn sse_line_limit_only_trips_on_an_unterminated_frame() {
    let mut terminated = vec![b'a'; MAX_SSE_LINE_BYTES + 1];
    terminated.push(b'\n');
    assert!(!super::sse_line_limit_exceeded(&terminated));
    assert!(super::sse_line_limit_exceeded(&vec![
        b'a';
        MAX_SSE_LINE_BYTES + 1
    ]));
}

// ── provider HTTP 总超时的真实行为 ──

/// 启动慢速 SSE 服务：立即写响应头，然后每 `interval` 发一个非完成帧，
/// 持续 `duration`；`finish` 时追加 finish_reason + [DONE]。
async fn start_slow_sse_server(
    interval: Duration,
    duration: Duration,
    finish: bool,
) -> anyhow::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = vec![0u8; 8192];
        let _ = socket.read(&mut buf).await;
        if socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
        let started = std::time::Instant::now();
        while started.elapsed() < duration {
            let frame = format!("data: {}\n\n", json!({"choices":[{"delta":{}}]}));
            if socket.write_all(frame.as_bytes()).await.is_err() {
                return;
            }
            tokio::time::sleep(interval).await;
        }
        if finish {
            let tail = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"finish_reason":"stop","delta":{}}]})
            );
            let _ = socket.write_all(tail.as_bytes()).await;
        }
    });
    Ok(format!("http://{addr}/chat/completions"))
}

/// 每个连接在响应头之后延迟 `delay` 再发完成帧。
async fn start_delayed_sse_server(connections: usize, delay: Duration) -> anyhow::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        for _ in 0..connections {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let _ = socket.read(&mut buf).await;
            if socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            tokio::time::sleep(delay).await;
            let tail = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"finish_reason":"stop","delta":{}}]})
            );
            let _ = socket.write_all(tail.as_bytes()).await;
        }
    });
    Ok(format!("http://{addr}/chat/completions"))
}

fn timeout_test_request(ctx: &Arc<AgentSharedContext>, api_url: String) -> LlmRequest {
    LlmRequest {
        purpose: LlmPurpose::Agent,
        model: "deepseek-v4-flash".into(),
        model_alias: Some("flash".into()),
        api_url,
        api_key: "secret-key".into(),
        system_prompt: "system".into(),
        messages: vec![json!({"role":"user","content":"ping"})].into(),
        tools: Vec::new().into(),
        max_tokens: ctx.max_tokens(),
        cancel: ctx.cancel.clone(),
        verbose: ctx.verbose(),
        display: ctx.display.clone(),
    }
}

/// `http_timeout_secs=1` 时，非 idle 的长流（每 100ms 有事件）
/// 也必须在 1 秒总超时处终止。
#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn http_total_timeout_terminates_a_heartbeat_sse_stream() -> anyhow::Result<()> {
    let api_url = start_slow_sse_server(
        Duration::from_millis(100),
        Duration::from_millis(2_000),
        true,
    )
    .await?;
    let ctx = test_context("http-total-timeout", &api_url).await?;
    let backend = OpenAiCompatibleBackend::deepseek_defaults().with_http_timeout_secs(1);
    let response = backend.stream(timeout_test_request(&ctx, api_url)).await?;
    let started = std::time::Instant::now();
    let mut stream = response.events;
    let mut stop = false;
    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::Stop(_)) => {
                stop = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let elapsed = started.elapsed();
    assert!(
        !stop,
        "the 1s total timeout must cut the stream before completion"
    );
    assert!(
        elapsed < Duration::from_millis(1_800),
        "the total timeout must fire near 1s: {elapsed:?}"
    );
    Ok(())
}

/// `http_timeout_secs=0` 关闭总超时，同一长流正常读到完成帧。
#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn zero_http_timeout_lets_the_stream_finish() -> anyhow::Result<()> {
    let api_url = start_slow_sse_server(
        Duration::from_millis(100),
        Duration::from_millis(1_200),
        true,
    )
    .await?;
    let ctx = test_context("http-no-timeout", &api_url).await?;
    let backend = OpenAiCompatibleBackend::deepseek_defaults().with_http_timeout_secs(0);
    let response = backend.stream(timeout_test_request(&ctx, api_url)).await?;
    let started = std::time::Instant::now();
    let mut stream = response.events;
    let mut stop = false;
    while let Some(event) = stream.next().await {
        if matches!(event?, Event::Stop(_)) {
            stop = true;
            break;
        }
    }
    assert!(
        stop,
        "0 = no total timeout: the stream must reach its finish"
    );
    assert!(started.elapsed() >= Duration::from_millis(1_000));
    Ok(())
}

/// backend 已经用过一次后，`with_http_timeout_secs` 必须丢弃缓存的
/// HTTP client，否则新配置不会生效。
#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn http_timeout_setter_applies_after_first_use() -> anyhow::Result<()> {
    let api_url = start_delayed_sse_server(2, Duration::from_millis(1_500)).await?;
    let ctx = test_context("http-timeout-setter", &api_url).await?;
    let backend = OpenAiCompatibleBackend::deepseek_defaults().with_http_timeout_secs(600);
    let response = backend
        .stream(timeout_test_request(&ctx, api_url.clone()))
        .await?;
    let mut stream = response.events;
    let mut stop = false;
    while let Some(event) = stream.next().await {
        if matches!(event?, Event::Stop(_)) {
            stop = true;
            break;
        }
    }
    assert!(stop, "the first (600s) request must complete");

    let backend = backend.with_http_timeout_secs(1);
    let response = backend.stream(timeout_test_request(&ctx, api_url)).await?;
    let started = std::time::Instant::now();
    let mut stream = response.events;
    let mut stop = false;
    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::Stop(_)) => {
                stop = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(!stop, "the 1s timeout must cut the second request");
    assert!(
        started.elapsed() < Duration::from_millis(1_400),
        "a cached client would still wait 1.5s: {:?}",
        started.elapsed()
    );
    Ok(())
}
