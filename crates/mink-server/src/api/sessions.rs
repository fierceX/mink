use super::*;

#[derive(Deserialize)]
pub(super) struct CreateSessionReq {
    name: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// Client identity for a conversation that is created on its first send.
    /// Reusing it after a lost reply resolves the same session instead of
    /// leaving a second empty one.
    #[serde(default)]
    draft_id: Option<String>,
    /// The first human input; the server derives the display title from it.
    #[serde(default)]
    first_prompt: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct TurnReq {
    input: String,
    /// Optional stable id for retries; the inbox treats a repeat as the same
    /// submission instead of starting a second turn.
    #[serde(default)]
    request_id: Option<String>,
}

/// Resolve a session directory with the registry's typed status mapping.
pub(super) async fn session_dir_or_err(
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

#[derive(Deserialize)]
pub(super) struct WorkspaceQuery {
    path: Option<String>,
}

/// Validate a working directory before the workbench opens a composing view.
/// Projects are derived from sessions, so this deliberately creates nothing.
pub(super) async fn check_workspace(Query(query): Query<WorkspaceQuery>) -> ApiResponse {
    let Some(raw) = query
        .path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return ApiResponse::err(400, "path is required");
    };
    let path = PathBuf::from(raw);
    let checked = tokio::task::spawn_blocking(
        move || -> std::result::Result<(String, String, String), String> {
            let canonical = std::fs::canonicalize(&path)
                .map_err(|error| format!("workspace is not accessible: {error}"))?;
            if !canonical.is_dir() {
                return Err("workspace must be a directory".into());
            }
            let name = canonical
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| canonical.display().to_string());
            Ok((
                canonical.display().to_string(),
                name,
                mink::runtime::session::project_key(&canonical),
            ))
        },
    )
    .await;
    match checked {
        Ok(Ok((cwd, name, project_key))) => ApiResponse::ok(json!({
            "cwd": cwd,
            "project_key": project_key,
            "name": name,
        })),
        Ok(Err(message)) => ApiResponse::err(400, message),
        Err(error) => ApiResponse::err(500, format!("workspace check failed: {error}")),
    }
}

pub(super) async fn list_sessions(State(state): State<Arc<ApiState>>) -> ApiResponse {
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
pub(super) async fn api_version() -> ApiResponse {
    static BUNDLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let bundle = BUNDLE.get_or_init(|| {
        let index = crate::web_assets::FILES
            .iter()
            .find(|(path, _)| *path == "index.html")
            .map(|(_, bytes)| String::from_utf8_lossy(bytes).to_string());
        index
            .as_deref()
            .and_then(|html| {
                let mut rest = html;
                let mut first_match = None;
                while let Some(at) = rest.find("/assets/") {
                    let tail = &rest[at + "/assets/".len()..];
                    let Some(end) = tail.find(['"', '\'']) else {
                        break;
                    };
                    let candidate = &tail[..end];
                    if first_match.is_none() {
                        first_match = Some(candidate.to_string());
                    }
                    if candidate.ends_with(".js") || candidate.ends_with(".mjs") {
                        return Some(candidate.to_string());
                    }
                    rest = &tail[end..];
                }
                first_match
            })
            .unwrap_or_default()
    });
    ApiResponse::ok(json!({ "bundle": bundle }))
}

pub(super) async fn create_session(
    State(state): State<Arc<ApiState>>,
    Json(req): Json<CreateSessionReq>,
) -> ApiResponse {
    if let Some(prompt) = req.first_prompt.as_deref() {
        let prompt = prompt.trim();
        if prompt.len() > 128 * 1024 {
            return ApiResponse::err(400, "first_prompt too large (max 128 KiB)");
        }
    }
    let cwd = req.cwd.clone().unwrap_or_else(|| state.cwd.clone());
    // A workbench conversation is created on its first send. The draft id is
    // the idempotency key, so a reply lost on the network cannot leave two
    // sessions behind.
    if let Some(draft_id) = req
        .draft_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return match state
            .registry
            .create_draft(draft_id, &cwd, req.first_prompt.as_deref())
            .await
        {
            Ok(summary) => ApiResponse::ok(json!(summary)),
            Err(error) => registry_error(error),
        };
    }
    // No name is not "the unnamed session": every call without one must create
    // a fresh conversation. The generated alias stays internal; the title is
    // derived from the first prompt when the caller supplied one.
    let Some(name) = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        let draft_id = format!("draft-{}", mink::runtime::session::new_session_id());
        return match state
            .registry
            .create_draft(&draft_id, &cwd, req.first_prompt.as_deref())
            .await
        {
            Ok(summary) => ApiResponse::ok(json!(summary)),
            Err(error) => registry_error(error),
        };
    };
    match state.registry.create(name, &cwd).await {
        Ok(summary) => ApiResponse::ok(json!(summary)),
        Err(error) => registry_error(error),
    }
}

pub(super) async fn get_session(
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

pub(super) async fn delete_session(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    match state.registry.delete(&id, query.project.as_deref()).await {
        Ok(()) => ApiResponse::ok(json!({ "id": id, "deleted": true })),
        Err(error) => registry_error(error),
    }
}

pub(super) async fn open_session(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    match state.registry.open(&id, query.project.as_deref()).await {
        Ok(_) => ApiResponse::ok(json!({ "id": id, "status": "active", "snapshot": true })),
        Err(error) => registry_error(error),
    }
}

pub(super) async fn close_session(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    match state.registry.close(&id, query.project.as_deref()).await {
        Ok(_) => ApiResponse::ok(json!({ "id": id, "status": "free" })),
        Err(error) => registry_error(error),
    }
}

pub(super) async fn turn_session(
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
