//! REST + SSE API. Unified `ApiResponse{code,message,data}` envelope.

use crate::session::registry::Registry;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
pub(super) use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

mod controls;
mod goals;
mod inputs;
pub(crate) mod middleware;
mod resources;
mod sessions;
mod stream;

#[cfg(test)]
mod tests;

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
    pub(super) fn ok(data: serde_json::Value) -> Self {
        Self {
            code: 200,
            message: String::new(),
            data,
        }
    }

    pub(crate) fn err(code: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: serde_json::Value::Null,
        }
    }
}

impl IntoResponse for ApiResponse {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self)).into_response()
    }
}

pub fn api_routes(state: Arc<ApiState>) -> Router {
    let attachment_route = Router::new()
        .route(
            "/api/sessions/{id}/attachments",
            post(inputs::upload_attachment),
        )
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024));

    let standard_routes = Router::new()
        .route("/api/version", get(sessions::api_version))
        .route(
            "/api/sessions",
            get(sessions::list_sessions).post(sessions::create_session),
        )
        .route("/api/workspace", get(sessions::check_workspace))
        .route(
            "/api/sessions/{id}",
            get(sessions::get_session).delete(sessions::delete_session),
        )
        .route("/api/sessions/{id}/open", post(sessions::open_session))
        .route("/api/sessions/{id}/close", post(sessions::close_session))
        .route("/api/sessions/{id}/turn", post(sessions::turn_session))
        .route(
            "/api/sessions/{id}/inputs",
            get(inputs::list_inputs).post(inputs::submit_input),
        )
        .route(
            "/api/sessions/{id}/inputs/{input_id}",
            axum::routing::patch(inputs::edit_input).delete(inputs::withdraw_input),
        )
        .route(
            "/api/sessions/{id}/inputs/{input_id}/resume",
            post(inputs::resume_input),
        )
        .route(
            "/api/sessions/{id}/attachments/{attachment_id}",
            get(inputs::read_attachment),
        )
        .route(
            "/api/sessions/{id}/interrupt",
            post(controls::interrupt_session),
        )
        .route("/api/sessions/{id}/events", get(stream::events_history))
        .route(
            "/api/sessions/{id}/conversation",
            get(stream::conversation_history),
        )
        .route("/api/sessions/{id}/stream", get(stream::stream_events))
        .route("/api/sessions/{id}/plan", get(resources::get_plan))
        .route("/api/sessions/{id}/todo", get(resources::get_todo))
        .route(
            "/api/sessions/{id}/goal",
            get(goals::get_goal).post(goals::create_goal),
        )
        .route("/api/sessions/{id}/goal/{action}", post(goals::goal_action))
        .route(
            "/api/sessions/{id}/model",
            get(controls::get_session_model).post(controls::set_session_model),
        )
        .route(
            "/api/sessions/{id}/compact",
            post(controls::compact_session),
        )
        .route(
            "/api/sessions/{id}/artifacts",
            get(resources::list_artifacts),
        )
        .route(
            "/api/sessions/{id}/artifacts/{name}",
            get(resources::get_artifact),
        )
        .route("/api/sessions/{id}/files", get(resources::get_files))
        .route("/health", get(stream::health))
        .route("/api", axum::routing::any(api_not_found))
        .route("/api/{*path}", axum::routing::any(api_not_found))
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024));

    standard_routes.merge(attachment_route).with_state(state)
}

async fn api_not_found() -> ApiResponse {
    ApiResponse::err(404, "not found")
}

#[cfg(test)]
pub fn router(state: Arc<ApiState>) -> Router {
    middleware::apply_security_layer(
        api_routes(state),
        Arc::new(middleware::HostGuardConfig::loopback_default()),
    )
}

#[derive(Default, Deserialize)]
pub(super) struct ProjectQuery {
    pub(super) project: Option<String>,
    pub(super) snapshot: Option<bool>,
    pub(super) request_id: Option<String>,
    /// Goal reads ask for the full contract only when a detail view is open.
    #[serde(default)]
    pub(super) full: Option<bool>,
}

pub(super) fn registry_error(error: crate::session::registry::RegistryError) -> ApiResponse {
    use crate::session::registry::RegistryError;
    let code = match &error {
        RegistryError::NotFound(_) => 404,
        RegistryError::Ambiguous(_) | RegistryError::Locked(_) | RegistryError::Busy(_) => 409,
        RegistryError::Capacity(_) => 429,
        RegistryError::Internal(_) => 500,
    };
    ApiResponse::err(code, error.to_string())
}

pub(super) const FILE_RAW_MAX_BYTES: u64 = 1 << 20;
use resources::resource_text;
use sessions::session_dir_or_err;
