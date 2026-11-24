use super::*;
use crate::bridge::read_history;

#[derive(Deserialize)]
pub(super) struct HistoryQuery {
    project: Option<String>,
    #[serde(default)]
    from_seq: u64,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    tail: bool,
    #[serde(default)]
    before_seq: Option<u64>,
    #[serde(default)]
    turns: bool,
    /// Byte ceiling for one whole-turn page; the newest turn is always kept.
    #[serde(default)]
    max_bytes: Option<usize>,
}

pub(super) fn default_limit() -> usize {
    500
}

pub(super) async fn events_history(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<HistoryQuery>,
) -> ApiResponse {
    history_endpoint(state, id, query, HistoryKind::Events).await
}

pub(super) async fn conversation_history(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<HistoryQuery>,
) -> ApiResponse {
    history_endpoint(state, id, query, HistoryKind::Conversation).await
}

#[derive(Clone, Copy)]
pub(super) enum HistoryKind {
    Events,
    Conversation,
}

pub(super) async fn history_endpoint(
    state: Arc<ApiState>,
    id: String,
    query: HistoryQuery,
    kind: HistoryKind,
) -> ApiResponse {
    if !(1..=2000).contains(&query.limit) {
        return ApiResponse::err(400, "history limit must be in 1..=2000");
    }
    if let Some(max_bytes) = query.max_bytes
        && !(1..=16 * 1024 * 1024).contains(&max_bytes)
    {
        return ApiResponse::err(400, "history max_bytes must be in 1..=16777216");
    }
    let dir = match state
        .registry
        .session_dir(&id, query.project.as_deref())
        .await
    {
        Ok(dir) => dir,
        Err(error) => return registry_error(error),
    };
    if query.turns && matches!(kind, HistoryKind::Conversation) {
        let max_bytes = query.max_bytes.unwrap_or(512 * 1024);
        return match tokio::task::spawn_blocking(move || {
            mink::runtime::session::SessionReader::new(dir).conversation_turns_bounded(
                query.from_seq,
                query.limit,
                query.tail,
                query.before_seq,
                Some(max_bytes),
            )
        })
        .await
        {
            Ok(Ok(rows)) => ApiResponse::ok(json!(rows)),
            Ok(Err(error)) => ApiResponse::err(500, error.to_string()),
            Err(error) => ApiResponse::err(500, error.to_string()),
        };
    }
    let path = dir.join(match kind {
        HistoryKind::Events => "events.jsonl",
        HistoryKind::Conversation => "conversation.jsonl",
    });
    match read_history(
        &path,
        query.from_seq,
        query.limit,
        query.tail,
        query.before_seq,
    )
    .await
    {
        Ok(events) => ApiResponse::ok(json!(events)),
        Err(error) => ApiResponse::err(500, error.to_string()),
    }
}

/// SSE：纯转手——订阅 SessionRuntime 的广播通道（AgentEvent 流），
/// 事件帧直接转发，不做轮次/seq 判断（历史由 conversation 接口提供）。
pub(super) async fn stream_events(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> Response {
    use axum::body::Body;
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
    use http_body_util::StreamBody;

    let session = match state.registry.active_runtime(&id, query.project.as_deref()) {
        Ok(Some(s)) => s,
        Err(error) => return registry_error(error).into_response(),
        Ok(None) => return ApiResponse::err(404, format!("session {id} not open")).into_response(),
    };
    let snapshot_mode = query.snapshot == Some(true);
    let (snapshot, mut rx) = if query.snapshot == Some(true) {
        let (value, rx) = session.snapshot_subscription();
        (Some(value), rx)
    } else {
        (None, session.event_receiver())
    };
    let mut shutdown = state.shutdown.subscribe();
    let stream = async_stream::stream! {
        let heartbeat_period = std::time::Duration::from_secs(30);
        // 心跳：30s 无事件时发 `: ping` 注释帧，防止中间代理按空闲超时断开
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + heartbeat_period,
            heartbeat_period,
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        if let Some(snapshot) = snapshot {
            yield Ok::<http_body::Frame<axum::body::Bytes>, std::convert::Infallible>(
                http_body::Frame::data(axum::body::Bytes::from(format!("data: {snapshot}\n\n"))),
            );
            heartbeat.reset();
        }
        loop {
            // If shutdown was already signaled before this stream subscribed,
            // close immediately instead of waiting for another watch change.
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                recv = rx.recv() => {
                    match recv {
                        Ok(event) => {
                            if !snapshot_mode
                                && event.kind == crate::session::runtime::BroadcastEventKind::Control
                            {
                                continue;
                            }
                            let sse = format!("data: {}\n\n", event.payload);
                            yield Ok::<http_body::Frame<axum::body::Bytes>, std::convert::Infallible>(
                                http_body::Frame::data(axum::body::Bytes::from(sse)),
                            );
                            heartbeat.reset();
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                            let line = serde_json::json!({"type":"stream_gap","missed":missed}).to_string();
                            yield Ok::<http_body::Frame<axum::body::Bytes>, std::convert::Infallible>(
                                http_body::Frame::data(axum::body::Bytes::from(format!("data: {line}\n\n"))),
                            );
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // 会话 runtime 已关闭：显式告知浏览器停止自动重连。
                            let line = serde_json::json!({"type":"session_closed"}).to_string();
                            yield Ok::<http_body::Frame<axum::body::Bytes>, std::convert::Infallible>(
                                http_body::Frame::data(axum::body::Bytes::from(format!("data: {line}\n\n"))),
                            );
                            break;
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    yield Ok::<http_body::Frame<axum::body::Bytes>, std::convert::Infallible>(
                        http_body::Frame::data(axum::body::Bytes::from(": ping\n\n".to_string())),
                    );
                }
            }
        }
    };
    Response::builder()
        .header(CONTENT_TYPE, "text/event-stream")
        .header(CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::new(StreamBody::new(stream)))
        .unwrap()
}

pub(super) async fn health() -> ApiResponse {
    ApiResponse::ok(json!({ "status": "ok" }))
}
