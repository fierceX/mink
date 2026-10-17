//! REST + SSE API. Unified `ApiResponse{code,message,data}` envelope.

use crate::bridge::read_history;
use crate::session::registry::Registry;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

pub struct ApiState {
    pub registry: Arc<Registry>,
    pub cwd: PathBuf,
    /// Set to `true` when the server begins graceful shutdown. SSE handlers
    /// subscribe to this sender so long-lived streams can terminate without
    /// waiting for the runtime broadcast senders to be dropped. Keeping the
    /// sender in state also prevents receiver `Closed` errors in tests where
    /// no external shutdown sender is held.
    pub shutdown: tokio::sync::watch::Sender<bool>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiResponse {
    pub code: u16,
    pub message: String,
    pub data: serde_json::Value,
}

impl ApiResponse {
    fn ok(data: serde_json::Value) -> Self {
        Self {
            code: 200,
            message: String::new(),
            data,
        }
    }

    fn err(code: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: serde_json::Value::Null,
        }
    }
}

impl IntoResponse for ApiResponse {
    fn into_response(self) -> Response {
        let status = if self.code == 200 {
            StatusCode::OK
        } else if self.code == 404 {
            StatusCode::NOT_FOUND
        } else if self.code == 400 {
            StatusCode::BAD_REQUEST
        } else if self.code == 413 {
            StatusCode::PAYLOAD_TOO_LARGE
        } else if self.code == 409 {
            StatusCode::CONFLICT
        } else if self.code == 429 {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (status, Json(self)).into_response()
    }
}

pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/api/version", get(api_version))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/sessions/{id}/open", post(open_session))
        .route("/api/sessions/{id}/close", post(close_session))
        .route("/api/sessions/{id}/turn", post(turn_session))
        .route(
            "/api/sessions/{id}/inputs",
            get(list_inputs).post(submit_input),
        )
        .route(
            "/api/sessions/{id}/inputs/{input_id}",
            axum::routing::patch(edit_input).delete(withdraw_input),
        )
        .route(
            "/api/sessions/{id}/inputs/{input_id}/resume",
            post(resume_input),
        )
        .route("/api/sessions/{id}/attachments", post(upload_attachment))
        .route(
            "/api/sessions/{id}/attachments/{attachment_id}",
            get(read_attachment),
        )
        .route("/api/sessions/{id}/interrupt", post(interrupt_session))
        .route("/api/sessions/{id}/events", get(events_history))
        .route("/api/sessions/{id}/conversation", get(conversation_history))
        .route("/api/sessions/{id}/stream", get(stream_events))
        .route("/api/sessions/{id}/plan", get(get_plan))
        .route("/api/sessions/{id}/todo", get(get_todo))
        .route("/api/sessions/{id}/goal", get(get_goal).post(create_goal))
        .route("/api/sessions/{id}/goal/{action}", post(goal_action))
        .route(
            "/api/sessions/{id}/model",
            get(get_session_model).post(set_session_model),
        )
        .route("/api/sessions/{id}/compact", post(compact_session))
        .route("/api/sessions/{id}/artifacts", get(list_artifacts))
        .route("/api/sessions/{id}/artifacts/{name}", get(get_artifact))
        .route("/api/sessions/{id}/files", get(get_files))
        .route("/health", get(health))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(state)
}

#[derive(Deserialize)]
struct CreateSessionReq {
    name: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
}

#[derive(Deserialize)]
struct TurnReq {
    input: String,
    /// Optional stable id for retries; the inbox treats a repeat as the same
    /// submission instead of starting a second turn.
    #[serde(default)]
    request_id: Option<String>,
}

#[derive(Default, Deserialize)]
struct ProjectQuery {
    project: Option<String>,
    snapshot: Option<bool>,
    request_id: Option<String>,
}

#[derive(Deserialize)]
struct HistoryQuery {
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
}

fn default_limit() -> usize {
    500
}

#[derive(Deserialize)]
struct FilesQuery {
    project: Option<String>,
    path: Option<String>,
    #[serde(default)]
    raw: bool,
}

const FILE_RAW_MAX_BYTES: u64 = 1 << 20; // 1 MiB

/// Resolve a session directory with the registry's typed status mapping.
async fn session_dir_or_err(
    state: &ApiState,
    id: &str,
    project: Option<&str>,
) -> Result<std::path::PathBuf, ApiResponse> {
    state
        .registry
        .session_dir(id, project)
        .await
        .map_err(registry_error)
}

fn registry_error(error: crate::session::registry::RegistryError) -> ApiResponse {
    use crate::session::registry::RegistryError;
    let code = match &error {
        RegistryError::NotFound(_) => 404,
        RegistryError::Ambiguous(_) | RegistryError::Locked(_) | RegistryError::Busy(_) => 409,
        RegistryError::Capacity(_) => 429,
        RegistryError::Internal(_) => 500,
    };
    ApiResponse::err(code, error.to_string())
}
async fn list_sessions(State(state): State<Arc<ApiState>>) -> ApiResponse {
    match state.registry.list().await {
        Ok(sessions) => ApiResponse::ok(json!(sessions)),
        Err(e) => ApiResponse::err(500, e.to_string()),
    }
}

/// The workbench bundle this process serves, so a stale page can tell it is stale.
///
/// The embedded `index.html` names its script with a content hash: reading that
/// name back gives the page a value to compare against the bundle it actually
/// loaded (a phone that refreshed into an older build then says so, instead of
/// looking like a half-working UI).
async fn api_version() -> ApiResponse {
    let index = crate::web_assets::FILES
        .iter()
        .find(|(path, _)| *path == "index.html")
        .map(|(_, bytes)| String::from_utf8_lossy(bytes).to_string());
    let bundle = index
        .as_deref()
        .and_then(|html| {
            let marker = "/assets/";
            let at = html.find(marker)?;
            let rest = &html[at + marker.len()..];
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        })
        .unwrap_or_default();
    ApiResponse::ok(json!({ "bundle": bundle }))
}

async fn create_session(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<CreateSessionReq>,
) -> ApiResponse {
    let cwd = req.cwd.clone().unwrap_or_else(|| state.cwd.clone());
    let name = req.name.unwrap_or_else(|| "unnamed".to_string());
    match state.registry.create(&name, &cwd).await {
        Ok(summary) => ApiResponse::ok(json!(summary)),
        Err(error) => registry_error(error),
    }
}

async fn get_session(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    match state
        .registry
        .session_dir(&id, query.project.as_deref())
        .await
    {
        Ok(_) => {
            let is_open = state
                .registry
                .is_open(&id, query.project.as_deref())
                .unwrap_or(false);
            let running = state
                .registry
                .running(&id, query.project.as_deref())
                .unwrap_or(false);
            let mut detail = json!({ "id": id, "open": is_open, "running": running });
            if let Ok(Some(runtime)) = state.registry.active_runtime(&id, query.project.as_deref())
                && let Some(fields) = runtime.detail().as_object()
            {
                for (key, value) in fields {
                    detail[key] = value.clone();
                }
            }
            ApiResponse::ok(detail)
        }
        Err(error) => registry_error(error),
    }
}

async fn delete_session(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    match state.registry.delete(&id, query.project.as_deref()).await {
        Ok(()) => ApiResponse::ok(json!({ "id": id, "deleted": true })),
        Err(error) => registry_error(error),
    }
}

async fn open_session(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    match state.registry.open(&id, query.project.as_deref()).await {
        Ok(_) => ApiResponse::ok(json!({ "id": id, "status": "active", "snapshot": true })),
        Err(error) => registry_error(error),
    }
}

async fn close_session(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    match state.registry.close(&id, query.project.as_deref()).await {
        Ok(_) => ApiResponse::ok(json!({ "id": id, "status": "free" })),
        Err(error) => registry_error(error),
    }
}

async fn turn_session(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
    Json(req): Json<TurnReq>,
) -> ApiResponse {
    let input = req.input.trim().to_string();
    if input.is_empty() {
        return ApiResponse::err(400, "input must not be empty");
    }
    if input.len() > 128 * 1024 {
        return ApiResponse::err(400, "input too large (max 128 KiB)");
    }
    let request_id = match req.request_id {
        Some(id) if id.trim().is_empty() => {
            return ApiResponse::err(400, "request_id must not be empty");
        }
        Some(id) if id.len() > 256 => {
            return ApiResponse::err(400, "request_id too long (max 256 bytes)");
        }
        other => other,
    };
    match state
        .registry
        .start_turn(&id, query.project.as_deref(), input, request_id)
    {
        Ok(_) => ApiResponse::ok(json!({ "id": id, "status": "running" })),
        Err(error) => registry_error(error),
    }
}

fn runtime_or_err(
    state: &ApiState,
    id: &str,
    project: Option<&str>,
) -> Result<Arc<crate::session::runtime::SessionRuntime>, ApiResponse> {
    state
        .registry
        .active_runtime(id, project)
        .map_err(registry_error)?
        .filter(|runtime| !runtime.closed())
        .ok_or_else(|| ApiResponse::err(409, "session is not open"))
}
fn input_query(
    entries: Vec<mink::runtime::InputReceipt>,
    request_id: Option<&str>,
) -> Vec<mink::runtime::InputReceipt> {
    entries
        .into_iter()
        .filter(|entry| {
            request_id.map_or(
                matches!(
                    entry.status,
                    mink::runtime::InputStatus::Pending
                        | mink::runtime::InputStatus::Applying
                        | mink::runtime::InputStatus::Unapplied
                ),
                |id| entry.input_id == id,
            )
        })
        .collect()
}
async fn list_inputs(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    if let Ok(runtime) = runtime_or_err(&state, &id, query.project.as_deref()) {
        return ApiResponse::ok(json!(input_query(
            runtime.inbox().entries(),
            query.request_id.as_deref()
        )));
    }
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(d) => d,
        Err(e) => return e,
    };
    match tokio::task::spawn_blocking(move || {
        mink::runtime::session::SessionReader::new(dir).inputs()
    })
    .await
    {
        Ok(Ok(inputs)) => ApiResponse::ok(json!(input_query(inputs, query.request_id.as_deref()))),
        result => ApiResponse::err(500, format!("cannot read inputs: {result:?}")),
    }
}
async fn submit_input(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
    Json(mut input): Json<mink::runtime::HumanInput>,
) -> ApiResponse {
    let runtime = match runtime_or_err(&state, &id, query.project.as_deref()) {
        Ok(r) => r,
        Err(e) => return e,
    };
    if input.attachment_ids.len() > 8 {
        return ApiResponse::err(400, "max 8 attachments");
    }
    if !input.attachment_ids.is_empty() {
        let Some(limits) = runtime.image_input().limits() else {
            return ApiResponse::err(400, "image input unavailable for this session");
        };
        let mut total = 0u64;
        let mut paths = Vec::new();
        for attachment in &input.attachment_ids {
            match runtime
                .attachments()
                .read(attachment, limits.max_image_bytes)
            {
                Ok((path, bytes)) => {
                    total += bytes.len() as u64;
                    paths.push(path);
                }
                Err(e) => return ApiResponse::err(400, e.to_string()),
            }
        }
        if input.attachment_ids.len() > limits.max_images_per_request
            || total > limits.max_image_bytes_per_request.min(16 * 1024 * 1024)
        {
            return ApiResponse::err(413, "attachments exceed session limits");
        }
        if input.text.trim().is_empty() {
            input.text =
                "Please use Read to inspect the attached images and respond to their contents."
                    .into();
        }
        for path in paths {
            input
                .text
                .push_str(&format!("\n[Attached image: \"{}\"]", path.display()));
        }
    }
    match state
        .registry
        .submit_input(&id, query.project.as_deref(), input, None)
    {
        Ok(receipt) => ApiResponse::ok(json!(receipt)),
        Err(e) => registry_error(e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputEdit {
    revision: u64,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    revision: u64,
}
async fn edit_input(
    State(state): State<Arc<ApiState>>,
    AxumPath((id, input_id)): AxumPath<(String, String)>,
    Query(query): Query<ProjectQuery>,
    Json(edit): Json<InputEdit>,
) -> ApiResponse {
    update_input(
        &state,
        &id,
        &input_id,
        query.project.as_deref(),
        edit.revision,
        Some(edit.text),
    )
}
async fn withdraw_input(
    State(state): State<Arc<ApiState>>,
    AxumPath((id, input_id)): AxumPath<(String, String)>,
    Query(query): Query<ProjectQuery>,
    Json(edit): Json<Revision>,
) -> ApiResponse {
    update_input(
        &state,
        &id,
        &input_id,
        query.project.as_deref(),
        edit.revision,
        None,
    )
}
fn update_input(
    state: &ApiState,
    id: &str,
    input_id: &str,
    project: Option<&str>,
    revision: u64,
    text: Option<String>,
) -> ApiResponse {
    let runtime = match runtime_or_err(state, id, project) {
        Ok(r) => r,
        Err(e) => return e,
    };
    match runtime.inbox().update(input_id, revision, text) {
        Ok(receipt) => {
            runtime.publish_inputs();
            ApiResponse::ok(json!(receipt))
        }
        Err(e) => ApiResponse::err(409, e.to_string()),
    }
}
async fn resume_input(
    State(state): State<Arc<ApiState>>,
    AxumPath((id, input_id)): AxumPath<(String, String)>,
    Query(query): Query<ProjectQuery>,
    Json(edit): Json<Revision>,
) -> ApiResponse {
    let runtime = match runtime_or_err(&state, &id, query.project.as_deref()) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let Some(receipt) = runtime
        .inbox()
        .entries()
        .into_iter()
        .find(|e| e.input_id == input_id)
    else {
        return ApiResponse::err(404, "input not found");
    };
    match state.registry.submit_input(
        &id,
        query.project.as_deref(),
        receipt.input,
        Some((&input_id, edit.revision)),
    ) {
        Ok(r) => ApiResponse::ok(json!(r)),
        Err(e) => registry_error(e),
    }
}
async fn upload_attachment(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
    bytes: axum::body::Bytes,
) -> ApiResponse {
    let runtime = match runtime_or_err(&state, &id, query.project.as_deref()) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let Some(limits) = runtime.image_input().limits().cloned() else {
        return ApiResponse::err(400, "image input unavailable for this session");
    };
    let store = runtime.attachments().clone();
    match tokio::task::spawn_blocking(move || store.upload(&bytes, &limits)).await {
        Ok(Ok(a)) => ApiResponse::ok(json!(a)),
        Ok(Err(e)) => ApiResponse::err(400, e.to_string()),
        Err(e) => ApiResponse::err(500, e.to_string()),
    }
}
async fn read_attachment(
    State(state): State<Arc<ApiState>>,
    AxumPath((id, attachment_id)): AxumPath<(String, String)>,
    Query(query): Query<ProjectQuery>,
) -> Response {
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(d) => d,
        Err(e) => return e.into_response(),
    };
    let result = tokio::task::spawn_blocking(move || {
        let (_, bytes) = mink::runtime::AttachmentStore::new(dir.join("attachments"))
            .read(&attachment_id, 16 * 1024 * 1024)?;
        let info =
            mink::runtime::probe_image(&bytes).ok_or_else(|| anyhow::anyhow!("invalid image"))?;
        Ok::<_, anyhow::Error>((info.mime(), bytes))
    })
    .await;
    match result {
        Ok(Ok((mime, bytes))) => (
            [
                (axum::http::header::CONTENT_TYPE, mime),
                (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (axum::http::header::CACHE_CONTROL, "private, max-age=3600"),
            ],
            bytes,
        )
            .into_response(),
        _ => ApiResponse::err(404, "attachment unavailable").into_response(),
    }
}

async fn interrupt_session(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    match state.registry.interrupt(&id, query.project.as_deref()) {
        Ok(_) => ApiResponse::ok(json!({ "id": id, "interrupted": true })),
        Err(error) => registry_error(error),
    }
}

#[derive(Deserialize)]
struct ModelBody {
    /// An alias such as `flash`/`pro` or any real model name.
    model: String,
}

/// Switch the session's active model, mirroring the terminal's `/model`.
/// The session's model vocabulary plus the configured model.
///
/// The label a UI shows comes from the title-update path (and from this route's
/// POST companion after a switch), because a switch is owned by the running
/// orchestrator rather than by the configuration these fields describe.
async fn get_session_model(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    let runtime = match state.registry.active_runtime(&id, query.project.as_deref()) {
        Ok(Some(runtime)) => runtime,
        Ok(None) => return ApiResponse::err(409, "session is not open"),
        Err(error) => return registry_error(error),
    };
    let Some(handle) = runtime.handle() else {
        return ApiResponse::err(409, "session runtime is shutting down");
    };
    let choices = handle
        .model_choices()
        .into_iter()
        .map(|(alias, model)| json!({ "alias": alias, "model": model }))
        .collect::<Vec<_>>();
    ApiResponse::ok(json!({
        "model": handle.current_model(),
        "resolved": handle.resolved_model(),
        "choices": choices,
    }))
}

async fn set_session_model(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
    Json(body): Json<ModelBody>,
) -> ApiResponse {
    let model = body.model.trim().to_string();
    if model.is_empty() {
        return ApiResponse::err(400, "model is required");
    }
    let runtime = match state.registry.active_runtime(&id, query.project.as_deref()) {
        Ok(Some(runtime)) => runtime,
        Ok(None) => return ApiResponse::err(404, "session is not open"),
        Err(error) => return registry_error(error),
    };
    let Some(handle) = runtime.handle() else {
        return ApiResponse::err(409, "session runtime is shutting down");
    };
    match handle.set_model_with_outcome(model.clone()).await {
        Ok(outcome) => ApiResponse::ok(json!({
            "model": model,
            "label": outcome.label,
            "stats": outcome.stats,
        })),
        Err(error) => ApiResponse::err(409, switch_error(&error)),
    }
}

/// Manual context compaction, mirroring the terminal's `/compact`.
async fn compact_session(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    let runtime = match state.registry.active_runtime(&id, query.project.as_deref()) {
        Ok(Some(runtime)) => runtime,
        Ok(None) => return ApiResponse::err(404, "session is not open"),
        Err(error) => return registry_error(error),
    };
    let Some(handle) = runtime.handle() else {
        return ApiResponse::err(409, "session runtime is shutting down");
    };
    match handle.compact().await {
        Ok(outcome) => ApiResponse::ok(json!({ "compacted": true, "outcome": outcome })),
        Err(error) => ApiResponse::err(409, switch_error(&error)),
    }
}

/// Report why a runtime control was refused in language the workbench can show.
///
/// A raw core error surfaces English identifiers in an otherwise Chinese
/// interface, and the actionable ones need to say what to do next.
fn switch_error(error: &mink::runtime::RuntimeError) -> String {
    let text = format!("{error}");
    if text.contains("goal") && text.contains("active") {
        "当前目标执行中，先暂停目标再切换".to_string()
    } else if text.contains("max_context_tokens") || text.to_lowercase().contains("disabled") {
        "该会话未启用上下文压缩".to_string()
    } else if text.contains("busy") || text.contains("turn") {
        "本轮尚未结束，请稍后重试".to_string()
    } else {
        text
    }
}

async fn events_history(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<HistoryQuery>,
) -> ApiResponse {
    history_endpoint(state, id, query, HistoryKind::Events).await
}

async fn conversation_history(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<HistoryQuery>,
) -> ApiResponse {
    history_endpoint(state, id, query, HistoryKind::Conversation).await
}

#[derive(Clone, Copy)]
enum HistoryKind {
    Events,
    Conversation,
}

async fn history_endpoint(
    state: Arc<ApiState>,
    id: String,
    query: HistoryQuery,
    kind: HistoryKind,
) -> ApiResponse {
    if !(1..=2000).contains(&query.limit) {
        return ApiResponse::err(400, "history limit must be in 1..=2000");
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
        return match tokio::task::spawn_blocking(move || {
            mink::runtime::session::SessionReader::new(dir).conversation_turns(
                query.from_seq,
                query.limit,
                query.tail,
                query.before_seq,
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
async fn stream_events(
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
        if let Some(snapshot)=snapshot {
            yield Ok::<http_body::Frame<axum::body::Bytes>, std::convert::Infallible>(http_body::Frame::data(axum::body::Bytes::from(format!("data: {snapshot}\n\n"))));
        }
        // 心跳：30s 无事件时发 `: ping` 注释帧，防止中间代理按空闲超时断开
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));
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
                        Ok(line) => {
                            if !snapshot_mode && serde_json::from_str::<serde_json::Value>(&line).ok().is_some_and(|v| matches!(v["type"].as_str(), Some("inputs_updated" | "conversation_committed" | "phase_updated"))) { continue; }
                            let sse = format!("data: {line}\n\n");
                            yield Ok::<http_body::Frame<axum::body::Bytes>, std::convert::Infallible>(
                                http_body::Frame::data(axum::body::Bytes::from(sse)),
                            );
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

async fn health() -> ApiResponse {
    ApiResponse::ok(json!({ "status": "ok" }))
}

async fn get_plan(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let plan = read_optional(&dir.join("plan.md")).await;
    let draft = read_optional(&dir.join("plan.draft")).await;
    ApiResponse::ok(json!({ "plan": plan, "draft": draft }))
}

async fn get_todo(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let todo = read_optional(&dir.join("todos.json")).await;
    match todo {
        Some(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => ApiResponse::ok(json!({ "todos": v })),
            Err(e) => ApiResponse::err(500, format!("todos.json parse error: {e}")),
        },
        None => ApiResponse::ok(json!({ "todos": null })),
    }
}

/// Read the goal, preferring the live runtime over the persisted file so a
/// session the server is driving reports its real state instead of a stale disk
/// read. Mutations go through the same runtime handle, so the core contract —
/// lease ownership, revision checks and acceptance rules — stays authoritative.
async fn get_goal(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    if let Ok(Some(runtime)) = state.registry.active_runtime(&id, query.project.as_deref())
        && let Some(goal) = runtime.goal_snapshot()
    {
        // A cleared (hidden) goal reads as null here exactly like it does in
        // the session snapshot, the goal_updated event and the disk fallback:
        // one normalization rule for every read surface.
        let goal = (!goal.hidden).then_some(goal);
        return ApiResponse::ok(
            json!({ "goal": goal, "driven": true, "verifier": state.registry.verifier_configured() }),
        );
    }
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let Some(text) = read_optional(&dir.join("goal.json")).await else {
        return ApiResponse::ok(
            json!({ "goal": serde_json::Value::Null, "driven": false, "verifier": state.registry.verifier_configured() }),
        );
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => {
            // `goal.json` stores {version, current, history}; the workbench only
            // needs the live snapshot. A cleared (hidden) goal is normalized to
            // null so the persisted path matches the live read.
            let goal = value
                .get("current")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let goal = if goal.get("hidden").and_then(serde_json::Value::as_bool) == Some(true) {
                serde_json::Value::Null
            } else {
                goal
            };
            ApiResponse::ok(
                json!({ "goal": goal, "driven": false, "verifier": state.registry.verifier_configured() }),
            )
        }
        Err(e) => ApiResponse::err(500, format!("goal.json parse error: {e}")),
    }
}

#[derive(serde::Deserialize)]
struct CreateGoalBody {
    #[serde(default)]
    protocol_version: Option<u32>,
    #[serde(default)]
    generate: Option<bool>,
    #[serde(default)]
    draft_spec: Option<mink::runtime::GoalDraftSpec>,
    objective: String,
    /// `agent_reviewed` (default), `verifier` or `human`.
    #[serde(default)]
    acceptance: String,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    max_minutes: Option<u64>,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    constraints: Vec<String>,
}

/// Create a draft on an open session; explicit confirmation activates execution.
///
/// The browser is a host here, not the model: limits and acceptance mode are
/// validated by the core contract, and the model still cannot change them.
async fn create_goal(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
    body: Option<axum::Json<CreateGoalBody>>,
) -> ApiResponse {
    let Some(body) = body else {
        return ApiResponse::err(400, "goal body is required");
    };
    let runtime = match state.registry.active_runtime(&id, query.project.as_deref()) {
        Ok(Some(runtime)) => runtime,
        Ok(None) => {
            return ApiResponse::err(409, "session is not open; open it before creating a goal");
        }
        Err(error) => return registry_error(error),
    };
    let handle = match runtime.goal_handle() {
        Some(handle) => handle,
        None => return ApiResponse::err(409, "session runtime is closing"),
    };
    if body.protocol_version != Some(2) {
        return ApiResponse::err(
            409,
            "goal protocol upgraded; protocol_version 2 and draft confirmation are required",
        );
    }
    let objective = body.objective.trim();
    if objective.is_empty() {
        return ApiResponse::err(400, "goal objective is empty");
    }
    let mut spec = mink::runtime::GoalSpec::new(objective);
    if !body.acceptance_criteria.is_empty() {
        spec.acceptance_criteria = body.acceptance_criteria.clone();
        spec.criteria = body
            .acceptance_criteria
            .iter()
            .enumerate()
            .map(|(i, outcome)| mink::runtime::GoalCriterion {
                id: format!("criterion-{}", i + 1),
                subgoal_id: "subgoal-1".into(),
                outcome: outcome.clone(),
                required_evidence: "Concrete files or recorded verification results".into(),
            })
            .collect();
    }
    spec.constraints = body.constraints.clone();
    spec.acceptance = match body.acceptance.as_str() {
        "human" => mink::runtime::GoalAcceptance::Human,
        "verifier" => match handle.goal_verifier_ids().into_iter().next() {
            Some(id) => mink::runtime::GoalAcceptance::Verifier { id },
            None => return ApiResponse::err(409, "no goal verifier is configured"),
        },
        "agent_reviewed" | "" => mink::runtime::GoalAcceptance::AgentReviewed,
        other => return ApiResponse::err(400, format!("unknown acceptance mode {other}")),
    };
    if let Some(draft) = &body.draft_spec {
        spec.objective = draft.objective.clone();
        spec.scope = draft.scope.clone();
        spec.constraints = draft.constraints.clone();
        spec.subgoals = draft.subgoals.clone();
        spec.criteria = draft.criteria.clone();
        spec.suggested_actions = draft.suggested_actions.clone();
        spec.acceptance_criteria = spec.criteria.iter().map(|c| c.outcome.clone()).collect();
    }
    spec.checker_id = handle.goal_verifier_ids().into_iter().next();
    let mut limits = spec.limits.clone();
    if let Some(turns) = body.max_turns {
        limits.max_turns = turns;
    }
    if let Some(minutes) = body.max_minutes {
        limits.deadline_unix_secs = now_unix().saturating_add(minutes.saturating_mul(60));
    }
    spec.limits = limits;
    if let Err(error) = spec.validate() {
        return ApiResponse::err(400, format!("{error:#}"));
    }
    // Arm the driver before creating: a goal created while scheduling is
    // disabled would sit idle until the next explicit start. Arming never
    // starts a run by itself — the leased activation below is the single
    // owner of the run — so a refused create cannot leave an unleased run
    // behind. The slot is taken first — goal turns never pass through
    // `submit_input`, so without it a goal would run entirely outside
    // `max_running`. Commands are serialised per session so a failure can
    // release exactly the slot this attempt added, even under concurrent
    // creations.
    let _command = match state
        .registry
        .lock_goal_commands(&id, query.project.as_deref())
        .await
    {
        Ok(guard) => guard,
        Err(error) => return registry_error(error),
    };
    // An existing unfinished goal is refused before any capacity is taken and
    // before the driver is armed, so a refused create never starts a goal the
    // request was not about.
    if handle
        .goal_snapshot()
        .is_some_and(|goal| !goal.status.is_terminal())
    {
        return ApiResponse::err(409, "session already has an unfinished goal");
    }
    let lease = match state
        .registry
        .reserve_goal_run(&id, query.project.as_deref())
        .await
    {
        Ok(lease) => lease,
        Err(error) => return registry_error(error),
    };
    let duration_secs = body.max_minutes.unwrap_or(60).saturating_mul(60);
    let proposed = if body.generate == Some(false) {
        handle.draft_goal(spec, duration_secs)
    } else {
        handle.request_goal_draft(spec, duration_secs)
    };
    let draft = match proposed {
        Ok(draft) => draft,
        Err(error) => return ApiResponse::err(409, error.to_string()),
    };
    if body.generate == Some(false) {
        return ApiResponse::ok(json!({"goal":draft}));
    }
    drop(_command);
    let task = tokio::spawn(async move {
        let _lease = lease;
        handle
            .generate_goal_draft_at(&draft.id, draft.revision, "")
            .await
    });
    match task.await {
        Ok(Ok(draft)) => ApiResponse::ok(json!({"goal":draft})),
        other => ApiResponse::err(
            409,
            format!("draft generation failed; proposal preserved: {other:?}"),
        ),
    }
}

#[derive(serde::Deserialize)]
struct GoalActionBody {
    #[serde(default)]
    feedback: Option<String>,
    /// Exact goal the caller believes it is acting on.
    #[serde(default)]
    goal_id: Option<String>,
    #[serde(default)]
    revision: Option<u64>,
    /// Human evidence for `approve` and `reconcile`.
    #[serde(default)]
    evidence: Option<String>,
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    max_minutes: Option<u64>,
    #[serde(default)]
    soft_token_limit: Option<u64>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pause, resume, cancel, clear, re-budget, approve or reconcile a goal.
///
/// Every mutation is a compare-and-set on the caller's revision, so a stale
/// workbench cannot undo a change another surface already made.
async fn goal_action(
    State(state): State<Arc<ApiState>>,
    AxumPath((id, action)): AxumPath<(String, String)>,
    Query(query): Query<ProjectQuery>,
    body: Option<axum::Json<GoalActionBody>>,
) -> ApiResponse {
    let runtime = match state.registry.active_runtime(&id, query.project.as_deref()) {
        Ok(Some(runtime)) => runtime,
        Ok(None) => return ApiResponse::err(409, "session is not open"),
        Err(error) => return registry_error(error),
    };
    let handle = match runtime.goal_handle() {
        Some(handle) => handle,
        None => return ApiResponse::err(409, "session runtime is closing"),
    };
    let body = match body {
        Some(axum::Json(body)) => body,
        None => GoalActionBody {
            feedback: None,
            goal_id: None,
            revision: None,
            evidence: None,
            max_turns: None,
            max_minutes: None,
            soft_token_limit: None,
        },
    };
    let current = match handle.goal_snapshot() {
        Some(goal) => goal,
        None => return ApiResponse::err(404, "session has no goal"),
    };
    let Some(revision) = body.revision else {
        return ApiResponse::err(400, "revision is required for goal control");
    };
    // The client's expected identity is mandatory. Revisions restart at 1 for
    // every goal, so a stale request for a replaced goal could otherwise match
    // the replacement's revision and cancel it.
    match body.goal_id.as_deref() {
        None => return ApiResponse::err(400, "goal_id is required for goal control"),
        Some(expected) if expected != current.id => {
            return ApiResponse::err(409, "goal was replaced; reread it before acting");
        }
        Some(_) => {}
    }
    let result = match action.as_str() {
        "refine" => {
            if current.status != mink::runtime::GoalStatus::Draft || current.revision != revision {
                return ApiResponse::err(409, "stale draft; reread before refining");
            }
            let lease = match state
                .registry
                .reserve_goal_run(&id, query.project.as_deref())
                .await
            {
                Ok(lease) => lease,
                Err(error) => return registry_error(error),
            };
            let feedback = body.feedback.unwrap_or_default();
            let task = tokio::spawn(async move {
                let _lease = lease;
                handle
                    .generate_goal_draft_at(&current.id, revision, &feedback)
                    .await
            });
            return match task.await {
                Ok(Ok(goal)) => ApiResponse::ok(json!({"goal":goal})),
                other => ApiResponse::err(409, format!("draft refinement failed: {other:?}")),
            };
        }
        "pause" => {
            state
                .registry
                .mark_goal_draining(&id, query.project.as_deref());
            handle.pause_goal(&current.id, revision)
        }
        "confirm" | "resume" => {
            // A reopened session starts with scheduling disabled, so an
            // explicit resume has to arm the driver or the goal sits active
            // and never runs. Arming alone starts no run: the leased
            // activation below is the single owner of the run, so a refused
            // resume cannot issue provider requests or leak capacity. Capacity
            // is checked before it can start, and a refused resume releases
            // only a slot this attempt added.
            let _command = match state
                .registry
                .lock_goal_commands(&id, query.project.as_deref())
                .await
            {
                Ok(guard) => guard,
                Err(error) => return registry_error(error),
            };
            let lease = match state
                .registry
                .reserve_goal_run(&id, query.project.as_deref())
                .await
            {
                Ok(lease) => lease,
                Err(error) => return registry_error(error),
            };
            if let Err(error) = handle.start_goal_driver() {
                // The lease is dropped here: the driver could not be armed.
                return ApiResponse::err(409, format!("{error}"));
            }
            if action == "confirm" {
                handle.confirm_and_start_goal(&current.id, revision, Box::new(lease))
            } else {
                handle.resume_and_start_goal(&current.id, revision, Box::new(lease))
            }
        }
        "cancel" => {
            state
                .registry
                .mark_goal_draining(&id, query.project.as_deref());
            handle.cancel_goal(&current.id, revision)
        }
        "clear" => handle.clear_goal(&current.id, revision),
        "approve" => handle.approve_goal(
            &current.id,
            revision,
            vec![body.evidence.unwrap_or_default()],
        ),
        "reconcile" => {
            handle.reconcile_goal_effects(&current.id, revision, body.evidence.unwrap_or_default())
        }
        "budget" => {
            let mut limits = current.spec.limits.clone();
            if let Some(turns) = body.max_turns {
                limits.max_turns = turns;
            }
            if let Some(minutes) = body.max_minutes {
                limits.deadline_unix_secs = now_unix().saturating_add(minutes.saturating_mul(60));
            }
            if body.soft_token_limit.is_some() {
                limits.soft_token_limit = body.soft_token_limit;
            }
            handle.update_goal_limits(&current.id, revision, limits)
        }
        other => return ApiResponse::err(404, format!("unknown goal action {other}")),
    };
    match result {
        Ok(goal) => ApiResponse::ok(json!({ "goal": goal })),
        Err(error) => ApiResponse::err(409, format!("{error}")),
    }
}

async fn list_artifacts(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let artifacts_dir = dir.join("artifacts");
    let index = read_optional(&artifacts_dir.join("index.jsonl")).await;
    let mut records = Vec::new();
    if let Some(index) = index {
        for line in index.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                records.push(v);
            }
        }
    }
    ApiResponse::ok(json!({ "artifacts": records }))
}

async fn get_artifact(
    State(state): State<Arc<ApiState>>,
    AxumPath((id, name)): AxumPath<(String, String)>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    // `artifact://<id>` maps to artifacts/<id>.txt; also accept the raw
    // filename. Reject anything that could escape the artifacts directory.
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return ApiResponse::err(400, "invalid artifact name");
    }
    let filename = if name.ends_with(".txt") {
        name.clone()
    } else {
        format!("{name}.txt")
    };
    let path = dir.join("artifacts").join(&filename);
    // artifact 正文可达 MB 级（超长工具输出落盘）：与 /files?raw 相同的
    // 读取上限，防止内存放大。
    match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.len() > FILE_RAW_MAX_BYTES => {
            return ApiResponse::err(
                413,
                format!(
                    "artifact {name} is {} bytes; the REST API caps reads at {FILE_RAW_MAX_BYTES} bytes",
                    meta.len()
                ),
            );
        }
        _ => {}
    }
    match read_optional(&path).await {
        Some(text) => ApiResponse::ok(json!({ "name": filename, "content": text })),
        None => ApiResponse::err(404, format!("artifact {name} not found")),
    }
}

async fn get_files(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<FilesQuery>,
) -> ApiResponse {
    if let Err(response) = session_dir_or_err(&state, &id, query.project.as_deref()).await {
        return response;
    }
    // cwd 是会话工作目录（SessionMetadata.cwd），文件浏览限制在其内。
    let cwd = match state
        .registry
        .session_metadata_cwd(&id, query.project.as_deref())
        .await
    {
        Ok(Some(c)) => std::path::PathBuf::from(c),
        Ok(None) => return ApiResponse::err(500, "session metadata cwd is missing"),
        Err(error) => return ApiResponse::err(500, format!("failed to read session cwd: {error}")),
    };
    let rel = query.path.unwrap_or_default();
    let target = {
        let cwd = cwd.clone();
        let rel = rel.clone();
        match tokio::task::spawn_blocking(move || resolve_within(&cwd, &rel)).await {
            Ok(target) => target,
            Err(error) => return ApiResponse::err(500, format!("path task failed: {error}")),
        }
    };
    let Some(target) = target else {
        return ApiResponse::err(400, "path escapes the workspace");
    };

    if query.raw {
        let meta = match tokio::fs::metadata(&target).await {
            Ok(m) => m,
            Err(_) => return ApiResponse::err(404, "file not found"),
        };
        if !meta.is_file() {
            return ApiResponse::err(400, "not a file");
        }
        if meta.len() > FILE_RAW_MAX_BYTES {
            return ApiResponse::err(
                413,
                format!("file too large ({} bytes, limit 1 MiB)", meta.len()),
            );
        }
        match tokio::fs::read_to_string(&target).await {
            Ok(text) => ApiResponse::ok(json!({ "path": rel, "content": text })),
            Err(e) => ApiResponse::err(500, format!("read failed: {e}")),
        }
    } else {
        let mut entries = match tokio::fs::read_dir(&target).await {
            Ok(entries) => entries,
            Err(_) => return ApiResponse::err(404, "directory not found"),
        };
        let mut items = Vec::new();
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(error) => {
                    return ApiResponse::err(500, format!("directory read failed: {error}"));
                }
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            items.push(json!({ "name": name, "dir": is_dir }));
        }
        items.sort_by(|a, b| {
            let da = a["dir"].as_bool().unwrap_or(false);
            let db = b["dir"].as_bool().unwrap_or(false);
            db.cmp(&da)
                .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
        });
        ApiResponse::ok(json!({ "path": rel, "dir": true, "items": items }))
    }
}

async fn read_optional(path: &std::path::Path) -> Option<String> {
    tokio::fs::read_to_string(path).await.ok()
}

/// Join `rel` onto `root`, refusing any path that escapes `root`.
fn resolve_within(root: &std::path::Path, rel: &str) -> Option<std::path::PathBuf> {
    let rel_path = std::path::Path::new(rel);
    let joined = root.join(rel_path);
    let canonical_root = std::fs::canonicalize(root).ok()?;
    // 目标可能不存在（目录浏览场景 target 是已存在目录；raw 场景是文件）。
    // 对已存在路径做 canonicalize 校验；不存在的路径按组件词法校验。
    if let Ok(canonical) = std::fs::canonicalize(&joined) {
        return canonical.starts_with(&canonical_root).then_some(canonical);
    }
    if rel_path.is_absolute() {
        return None;
    }
    let mut components = std::path::PathBuf::from(&canonical_root);
    for comp in rel_path.components() {
        match comp {
            std::path::Component::Normal(c) => components.push(c),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(components)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use http_body_util::BodyExt;
    use std::path::PathBuf;
    use tower::ServiceExt;

    struct MockRuntimeBackend;

    #[async_trait::async_trait]
    impl mink::runtime::LlmBackend for MockRuntimeBackend {
        fn name(&self) -> &str {
            "server-api-mock"
        }

        async fn stream(
            &self,
            _request: mink::runtime::LlmRequest,
        ) -> anyhow::Result<mink::runtime::LlmResponseStream> {
            Ok(mink::runtime::LlmResponseStream {
                events: Box::pin(futures::stream::iter(vec![
                    Ok(mink::runtime::LlmEvent::Text(mink::runtime::LlmTextEvent {
                        content: "mock api answer".into(),
                    })),
                    Ok(mink::runtime::LlmEvent::Stop(mink::runtime::LlmStopEvent {
                        reason: "end_turn".into(),
                    })),
                ])),
                attempt_count: 1,
            })
        }
    }

    /// 临时 home + 一个含 3 行 conversation 的会话 + 1 个 artifact。
    /// 每个调用使用唯一 home（进程内并行测试共享同一 pid 的固定目录会
    /// 互相覆盖 artifact/会话文件，造成偶发竞态失败）。
    fn test_router() -> Router {
        test_router_with_session_dir().0
    }

    /// Same fixture, but also hands back the session directory so a test can
    /// seed authoritative state files before requesting them.
    fn test_router_with_session_dir() -> (Router, PathBuf) {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let home = std::env::temp_dir().join(format!(
            "mink-server-itest-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let sess = home
            .join(".mink")
            .join("projects")
            .join("proj")
            .join("test-session");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(
            sess.join("session.json"),
            serde_json::json!({
                "id": "test-session", "alias": null, "title": "t", "cwd": sess.display().to_string(),
                "created_at": "", "updated_at": "", "parent": null, "first_prompt": null, "summary": null
            }).to_string(),
        ).unwrap();
        std::fs::write(
            sess.join("conversation.jsonl"),
            "{\"role\":\"user\",\"content\":\"one\"}\n{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"two\"}]}\n{\"role\":\"user\",\"content\":\"three\"}\n",
        ).unwrap();
        std::fs::create_dir_all(sess.join("artifacts")).unwrap();
        std::fs::write(sess.join("artifacts/abc.txt"), "artifact body").unwrap();

        let registry = Arc::new(Registry::new(home, "flash".to_string(), 4));
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let state = Arc::new(ApiState {
            registry,
            cwd: std::env::temp_dir(),
            shutdown,
        });
        (router(state), sess)
    }

    #[tokio::test]
    async fn goal_endpoint_reports_the_persisted_snapshot_or_null() {
        let (app, dir) = test_router_with_session_dir();
        // No goal.json yet: the workbench marker must read as "no goal".
        let (status, body) = req(&app, Method::GET, "/api/sessions/test-session/goal").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["data"]["goal"].is_null(),
            "sessions without a goal report null, got {body}"
        );

        // goal.json wraps the snapshot in {version, current, history}; only
        // `current` is mirrored.
        std::fs::write(
            dir.join("goal.json"),
            serde_json::json!({
                "version": 1,
                "current": {
                    "id": "goal-1", "status": "active", "phase": "executing",
                    "remaining_turns": 12, "hidden": false
                },
                "history": []
            })
            .to_string(),
        )
        .unwrap();
        let (status, body) = req(&app, Method::GET, "/api/sessions/test-session/goal").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["goal"]["id"], "goal-1", "{body}");
        assert_eq!(body["data"]["goal"]["status"], "active", "{body}");
        assert_eq!(body["data"]["goal"]["remaining_turns"], 12, "{body}");
        assert!(
            body["data"]["goal"].get("history").is_none(),
            "history is not mirrored: {body}"
        );

        // A cleared (hidden) goal is absent on the persisted path too: the
        // workbench must read the same "no goal" as a live runtime reports.
        std::fs::write(
            dir.join("goal.json"),
            serde_json::json!({
                "version": 1,
                "current": {
                    "id": "goal-1", "status": "cancelled", "phase": "idle",
                    "hidden": true
                },
                "history": []
            })
            .to_string(),
        )
        .unwrap();
        let (status, body) = req(&app, Method::GET, "/api/sessions/test-session/goal").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["data"]["goal"].is_null(),
            "a cleared goal must not leak through the disk fallback: {body}"
        );
    }

    #[tokio::test]
    async fn goal_endpoint_normalizes_cleared_goals_to_null() {
        let (app, home, sid) = open_goal_router().await;
        let g = |suffix: &str| format!("/api/sessions/{sid}{suffix}");

        let (status, body) = post_json(
            &app,
            &g("/goal"),
            serde_json::json!({
                "objective": "clear me",
                "protocol_version": 2,
                "generate": false
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let id = body["data"]["goal"]["id"].as_str().unwrap().to_string();
        let revision = body["data"]["goal"]["revision"].as_u64().unwrap();

        let (status, body) = post_json(
            &app,
            &g("/goal/cancel"),
            serde_json::json!({"goal_id": id, "revision": revision}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["goal"]["status"], "cancelled", "{body}");
        let revision = body["data"]["goal"]["revision"].as_u64().unwrap();

        let (status, body) = post_json(
            &app,
            &g("/goal/clear"),
            serde_json::json!({"goal_id": id, "revision": revision}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["goal"]["hidden"], true, "{body}");

        // The live runtime still drives the session, but the cleared goal is
        // gone from every read surface.
        let (status, body) = req(&app, Method::GET, &g("/goal")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["data"]["goal"].is_null(),
            "a cleared goal must not leak through the live read: {body}"
        );
        assert_eq!(body["data"]["driven"], true, "{body}");

        // The authenticated session snapshot agrees with the REST read.
        let (status, snapshot) = post_json(&app, &g("/open"), serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK, "{snapshot}");
        assert!(
            snapshot["data"]["snapshot"]["goal"].is_null(),
            "the snapshot must not carry a cleared goal: {snapshot}"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// A router holding one session that was created and opened through the
    /// real API against a mock backend, so goal control has a live runtime to
    /// drive. Returns the router and the fixture home for cleanup.
    ///
    /// The session is created through the API rather than hand-placed on disk:
    /// the runtime derives the project key by hashing the working directory, so
    /// a hand-written `projects/<name>` layout would never resolve.
    async fn open_goal_router() -> (Router, PathBuf, String) {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let home = std::env::temp_dir().join(format!(
            "mink-server-goaltest-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let cwd = home.join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = Arc::new(Registry::with_llm_backend(
            home.clone(),
            "flash".to_string(),
            4,
            Arc::new(MockRuntimeBackend),
        ));
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let app = router(Arc::new(ApiState {
            registry,
            cwd: cwd.clone(),
            shutdown,
        }));

        let created = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sessions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "name": "goaltest", "cwd": cwd.display().to_string() })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .expect("create request must be served");
        let status = created.status();
        let bytes = created.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(status, StatusCode::OK, "create: {body}");
        let session_id = body["data"]["id"].as_str().expect("session id").to_string();

        let opened = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/sessions/{session_id}/open"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("open request must be served");
        let status = opened.status();
        let bytes = opened.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            status,
            StatusCode::OK,
            "open: {}",
            String::from_utf8_lossy(&bytes)
        );
        (app, home, session_id)
    }

    async fn post_json(
        app: &Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn goal_control_requires_an_open_session() {
        // `test_router` never opens the session, so control must refuse rather
        // than write a goal.json that no runtime would ever drive.
        let (app, _dir) = test_router_with_session_dir();
        let (status, body) = post_json(
            &app,
            "/api/sessions/test-session/goal",
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "ship it" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    #[tokio::test]
    async fn goal_control_rejects_a_blank_objective_with_a_host_error() {
        let (app, home, sid) = open_goal_router().await;
        let g = |suffix: &str| format!("/api/sessions/{sid}{suffix}");
        let (status, body) = post_json(
            &app,
            &g("/goal"),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "   " }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("objective"),
            "{body}"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// The workbench reaches `/model` and `/compact` through the same session
    /// runtime the terminal drives, so both must be host endpoints rather than
    /// client-only behaviour.
    #[tokio::test]
    async fn server_switches_model_and_compacts_through_the_runtime() {
        let (app, home, sid) = open_goal_router().await;
        let g = |suffix: &str| format!("/api/sessions/{sid}{suffix}");

        // A blank model is a host error, not a silent no-op.
        let (status, body) =
            post_json(&app, &g("/model"), serde_json::json!({ "model": "   " })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (status, body) =
            post_json(&app, &g("/model"), serde_json::json!({ "model": "pro" })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["model"], "pro", "{body}");
        assert!(
            body["data"]["label"].is_string(),
            "the switch must resolve an alias to a real label: {body}"
        );

        let (status, body) = post_json(&app, &g("/compact"), serde_json::json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["compacted"], true, "{body}");

        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn server_creates_and_controls_a_goal_through_the_runtime() {
        let (app, home, sid) = open_goal_router().await;
        let g = |suffix: &str| format!("/api/sessions/{sid}{suffix}");

        // Creating requires the session to be open; the fixture opened it.
        let (status, body) = post_json(
            &app,
            &g("/goal"),
            serde_json::json!({
                "objective": "ship the parser",
                "protocol_version": 2,
                "generate": false,
                "acceptance": "agent_reviewed",
                "max_turns": 4,
                "acceptance_criteria": ["parses fixtures"],
                "constraints": ["no network"]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let goal = &body["data"]["goal"];
        assert_eq!(goal["status"], "draft", "{body}");
        assert_eq!(
            goal["spec"]["limits"]["max_turns"], 4,
            "host-chosen limit is honoured"
        );
        let id = goal["id"].as_str().expect("goal id").to_string();
        let revision = goal["revision"].as_u64().expect("goal revision");
        let (status, confirmed) = post_json(
            &app,
            &g("/goal/confirm"),
            serde_json::json!({"goal_id":id,"revision":revision}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{confirmed}");
        let revision = confirmed["data"]["goal"]["revision"].as_u64().unwrap();

        // The live runtime reports the same goal, not a stale disk read.
        let (status, body) = req(&app, Method::GET, &g("/goal")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["driven"], true, "{body}");
        assert_eq!(body["data"]["goal"]["id"], id, "{body}");

        // Every mutation is a compare-and-set on the goal the caller means.
        let (status, body) = post_json(
            &app,
            &g("/goal/pause"),
            serde_json::json!({ "revision": revision + 99, "goal_id": id }),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "stale revision must be refused: {body}"
        );

        // A missing goal id is a host error, not a silent default to the
        // server's current goal: replacements restart revisions at 1, so
        // substituting the current id would let a stale request act on it.
        let (status, body) = post_json(
            &app,
            &g("/goal/pause"),
            serde_json::json!({ "revision": revision }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        // And a goal id that is not the current one is refused outright.
        let (status, body) = post_json(
            &app,
            &g("/goal/pause"),
            serde_json::json!({ "revision": revision, "goal_id": "goal-someone-else" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // The current revision pauses, and the state is observable afterwards.
        let (status, body) = post_json(
            &app,
            &g("/goal/pause"),
            serde_json::json!({ "revision": revision, "goal_id": id }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["goal"]["status"], "paused", "{body}");
        let paused_revision = body["data"]["goal"]["revision"].as_u64().unwrap();

        let (status, body) = post_json(
            &app,
            &g("/goal/resume"),
            serde_json::json!({ "revision": paused_revision, "goal_id": id }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["goal"]["status"], "active", "{body}");

        // Missing revision is a host error, not a silent no-op.
        let (status, body) = post_json(&app, &g("/goal/cancel"), serde_json::json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn goal_rejects_an_unknown_acceptance_mode() {
        let (app, home, sid) = open_goal_router().await;
        let g = |suffix: &str| format!("/api/sessions/{sid}{suffix}");
        let (status, body) = post_json(
            &app,
            &g("/goal"),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "x", "acceptance": "wishful" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn legacy_goal_write_requires_explicit_protocol_upgrade() {
        let (app, home, sid) = open_goal_router().await;
        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{sid}/goal"),
            serde_json::json!({"objective":"legacy request"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            body["message"]
                .as_str()
                .unwrap()
                .contains("protocol_version 2")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    /// Observable form of the `max_running` contract: how many provider
    /// streams are in flight right now, and the high-water mark seen. A
    /// sequence of host commands may never push the high-water mark above the
    /// configured limit.
    #[derive(Default)]
    struct CapacityGauge {
        active: std::sync::atomic::AtomicUsize,
        max_active: std::sync::atomic::AtomicUsize,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CapacityGauge {
        fn active(&self) -> usize {
            self.active.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn max_active(&self) -> usize {
            self.max_active.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Backend whose stream waits for a signal, so an execution can hold its
    /// capacity slot while the host makes competing decisions.
    struct GatedBackend {
        release: Arc<tokio::sync::watch::Sender<bool>>,
        gauge: Arc<CapacityGauge>,
    }

    impl GatedBackend {
        fn new(release: &Arc<tokio::sync::watch::Sender<bool>>) -> Self {
            Self {
                release: release.clone(),
                gauge: Arc::new(CapacityGauge::default()),
            }
        }

        fn with_gauge(
            release: &Arc<tokio::sync::watch::Sender<bool>>,
            gauge: Arc<CapacityGauge>,
        ) -> Self {
            Self {
                release: release.clone(),
                gauge,
            }
        }
    }

    #[async_trait::async_trait]
    impl mink::runtime::LlmBackend for GatedBackend {
        fn name(&self) -> &str {
            "server-api-gated"
        }

        async fn stream(
            &self,
            _request: mink::runtime::LlmRequest,
        ) -> anyhow::Result<mink::runtime::LlmResponseStream> {
            use std::sync::atomic::Ordering::SeqCst;
            let in_flight = self.gauge.active.fetch_add(1, SeqCst) + 1;
            self.gauge.max_active.fetch_max(in_flight, SeqCst);
            self.gauge.calls.fetch_add(1, SeqCst);
            let result: anyhow::Result<()> = async {
                let mut released = self.release.subscribe();
                while !*released.borrow() {
                    released
                        .changed()
                        .await
                        .map_err(|error| anyhow::anyhow!("{error}"))?;
                }
                Ok(())
            }
            .await;
            self.gauge.active.fetch_sub(1, SeqCst);
            result?;
            Ok(mink::runtime::LlmResponseStream {
                events: Box::pin(futures::stream::iter(vec![
                    Ok(mink::runtime::LlmEvent::Text(mink::runtime::LlmTextEvent {
                        content: "gated answer".into(),
                    })),
                    Ok(mink::runtime::LlmEvent::Stop(mink::runtime::LlmStopEvent {
                        reason: "end_turn".into(),
                    })),
                ])),
                attempt_count: 1,
            })
        }
    }

    async fn gated_router(
        max_running: usize,
        release: Arc<tokio::sync::watch::Sender<bool>>,
    ) -> (Router, PathBuf, String, String) {
        let home = std::env::temp_dir().join(format!(
            "mink-server-gated-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = home.join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = Arc::new(Registry::with_llm_backend(
            home.clone(),
            "flash".to_string(),
            max_running,
            Arc::new(GatedBackend::new(&release)),
        ));
        let first = registry.create("first", &cwd).await.unwrap();
        let second = registry.create("second", &cwd).await.unwrap();
        registry
            .open(&first.id, Some(&first.project_key))
            .await
            .unwrap();
        registry
            .open(&second.id, Some(&second.project_key))
            .await
            .unwrap();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let app = router(Arc::new(ApiState {
            registry,
            cwd,
            shutdown,
        }));
        (app, home, first.id, second.id)
    }

    async fn wait_for_running(app: &Router, id: &str, expected: bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (_, body) = req(app, Method::GET, &format!("/api/sessions/{id}")).await;
            if body["data"]["running"].as_bool() == Some(expected) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session {id} never reached running={expected}: {body}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Audit regression: a goal creation refused while an ordinary turn owns
    /// the session reserved a capacity slot and never released it, so the
    /// slot leaked even though no goal ever existed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refused_goal_creation_does_not_leak_a_capacity_slot() {
        let release = Arc::new(tokio::sync::watch::Sender::new(false));
        let (app, home, first, second) = gated_router(1, release.clone()).await;

        // The ordinary turn is the only running turn and holds the slot.
        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{first}/turn"),
            serde_json::json!({ "input": "hold the capacity slot" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        wait_for_running(&app, &first, true).await;

        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{first}/goal"),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "busy goal" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // Let the turn end, then a second session must be able to claim the
        // released capacity: the refused attempt created no goal, so nothing
        // may stay reserved for it.
        release.send_replace(true);
        wait_for_running(&app, &first, false).await;
        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{second}/goal"),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "second goal", "max_turns": 1 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // Stop the driven goal before dropping the fixture home.
        let goal = &body["data"]["goal"];
        let _ = post_json(
            &app,
            &format!("/api/sessions/{second}/goal/cancel"),
            serde_json::json!({ "goal_id": goal["id"], "revision": goal["revision"] }),
        )
        .await;
        let _ = std::fs::remove_dir_all(home);
    }

    /// A repeat create must not free the slot its unfinished goal still holds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeat_goal_create_keeps_the_running_goal_slot() {
        let release = Arc::new(tokio::sync::watch::Sender::new(false));
        let (app, home, first, second) = gated_router(1, release.clone()).await;

        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{first}/goal"),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "first goal" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let draft = &body["data"]["goal"];
        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{first}/goal/confirm"),
            serde_json::json!({"goal_id":draft["id"],"revision":draft["revision"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // The repeat is refused but its rollback must observe the active goal
        // and leave the slot alone.
        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{first}/goal"),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "replacement must be refused" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{second}/goal"),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "second goal" }),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

        release.send_replace(true);
        let _ = std::fs::remove_dir_all(home);
    }

    /// Audit8 F1: a goal left `active` by a crash must never be driven by the
    /// mere act of arming the driver. Reopening issues no provider request;
    /// the leased resume is the activation that starts exactly one run, keeps
    /// the single capacity slot, and refuses competing work while it runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restored_active_goal_starts_only_through_a_leased_resume() {
        let home = std::env::temp_dir().join(format!(
            "mink-server-restore-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = home.join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let release = Arc::new(tokio::sync::watch::Sender::new(false));
        let gauge = Arc::new(CapacityGauge::default());
        let registry = Arc::new(Registry::with_llm_backend(
            home.clone(),
            "flash".to_string(),
            1,
            Arc::new(GatedBackend::with_gauge(&release, gauge.clone())),
        ));
        let first = registry.create("restore-a", &cwd).await.unwrap();
        let second = registry.create("restore-b", &cwd).await.unwrap();
        registry
            .open(&first.id, Some(&first.project_key))
            .await
            .unwrap();
        registry
            .open(&second.id, Some(&second.project_key))
            .await
            .unwrap();

        // Create the goal while scheduling is disabled: the contract is on
        // disk as `active` with no run behind it, which is exactly the state a
        // hard-killed server reopens into (recovery leaves it active and
        // idle; nothing arms the driver until an explicit activation).
        let created = {
            let runtime = registry
                .active_runtime(&first.id, Some(&first.project_key))
                .unwrap()
                .unwrap();
            let handle = runtime.goal_handle().unwrap();
            let draft = handle
                .draft_goal(mink::runtime::GoalSpec::new("restore then resume"), 3600)
                .unwrap();
            let goal = handle.confirm_goal(&draft.id, draft.revision).unwrap();
            assert_eq!(goal.status, mink::runtime::GoalStatus::Active);
            goal
        };
        assert_eq!(
            gauge.calls(),
            0,
            "an armed-only runtime must issue no provider request"
        );
        assert_eq!(registry.capacity.occupied(), 0);

        let (shutdown, _) = tokio::sync::watch::channel(false);
        let app = router(Arc::new(ApiState {
            registry: registry.clone(),
            cwd: cwd.clone(),
            shutdown,
        }));
        let base = format!("/api/sessions/{}", first.id);
        let (status, body) = req(&app, Method::GET, &format!("{base}/goal")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let restored = body["data"]["goal"].clone();
        assert_eq!(restored["status"], "active", "{body}");
        assert_eq!(restored["phase"], "idle", "{body}");
        let goal_id = restored["id"].as_str().unwrap().to_string();
        let revision = restored["revision"].as_u64().unwrap();

        // The leased resume is the activation that owns the run.
        let (status, body) = post_json(
            &app,
            &format!("{base}/goal/resume"),
            serde_json::json!({ "goal_id": goal_id, "revision": revision }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let resumed_revision = body["data"]["goal"]["revision"].as_u64().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while gauge.active() != 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "the resumed run never issued a provider request"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            registry.capacity.occupied(),
            1,
            "the admitted run holds the single slot"
        );
        assert!(
            registry
                .running(&first.id, Some(&first.project_key))
                .unwrap()
        );

        // Competing work under `max_running = 1` is refused, not admitted.
        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{}/goal", second.id),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "competing goal" }),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{}/turn", second.id),
            serde_json::json!({ "input": "competing turn" }),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");

        // A repeat resume while the run owns the slot adds no second run.
        let (status, body) = post_json(
            &app,
            &format!("{base}/goal/resume"),
            serde_json::json!({ "goal_id": goal_id, "revision": resumed_revision }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(
            gauge.calls(),
            1,
            "a refused resume issued a provider request"
        );
        assert_eq!(gauge.active(), 1);
        assert_eq!(
            created.id, goal_id,
            "the resumed goal is the restored contract"
        );

        // Cancel is a stop intent: the in-flight turn still owns the slot
        // until it really exits, and only then is capacity released.
        let (_, body) = req(&app, Method::GET, &format!("{base}/goal")).await;
        let live_revision = body["data"]["goal"]["revision"].as_u64().unwrap();
        let (status, body) = post_json(
            &app,
            &format!("{base}/goal/cancel"),
            serde_json::json!({ "goal_id": goal_id, "revision": live_revision }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            registry.capacity.occupied(),
            1,
            "capacity stays occupied while the cancelled turn exits"
        );
        release.send_replace(true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while registry.capacity.occupied() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the cancelled run never released its slot"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            gauge.max_active() <= 1,
            "max_running was exceeded: {}",
            gauge.max_active()
        );
        registry.shutdown_all().await.unwrap();
        let _ = std::fs::remove_dir_all(home);
    }

    /// A configuration refusal (no GoalReport in the resolved surface) also
    /// rolls back the reservation it made before the contract check.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configuration_refused_goal_creation_rolls_back_its_slot() {
        let home = std::env::temp_dir().join(format!(
            "mink-server-goal-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = home.join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        // The project layer removes GoalReport, so core refuses the contract.
        std::fs::write(cwd.join(".minkrc"), "[tools]\nenabled_tools = [\"Read\"]\n").unwrap();
        let release = Arc::new(tokio::sync::watch::Sender::new(false));
        let registry = Arc::new(Registry::with_llm_backend(
            home.clone(),
            "flash".to_string(),
            1,
            Arc::new(GatedBackend::new(&release)),
        ));
        let session = registry.create("config", &cwd).await.unwrap();
        registry
            .open(&session.id, Some(&session.project_key))
            .await
            .unwrap();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let app = router(Arc::new(ApiState {
            registry: registry.clone(),
            cwd,
            shutdown,
        }));

        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{}/goal", session.id),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "must be refused" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("GoalRead"),
            "the refusal names the missing capability: {body}"
        );

        // The refused attempt created no goal, so the sole capacity slot must
        // be free: a second session without the restrictive layer can claim it.
        let cwd2 = home.join("workspace2");
        std::fs::create_dir_all(&cwd2).unwrap();
        let other = registry.create("other", &cwd2).await.unwrap();
        registry
            .open(&other.id, Some(&other.project_key))
            .await
            .unwrap();
        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{}/goal", other.id),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "capacity was released", "max_turns": 1 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        release.send_replace(true);
        let _ = std::fs::remove_dir_all(home);
    }

    /// A persistence failure during goal creation must release the slot too:
    /// the goal never existed, so nothing may keep occupying `max_running`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_goal_persistence_rolls_back_its_slot() {
        use std::os::unix::fs::PermissionsExt;

        let release = Arc::new(tokio::sync::watch::Sender::new(false));
        let (app, home, first, second) = gated_router(1, release.clone()).await;
        let projects = home.join(".mink").join("projects");
        let session_dir = std::fs::read_dir(&projects)
            .unwrap()
            .flat_map(|entry| std::fs::read_dir(entry.unwrap().path()).unwrap())
            .map(|entry| entry.unwrap().path())
            .find(|path| path.file_name().is_some_and(|name| name == first.as_str()))
            .expect("session directory");
        let original = std::fs::metadata(&session_dir).unwrap().permissions();
        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{first}/goal"),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "persistence must fail", "max_turns": 1 }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // Restore write permission before the fixture cleanup, then prove the
        // slot is free by letting the second session claim it.
        std::fs::set_permissions(&session_dir, original).unwrap();
        let (status, body) = post_json(
            &app,
            &format!("/api/sessions/{second}/goal"),
            serde_json::json!({ "protocol_version":2, "generate":false, "objective": "slot was released", "max_turns": 1 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        release.send_replace(true);
        let _ = std::fs::remove_dir_all(home);
    }

    async fn req(app: &Router, method: Method, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn resume_guidance_obeys_capacity_and_live_guidance_remains_idempotent() {
        // This test builds its registry from ambient configuration, so it must
        // hold the shared environment lock: otherwise a concurrent config test
        // can leave `MODEL` set to a value that changes the resolved surface
        // and the capacity expectations below no longer describe what runs.
        let _env = crate::session::TEST_ENV_LOCK.lock().await;
        struct WaitingBackend;
        #[async_trait::async_trait]
        impl mink::runtime::LlmBackend for WaitingBackend {
            fn name(&self) -> &str {
                "capacity-test"
            }
            async fn stream(
                &self,
                _request: mink::runtime::LlmRequest,
            ) -> anyhow::Result<mink::runtime::LlmResponseStream> {
                Ok(mink::runtime::LlmResponseStream {
                    events: Box::pin(futures::stream::pending()),
                    attempt_count: 1,
                })
            }
        }
        async fn post(
            app: &Router,
            uri: &str,
            data: serde_json::Value,
        ) -> (StatusCode, serde_json::Value) {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from(data.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, serde_json::from_slice(&bytes).unwrap())
        }
        let home = std::env::temp_dir().join(format!(
            "mink-capacity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = Arc::new(Registry::with_llm_backend(
            home.clone(),
            "mock".into(),
            1,
            Arc::new(WaitingBackend),
        ));
        let a = registry.create("a", &cwd).await.unwrap();
        let b = registry.create("b", &cwd).await.unwrap();
        registry.open(&a.id, Some(&a.project_key)).await.unwrap();
        registry.open(&b.id, Some(&b.project_key)).await.unwrap();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let app = router(Arc::new(ApiState {
            registry: registry.clone(),
            cwd,
            shutdown,
        }));
        let a_base = format!("/api/sessions/{}", a.id);
        let a_inputs = format!("{a_base}/inputs?project={}", a.project_key);
        let b_inputs = format!("/api/sessions/{}/inputs?project={}", b.id, b.project_key);
        let task = json!({"request_id":"first", "text":"wait", "target_turn_id":null});
        let (status, first) = post(&app, &a_inputs, task.clone()).await;
        assert_eq!(status, StatusCode::OK);
        // A retry of an already admitted request must not consume another slot.
        let (status, duplicate) = post(&app, &a_inputs, task).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(first["data"]["input_id"], duplicate["data"]["input_id"]);
        let guidance = json!({"request_id":"guide", "text":"extra constraint", "target_turn_id":first["data"]["turn_id"]});
        let (status, guide) = post(&app, &a_inputs, guidance.clone()).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "live guidance is allowed at capacity"
        );
        assert_eq!(post(&app, &a_inputs, guidance).await.0, StatusCode::OK);
        registry.interrupt(&a.id, Some(&a.project_key)).unwrap();
        let runtime = registry
            .active_runtime(&a.id, Some(&a.project_key))
            .unwrap()
            .unwrap();
        let guide_id = guide["data"]["input_id"].as_str().unwrap();
        let receipt = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !runtime.running()
                    && let Some(receipt) = runtime.inbox().entries().into_iter().find(|r| {
                        r.input_id == guide_id && r.status == mink::runtime::InputStatus::Unapplied
                    })
                {
                    break receipt;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        // Reopen to exercise durable input restoration and finish old cleanup.
        registry.close(&a.id, Some(&a.project_key)).await.unwrap();
        registry.open(&a.id, Some(&a.project_key)).await.unwrap();
        assert_eq!(
            post(
                &app,
                &b_inputs,
                json!({"request_id":"second", "text":"wait"})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            post(
                &app,
                &a_inputs,
                json!({"request_id":"third", "text":"new task"})
            )
            .await
            .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        let resume_uri = format!(
            "{a_base}/inputs/{guide_id}/resume?project={}",
            a.project_key
        );
        assert_eq!(
            post(&app, &resume_uri, json!({"revision":receipt.revision}))
                .await
                .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert!(
            !registry
                .active_runtime(&a.id, Some(&a.project_key))
                .unwrap()
                .unwrap()
                .running()
        );
        // A normal input resume must also be capacity checked.
        let root_resume = format!(
            "{a_base}/inputs/{}/resume?project={}",
            first["data"]["input_id"].as_str().unwrap(),
            a.project_key
        );
        assert_eq!(
            post(
                &app,
                &root_resume,
                json!({"revision":first["data"]["revision"]})
            )
            .await
            .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        registry.close(&b.id, Some(&b.project_key)).await.unwrap();
        let (status, resumed) = post(&app, &resume_uri, json!({"revision":receipt.revision})).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "resume succeeds after capacity is released: {resumed}"
        );
        assert_eq!(resumed["data"]["guidance"], false);
        assert_ne!(resumed["data"]["turn_id"], guide["data"]["turn_id"]);
        registry.shutdown_all().await.unwrap();
        std::fs::remove_dir_all(home).unwrap();
    }

    #[tokio::test]
    async fn conversation_pagination_and_seq() {
        let app = test_router();
        // tail：返回最后 2 行，注入行号 seq
        let (s, body) = req(
            &app,
            Method::GET,
            "/api/sessions/test-session/conversation?limit=2&tail=true",
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let rows = body["data"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["seq"], serde_json::json!(2));
        assert_eq!(rows[1]["content"], serde_json::json!("three"));
        // before_seq：取 seq 之前的行
        let (_, body) = req(
            &app,
            Method::GET,
            "/api/sessions/test-session/conversation?limit=1&before_seq=3",
        )
        .await;
        let rows = body["data"].as_array().unwrap();
        assert_eq!(rows[0]["seq"], serde_json::json!(2));
        // 不存在的会话 → 404
        let (s, _) = req(&app, Method::GET, "/api/sessions/nope/conversation").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn files_path_escape_rejected() {
        let app = test_router();
        let (s, body) = req(
            &app,
            Method::GET,
            "/api/sessions/test-session/files?path=../../../etc",
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(body["message"].as_str().unwrap_or("").contains("escape"));
    }

    #[tokio::test]
    async fn artifact_name_filtered() {
        let app = test_router();
        let (s, _body) = req(
            &app,
            Method::GET,
            "/api/sessions/test-session/artifacts/..%2F..%2Fpasswd",
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        // 正常 artifact 可读
        let (s, body) = req(
            &app,
            Method::GET,
            "/api/sessions/test-session/artifacts/abc",
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["data"]["content"], serde_json::json!("artifact body"));
    }

    #[tokio::test]
    async fn turn_input_validation() {
        let app = test_router();
        // 空 body → axum JSON 提取器拒绝（415）
        let (s, _) = req(&app, Method::POST, "/api/sessions/test-session/turn").await;
        assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        // 带 JSON body 但 input 为空 → 400
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/sessions/test-session/turn")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"input":""}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn health_and_plan() {
        let app = test_router();
        let (s, body) = req(&app, Method::GET, "/health").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["data"]["status"], serde_json::json!("ok"));
        let (s, body) = req(&app, Method::GET, "/api/sessions/test-session/plan").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["data"]["plan"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn turn_request_id_retry_does_not_start_a_second_turn() {
        let home = std::env::temp_dir().join(format!(
            "mink-server-turn-idem-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = Arc::new(Registry::with_llm_backend(
            home.clone(),
            "mock".into(),
            1,
            Arc::new(MockRuntimeBackend),
        ));
        let created = registry.create("turn-idem", &cwd).await.unwrap();
        let project = created.project_key.clone();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let state = Arc::new(ApiState {
            registry: registry.clone(),
            cwd,
            shutdown,
        });
        let app = router(state);
        let (status, _) = req(
            &app,
            Method::POST,
            &format!("/api/sessions/{}/open?project={project}", created.id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(format!(
                            "/api/sessions/{}/turn?project={project}",
                            created.id
                        ))
                        .header("content-type", "application/json")
                        .body(Body::from(
                            r#"{"input":"hello once","request_id":"stable-turn-1"}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        // Reusing the id with different content is a conflict, not an internal
        // error and not a second turn.
        let conflict = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!(
                        "/api/sessions/{}/turn?project={project}",
                        created.id
                    ))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"input":"different words","request_id":"stable-turn-1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        // The mock turn finishes quickly; the retry must have reused the first
        // receipt instead of appending the same user input twice.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let dir = home
            .join(".mink")
            .join("projects")
            .join(&project)
            .join(&created.id);
        let conversation = std::fs::read_to_string(dir.join("conversation.jsonl")).unwrap();
        assert_eq!(
            conversation.matches("hello once").count(),
            1,
            "{conversation}"
        );
        registry.close(&created.id, Some(&project)).await.unwrap();
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn closed_runtime_is_reapable_and_leaves_the_active_table() {
        let home = std::env::temp_dir().join(format!(
            "mink-server-reap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = Arc::new(Registry::with_llm_backend(
            home.clone(),
            "mock".into(),
            1,
            Arc::new(MockRuntimeBackend),
        ));
        let created = registry.create("reap", &cwd).await.unwrap();
        let project = created.project_key.clone();
        registry.open(&created.id, Some(&project)).await.unwrap();

        let runtime = registry
            .active_runtime(&created.id, Some(&project))
            .unwrap()
            .expect("opened runtime");
        runtime.shutdown().await.unwrap();
        assert!(
            registry
                .closed_session_ids()
                .iter()
                .any(|(id, _)| id == &created.id),
            "a forced/closed runtime must be reaper-visible"
        );

        // The reaper's action removes the entry (and its broadcast sender).
        registry.close(&created.id, Some(&project)).await.unwrap();
        assert!(
            !registry
                .closed_session_ids()
                .iter()
                .any(|(id, _)| id == &created.id)
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn closing_a_session_emits_closed_to_its_sse_stream() {
        let home = std::env::temp_dir().join(format!(
            "mink-server-sse-close-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = Arc::new(Registry::with_llm_backend(
            home.clone(),
            "mock".into(),
            1,
            Arc::new(MockRuntimeBackend),
        ));
        let created = registry.create("sse-close", &cwd).await.unwrap();
        let project = created.project_key.clone();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let state = Arc::new(ApiState {
            registry: registry.clone(),
            cwd,
            shutdown,
        });
        let app = router(state);
        let (status, _) = req(
            &app,
            Method::POST,
            &format!("/api/sessions/{}/open?project={project}", created.id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let stream_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/sessions/{}/stream?project={project}&snapshot=true",
                        created.id
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stream_response.status(), StatusCode::OK);
        let mut body = stream_response.into_body();

        // The reaper's action: close removes the active entry and shutdown
        // publishes the terminal phase to every subscribed stream.
        registry.close(&created.id, Some(&project)).await.unwrap();
        let saw_closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(frame) = body.frame().await {
                let Ok(data) = frame.unwrap().into_data() else {
                    continue;
                };
                for line in String::from_utf8_lossy(&data).lines() {
                    let Some(json) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let Ok(event) = serde_json::from_str::<serde_json::Value>(json) else {
                        continue;
                    };
                    if event["type"] == "phase_updated" && event["phase"] == "closed" {
                        return true;
                    }
                }
            }
            false
        })
        .await
        .expect("the stream must either deliver closed or end");
        assert!(saw_closed, "close must publish phase_updated closed");
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn mock_runtime_api_streams_authoritative_sse_envelope() {
        let home = std::env::temp_dir().join(format!(
            "mink-server-mock-sse-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = Arc::new(Registry::with_llm_backend(
            home.clone(),
            "mock".into(),
            1,
            Arc::new(MockRuntimeBackend),
        ));
        let created = registry.create("mock-sse", &cwd).await.unwrap();
        let project = created.project_key.clone();
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let state = Arc::new(ApiState {
            registry: registry.clone(),
            cwd,
            shutdown,
        });
        let app = router(state);

        let (status, _) = req(
            &app,
            Method::POST,
            &format!("/api/sessions/{}/open?project={project}", created.id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let stream_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/sessions/{}/stream?project={project}",
                        created.id
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stream_response.status(), StatusCode::OK);
        let mut body = stream_response.into_body();

        let turn_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!(
                        "/api/sessions/{}/turn?project={project}",
                        created.id
                    ))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"input":"hello"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(turn_response.status(), StatusCode::OK);

        let mut types = Vec::new();
        let mut saw_stream_sequence = false;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while let Some(frame) = body.frame().await {
                let frame = frame.unwrap();
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                for line in String::from_utf8_lossy(&data).lines() {
                    let Some(json) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let event: serde_json::Value = serde_json::from_str(json).unwrap();
                    saw_stream_sequence |= event["stream_sequence"].is_number();
                    if let Some(kind) = event["type"].as_str() {
                        types.push(kind.to_string());
                        if kind == "turn_final" {
                            return;
                        }
                    }
                }
            }
        })
        .await
        .expect("mock runtime SSE did not reach turn_final");

        assert!(saw_stream_sequence);
        assert!(types.starts_with(&["turn_started".into()]));
        assert!(types.contains(&"text".into()));
        assert!(types.contains(&"stop".into()));
        assert_eq!(types.last().map(String::as_str), Some("turn_final"));
        registry.close(&created.id, Some(&project)).await.unwrap();
        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn sse_stream_ends_when_shutdown_signal_is_sent() {
        let home = std::env::temp_dir().join(format!(
            "mink-server-shutdown-sse-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = Arc::new(Registry::with_llm_backend(
            home.clone(),
            "mock".into(),
            1,
            Arc::new(MockRuntimeBackend),
        ));
        let created = registry.create("shutdown-sse", &cwd).await.unwrap();
        let project = created.project_key.clone();
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        let state = Arc::new(ApiState {
            registry: registry.clone(),
            cwd,
            shutdown: shutdown_tx.clone(),
        });
        let app = router(state);

        let (status, _) = req(
            &app,
            Method::POST,
            &format!("/api/sessions/{}/open?project={project}", created.id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let stream_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/sessions/{}/stream?project={project}",
                        created.id
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stream_response.status(), StatusCode::OK);
        let mut body = stream_response.into_body();

        let _ = shutdown_tx.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while let Some(frame) = body.frame().await {
                let _ = frame.unwrap();
            }
        })
        .await
        .expect("SSE stream should close after the shutdown signal is sent");

        registry.close(&created.id, Some(&project)).await.unwrap();
        let _ = std::fs::remove_dir_all(home);
    }
    #[tokio::test]
    async fn input_receipts_snapshot_and_legacy_history_are_consistent() {
        let home = std::env::temp_dir().join(format!(
            "mink-input-api-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = Arc::new(Registry::with_llm_backend(
            home.clone(),
            "mock".into(),
            1,
            Arc::new(MockRuntimeBackend),
        ));
        let created = registry.create("inputs", &cwd).await.unwrap();
        let base = format!("/api/sessions/{}", created.id);
        let project = created.project_key;
        let (shutdown, _) = tokio::sync::watch::channel(false);
        let app = router(Arc::new(ApiState {
            registry: registry.clone(),
            cwd,
            shutdown,
        }));
        assert_eq!(
            req(
                &app,
                Method::POST,
                &format!("{base}/open?project={project}")
            )
            .await
            .0,
            StatusCode::OK
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("{base}/stream?project={project}&snapshot=true"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut stream = response.into_body();
        let initial = tokio::time::timeout(std::time::Duration::from_secs(3), stream.frame())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap();
        let initial = String::from_utf8(initial.to_vec()).unwrap();
        let snapshot: serde_json::Value =
            serde_json::from_str(initial.trim().strip_prefix("data: ").unwrap()).unwrap();
        assert_eq!(snapshot["type"], "session_snapshot");
        assert!(snapshot["resources"]["todo"]["items"].is_array());
        assert!(snapshot["generation"].is_string());
        let payload =
            r#"{"request_id":"stable","text":"task","target_turn_id":null,"attachment_ids":[]}"#;
        let submit = |payload: &str| {
            Request::builder()
                .method(Method::POST)
                .uri(format!("{base}/inputs?project={project}"))
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap()
        };
        assert_eq!(
            app.clone().oneshot(submit(payload)).await.unwrap().status(),
            StatusCode::OK
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while let Some(frame) = stream.frame().await {
                let data = frame.unwrap().into_data().unwrap();
                let text = String::from_utf8(data.to_vec()).unwrap();
                let Some(json) = text.trim().strip_prefix("data: ") else {
                    continue;
                };
                let event: serde_json::Value = serde_json::from_str(json).unwrap();
                assert_eq!(event["generation"], snapshot["generation"]);
                if event["type"] == "turn_final" {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let (_, receipt) = req(
            &app,
            Method::GET,
            &format!("{base}/inputs?project={project}&request_id=stable"),
        )
        .await;
        assert_eq!(receipt["data"][0]["status"], "applied");
        assert_eq!(
            app.clone().oneshot(submit(payload)).await.unwrap().status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(submit(&payload.replace("task", "different")))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        let (_, outstanding) = req(
            &app,
            Method::GET,
            &format!("{base}/inputs?project={project}"),
        )
        .await;
        assert_eq!(outstanding["data"], json!([]));
        let (_, history) = req(
            &app,
            Method::GET,
            &format!("{base}/conversation?project={project}&turns=true&limit=1"),
        )
        .await;
        assert_eq!(history["data"][0]["_mink"]["input_id"], "stable");
        assert_eq!(history["data"].as_array().unwrap().len(), 2);
        drop(stream);
        registry.shutdown_all().await.unwrap();
        let _ = std::fs::remove_dir_all(home);
    }
}
