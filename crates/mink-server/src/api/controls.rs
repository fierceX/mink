use super::*;

pub(super) async fn interrupt_session(
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
pub(super) struct ModelBody {
    /// An alias such as `flash`/`pro` or any real model name.
    model: String,
}

/// Switch the session's active model, mirroring the terminal's `/model`.
/// The session's model vocabulary plus the configured model.
///
/// The label a UI shows comes from the title-update path (and from this route's
/// POST companion after a switch), because a switch is owned by the running
/// orchestrator rather than by the configuration these fields describe.
pub(super) async fn get_session_model(
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

pub(super) async fn set_session_model(
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
    let _op = runtime.begin_operation();
    match handle.set_model_with_outcome(model.clone()).await {
        Ok(outcome) => ApiResponse::ok(json!({
            "model": model,
            "label": outcome.label,
            "stats": outcome.stats,
        })),
        Err(error) => ApiResponse::err(409, control_error(&error)),
    }
}

/// Manual context compaction, mirroring the terminal's `/compact`.
pub(super) async fn compact_session(
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
    let _op = runtime.begin_operation();
    match handle.compact().await {
        Ok(outcome) => ApiResponse::ok(json!({ "compacted": true, "outcome": outcome })),
        Err(error) => ApiResponse::err(409, control_error(&error)),
    }
}

/// Report why a runtime control was refused in language the workbench can show.
///
/// A raw core error surfaces English identifiers in an otherwise Chinese
/// interface, and the actionable ones need to say what to do next.
pub(super) fn control_error(error: &mink::runtime::RuntimeError) -> String {
    match error {
        mink::runtime::RuntimeError::ExternallyControlled { .. } => {
            "当前目标执行中，先暂停目标再操作".to_string()
        }
        mink::runtime::RuntimeError::Busy { .. } => "本轮尚未结束，请稍后重试".to_string(),
        _ => error.to_string(),
    }
}
