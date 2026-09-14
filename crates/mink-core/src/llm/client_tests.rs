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
async fn send_with_retry_retries_429_and_preserves_authorization() -> anyhow::Result<()> {
    let responses = vec![
        http_response(429, &[("retry-after", "0")], "rate limited"),
        http_response(200, &[], "ok"),
    ];
    let (api_url, seen, _server) = start_http_server(responses).await?;
    let ctx = test_context("client-retry", &api_url).await?;
    let client = AsyncLlClient::new("secret-key", &api_url)?;

    let (resp, _) = client
        .send_with_retry(
            ctx.display.as_ref(),
            br#"{"ping":true}"#.to_vec(),
            &ctx.cancel,
        )
        .await
        .map_err(|failure| failure.error)?;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(seen.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn send_with_retry_does_not_retry_non_retryable_400() -> anyhow::Result<()> {
    let responses = vec![http_response(400, &[], "bad request")];
    let (api_url, seen, _server) = start_http_server(responses).await?;
    let ctx = test_context("client-400", &api_url).await?;
    let client = AsyncLlClient::new("secret-key", &api_url)?;

    let err = client
        .send_with_retry(
            ctx.display.as_ref(),
            br#"{"ping":true}"#.to_vec(),
            &ctx.cancel,
        )
        .await
        .unwrap_err()
        .error
        .to_string();
    assert!(err.contains("HTTP 400"), "{err}");
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn retry_after_is_capped() -> anyhow::Result<()> {
    let responses = vec![
        http_response(429, &[("retry-after", "10000")], "rate limited"),
        http_response(429, &[("retry-after", "10000")], "rate limited"),
        http_response(429, &[("retry-after", "10000")], "rate limited"),
    ];
    let (api_url, seen, _server) = start_http_server(responses).await?;
    let ctx = test_context("client-retry-cap", &api_url).await?;
    let client = AsyncLlClient::new("secret-key", &api_url)?;

    let start = std::time::Instant::now();
    let err = client
        .send_with_retry(
            ctx.display.as_ref(),
            br#"{"ping":true}"#.to_vec(),
            &ctx.cancel,
        )
        .await
        .unwrap_err()
        .error
        .to_string();
    // Uncapped, retry-after 10000 would park each attempt for hours;
    // capped at 10s the failure arrives after ~2 sleeps + the 20s budget.
    assert!(err.contains("HTTP 429"), "{err}");
    assert!(start.elapsed() < std::time::Duration::from_secs(30));
    assert_eq!(seen.load(Ordering::SeqCst), 3);
    Ok(())
}

#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn send_is_cancellable() -> anyhow::Result<()> {
    let responses = vec![http_response(200, &[], "ok")];
    let (api_url, _seen, _server) = start_http_server(responses).await?;
    let ctx = test_context("client-cancel", &api_url).await?;
    let client = AsyncLlClient::new("secret-key", &api_url)?;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx_clone = ctx.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        // Bypass ctx.cancel (not cancelled) to exercise the parameter wiring.
        let result = client
            .send_with_retry(
                ctx_clone.display.as_ref(),
                br#"{"ping":true}"#.to_vec(),
                &cancel,
            )
            .await;
        let _ = tx.send(result);
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx).await?;
    match result {
        Ok(Err(failure)) => {
            assert_eq!(failure.error.to_string(), "request cancelled");
        }
        Ok(Ok(_)) => panic!("send unexpectedly succeeded"),
        Err(e) => panic!("task join failed: {e}"),
    }
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
            messages: vec![json!({"role":"user","content":"ping"})],
            tools: Vec::new(),
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
            messages: vec![json!({"role":"user","content":"ping"})],
            tools: Vec::new(),
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
