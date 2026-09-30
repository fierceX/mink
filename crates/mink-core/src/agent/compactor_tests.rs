use super::*;
use crate::agent::prefix::PrefixManager;
use crate::llm::client::LlmBackend;
use crate::llm::mock::MockLlmBackend;
use crate::protocol::Event;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn summary_script() -> Vec<Result<Event>> {
    vec![
        Ok(Event::Text(crate::protocol::TextEvent {
            content: "Task focus: x\nLatest request: y\nProgress: z\nTool evidence: none\nReflections: none"
                .into(),
        })),
        Ok(Event::Stop(crate::protocol::StopEvent {
            reason: "end_turn".into(),
        })),
    ]
}

/// 同一用户输入不再限制压缩次数：首次压缩之后，请求仍然装不下时必须能再次
/// 到达压缩引擎（这里第二次真的再次提交），而不是被入口互锁直接短路。
#[tokio::test]
async fn maybe_compact_attempts_again_after_a_successful_compaction() -> anyhow::Result<()> {
    let backend: Arc<dyn LlmBackend> = Arc::new(MockLlmBackend::new(
        "summary-model",
        vec![summary_script(), summary_script(), summary_script()],
    ));
    let ctx = crate::regression::test_context_for_agent_with_config_and_backend(
        "compactor-repeat",
        |cfg| {
            // 热尾部保持两次压缩都能找到合法切点。
            cfg.context_compact_tail_tokens = 5_000;
        },
        backend,
    )
    .await?;
    ctx.store
        .add_user(&format!("first {}", "x".repeat(24_000)))
        .await?;
    ctx.store
        .add_assistant(&"y".repeat(24_000), "", &[])
        .await?;
    ctx.store
        .add_user(&format!("second {}", "z".repeat(6_000)))
        .await?;
    ctx.store.add_assistant("ack", "", &[]).await?;
    ctx.store.add_user("third").await?;
    ctx.store.add_assistant("ack", "", &[]).await?;

    let prefix = PrefixManager::new(ctx.clone());
    let mut compactor = TurnCompactor::new(ctx.clone(), prefix);
    let mut messages = ctx.store.lines().await?;
    let mut system_prompt = String::new();
    let mut tools = Vec::new();
    let target = LlmModelTarget::new("test-model", None);

    let (first, detail) = compactor
        .maybe_compact_bounded(
            "manual",
            &mut messages,
            &mut system_prompt,
            &mut tools,
            target,
            None,
        )
        .await?;
    assert!(first, "the seeded history must compact: {detail}");
    assert_eq!(compactor.compactions_this_turn(), 1);

    // 同一输入的下一个 round 继续追加内容：必须还能压。
    ctx.store
        .add_assistant(&"w".repeat(24_000), "", &[])
        .await?;
    messages = ctx.store.lines().await?;
    let (second, detail) = compactor
        .maybe_compact_bounded(
            "preflight",
            &mut messages,
            &mut system_prompt,
            &mut tools,
            target,
            None,
        )
        .await?;
    assert!(
        second,
        "a later compaction in the same input must still run: {detail}"
    );
    assert_eq!(compactor.compactions_this_turn(), 2);
    assert_eq!(messages, ctx.compaction.active_messages().await?);

    ctx.flush_event_log().await?;
    let events = tokio::fs::read_to_string(&ctx.events_path).await?;
    assert_eq!(
        events.matches("\"type\":\"compact\"").count(),
        2,
        "{events}"
    );
    assert!(
        events.contains("_compactions_this_turn=2"),
        "the compact event carries the in-input ordinal: {events}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local loopback sockets"]
async fn maybe_compact_success_refreshes_context_and_prefix() -> anyhow::Result<()> {
    let (api_url, _server) = start_summary_server("Task focus: compacted\nLatest request: test\nProgress: done\nTool evidence: none\nReflections: none").await?;
    let ctx = crate::regression::test_context_for_agent_with_config("compactor-success", |cfg| {
        cfg.base_url = api_url.clone();
        // 与其余 compaction 测试一致：热尾部目标压到 1，使小对话也能
        // 通过"节省 ≥10%"门控（默认 256K 尾部对微型对话是恒等压缩）。
        cfg.context_compact_tail_tokens = 1;
    })
    .await?;
    for idx in 0..3 {
        ctx.store.add_user(&format!("user {idx}")).await?;
        ctx.store
            .add_assistant(&format!("assistant {idx}"), "", &[])
            .await?;
    }
    let prefix = PrefixManager::new(ctx.clone());
    let (_old_prompt, _old_tools) = prefix.ensure().await?;
    let mut compactor = TurnCompactor::new(ctx.clone(), prefix);
    let mut messages = ctx.store.lines().await?;
    let mut system_prompt = String::new();
    let mut tools = Vec::new();

    let (did_compact, detail) = compactor
        .maybe_compact_bounded(
            "manual",
            &mut messages,
            &mut system_prompt,
            &mut tools,
            LlmModelTarget::new("test-model", None),
            None,
        )
        .await?;

    assert!(did_compact, "{detail}");
    assert_eq!(compactor.compactions_this_turn(), 1);
    assert_eq!(messages, ctx.compaction.active_messages().await?);
    assert!(!system_prompt.is_empty());
    assert!(!tools.is_empty());
    Ok(())
}

async fn start_summary_server(
    summary_text: &str,
) -> anyhow::Result<(String, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let summary_text = summary_text.to_string();
    let handle = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while let Ok(n) = socket.read(&mut chunk).await {
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"delta":{"content":summary_text}}]}),
            json!({"choices":[{"finish_reason":"stop","delta":{}}],"usage":{"prompt_tokens":4,"completion_tokens":2}})
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });
    Ok((format!("http://{addr}/chat/completions"), handle))
}
