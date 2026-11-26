use crate::cancel::{CancellationToken, InterruptSignal};
use crate::config::ResolvedConfig;
use crate::context::ToolContext;
use crate::tools::metadata::{ApprovalTier, ToolMetadata, ToolResultKind};
use crate::tools::runner::{ToolExec, ToolOutcome};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const TOOLS_URL: &str = "https://api.deepseek.com/v1/tools/";
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

pub(crate) fn command(input: &str) -> Option<(&str, &str)> {
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    let (name, argument) = input.split_at(end);
    matches!(name, "/web-search" | "/web-fetch").then_some((name, argument.trim()))
}

pub(crate) fn is_deepseek_endpoint(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("api.deepseek.com")
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
    })
}

pub(crate) fn api_key(config: &ResolvedConfig) -> Option<Arc<str>> {
    if !config.web.client {
        return None;
    }
    if !config.api_key.is_empty() && is_deepseek_endpoint(&crate::config::api_url(config)) {
        return Some(Arc::from(config.api_key.as_str()));
    }
    std::env::var("DEEPSEEK_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .map(Arc::from)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    count: Option<u32>,
}

impl SearchArgs {
    fn body(self) -> Result<Value> {
        ensure!(
            !self.query.trim().is_empty() && self.query.chars().count() <= 128,
            "query must contain 1–128 Unicode characters and cannot be blank"
        );
        let count = self.count.unwrap_or(10);
        ensure!((1..=50).contains(&count), "count must be between 1 and 50");
        Ok(json!({"query": self.query, "count": count, "need_content": false}))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    url: String,
}

impl FetchArgs {
    fn body(self) -> Result<Value> {
        let parsed = reqwest::Url::parse(&self.url).context("invalid webpage URL")?;
        ensure!(
            self.url.chars().count() <= 8192
                && matches!(parsed.scheme(), "http" | "https")
                && parsed.host_str().is_some()
                && parsed.username().is_empty()
                && parsed.password().is_none(),
            "url must be an HTTP(S) URL with a host and no credentials"
        );
        Ok(json!({"url": parsed.as_str()}))
    }
}

#[derive(Deserialize, Serialize)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
    site_name: Option<String>,
    #[serde(rename(deserialize = "date", serialize = "published_at"))]
    published_at: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct SearchResponse {
    results: Vec<SearchResult>,
}

#[derive(Deserialize)]
struct FetchResponse {
    title: String,
    content: String,
}

async fn request(
    endpoint: &str,
    key: &str,
    body: &Value,
    timeout: Duration,
    cancel: &CancellationToken,
    interrupt: &InterruptSignal,
) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()?;
    let send = async {
        let mut response = client
            .post(endpoint)
            .bearer_auth(key)
            .json(body)
            .send()
            .await
            .context("DeepSeek web request failed")?;
        let status = response.status();
        if !status.is_success() {
            let detail = match status.as_u16() {
                401 => "invalid API key",
                402 => "insufficient balance",
                403 => "request refused by DeepSeek",
                404 => "page content unavailable",
                429 => "DeepSeek rate limit reached",
                _ => "request rejected",
            };
            bail!("DeepSeek web API returned HTTP {status}: {detail}");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("DeepSeek web response failed")?
        {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES,
                "DeepSeek web response exceeds the size limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("web request interrupted"),
        _ = interrupt.notified() => bail!("web request interrupted"),
        result = send => result,
    }
}

fn run(ctx: &ToolContext, operation: &str, body: &Value) -> Result<Vec<u8>> {
    ensure!(ctx.tool_config.web_client, "Web Search/Fetch is disabled");
    let key = ctx.web_api_key.as_deref().context(
        "Web Search/Fetch requires a DeepSeek API key (the configured DeepSeek key or DEEPSEEK_API_KEY)",
    )?;
    let timeout = Duration::from_secs(ctx.tool_config.tool_timeout_secs.clamp(1, 120) as u64);
    tokio::runtime::Handle::current().block_on(request(
        &format!("{TOOLS_URL}{operation}"),
        key,
        body,
        timeout,
        &ctx.cancel,
        &ctx.interrupt,
    ))
}

pub(crate) struct WebSearchTool;

impl ToolExec for WebSearchTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new("WebSearch", ApprovalTier::Read, ToolResultKind::Search)
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let args: SearchArgs = serde_json::from_value(input.clone())?;
        let body = run(ctx, "search", &args.body()?)?;
        let result: SearchResponse =
            serde_json::from_slice(&body).context("invalid DeepSeek search response")?;
        let content = serde_json::to_string_pretty(&result)?;
        Ok(ToolOutcome::text(content))
    }
}

pub(crate) struct WebFetchTool;

impl ToolExec for WebFetchTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::new("WebFetch", ApprovalTier::Read, ToolResultKind::Text)
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> Result<ToolOutcome> {
        let args: FetchArgs = serde_json::from_value(input.clone())?;
        let request = args.body()?;
        let body = run(ctx, "fetch", &request)?;
        let page: FetchResponse =
            serde_json::from_slice(&body).context("invalid DeepSeek fetch response")?;
        ensure!(
            !page.content.trim().is_empty(),
            "webpage has no readable content"
        );
        let url = request["url"].as_str().unwrap_or_default();
        let title: String = page
            .title
            .trim()
            .lines()
            .next()
            .unwrap_or(url)
            .chars()
            .filter(|ch| !ch.is_control())
            .take(400)
            .collect();
        Ok(ToolOutcome::text(format!(
            "# {title}\n\nSource: {url}\n\n{}",
            page.content
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn web_http_uses_documented_payloads_and_bearer_auth() -> Result<()> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            for (path, expected, reply) in [
                (
                    "/v1/tools/search",
                    json!({"query":"关键词","count":10,"need_content":false}),
                    json!({"results":[{"title":"Example","url":"https://example.com/","snippet":"Text","date":"2026-11-26"}]}),
                ),
                (
                    "/v1/tools/fetch",
                    json!({"url":"https://example.com/"}),
                    json!({"title":"Example","content":"# Text"}),
                ),
            ] {
                let (socket, _) = listener.accept().await?;
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await?;
                assert_eq!(line.trim(), format!("POST {path} HTTP/1.1"));
                let mut length = 0;
                let mut authenticated = false;
                loop {
                    line.clear();
                    socket.read_line(&mut line).await?;
                    match line.trim().to_ascii_lowercase().as_str() {
                        "" => break,
                        "authorization: bearer mock-key" => authenticated = true,
                        header if header.starts_with("content-length:") => {
                            length = header["content-length:".len()..].trim().parse()?;
                        }
                        _ => {}
                    }
                }
                assert!(authenticated);
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await?;
                assert_eq!(serde_json::from_slice::<Value>(&body)?, expected);
                let reply = reply.to_string();
                socket.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                ).as_bytes()).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let cancel = CancellationToken::new();
        let interrupt = InterruptSignal::new();
        let timeout = Duration::from_secs(3);
        let key = "mock-key";
        let search = SearchArgs {
            query: "关键词".into(),
            count: None,
        }
        .body()?;
        let response = request(
            &format!("{base}/v1/tools/search"),
            key,
            &search,
            timeout,
            &cancel,
            &interrupt,
        )
        .await?;
        let search: SearchResponse = serde_json::from_slice(&response)?;
        assert_eq!(
            search.results[0].published_at.as_deref(),
            Some("2026-11-26")
        );
        let fetch = FetchArgs {
            url: "https://example.com".into(),
        }
        .body()?;
        let response = request(
            &format!("{base}/v1/tools/fetch"),
            key,
            &fetch,
            timeout,
            &cancel,
            &interrupt,
        )
        .await?;
        let page: FetchResponse = serde_json::from_slice(&response)?;
        assert_eq!(page.content, "# Text");
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn web_request_already_cancelled_never_waits_for_http() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = request(
            "http://127.0.0.1:1",
            "mock-key",
            &json!({}),
            Duration::from_secs(1),
            &cancel,
            &InterruptSignal::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "web request interrupted");
    }
}
