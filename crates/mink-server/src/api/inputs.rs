use super::*;

pub(super) fn runtime_or_err(
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
pub(super) fn input_query(
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
pub(super) async fn list_inputs(
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
        Ok(Err(error)) => ApiResponse::err(500, format!("cannot read inputs: {error}")),
        Err(error) => ApiResponse::err(500, format!("inputs read task failed: {error}")),
    }
}
pub(super) async fn submit_input(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
    Json(mut input): Json<mink::runtime::HumanInput>,
) -> ApiResponse {
    let runtime = match runtime_or_err(&state, &id, query.project.as_deref()) {
        Ok(r) => r,
        Err(e) => return e,
    };
    if input.request_id.trim().is_empty() {
        return ApiResponse::err(400, "request_id must not be empty");
    }
    if input.request_id.len() > 256 {
        return ApiResponse::err(400, "request_id too long (max 256 bytes)");
    }
    if input.text.len() > 128 * 1024 {
        return ApiResponse::err(400, "input too large (max 128 KiB)");
    }
    let submitted_text = input.text.trim().to_string();
    if input.attachment_ids.len() > 8 {
        return ApiResponse::err(400, "max 8 attachments");
    }
    if !input.attachment_ids.is_empty() {
        let Some(limits) = runtime.image_input().limits().cloned() else {
            return ApiResponse::err(400, "image input unavailable for this session");
        };
        if input.attachment_ids.len() > limits.max_images_per_request {
            return ApiResponse::err(413, "attachments exceed session limits");
        }
        let store = runtime.attachments().clone();
        let attachment_ids = input.attachment_ids.clone();
        let read_res = tokio::task::spawn_blocking(move || {
            let mut total = 0u64;
            let mut paths = Vec::with_capacity(attachment_ids.len());
            let max_total = limits.max_image_bytes_per_request.min(16 * 1024 * 1024);
            for attachment in &attachment_ids {
                let (path, bytes) = store
                    .read(attachment, limits.max_image_bytes)
                    .map_err(|e| (400u16, e.to_string()))?;
                total += bytes.len() as u64;
                if total > max_total {
                    return Err((413u16, "attachments exceed session limits".to_string()));
                }
                paths.push(path);
            }
            Ok(paths)
        })
        .await;
        let paths = match read_res {
            Ok(Ok(paths)) => paths,
            Ok(Err((code, msg))) => return ApiResponse::err(code, msg),
            Err(error) => return ApiResponse::err(500, format!("attachment read failed: {error}")),
        };
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
        Ok(receipt) => {
            // A session created without text stays untitled until a real human
            // message arrives. Run the fill under the registry's per-session
            // lock: a delete racing this write must not recreate the metadata
            // directory. Image-only input deliberately does not fill it.
            if !submitted_text.is_empty()
                && let Err(error) = state
                    .registry
                    .backfill_title(&id, query.project.as_deref(), &submitted_text)
                    .await
            {
                eprintln!("[mink-server] title backfill failed: {error}");
            }
            ApiResponse::ok(json!(receipt))
        }
        Err(e) => registry_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InputEdit {
    revision: u64,
    text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Revision {
    revision: u64,
}
pub(super) async fn edit_input(
    State(state): State<Arc<ApiState>>,
    AxumPath((id, input_id)): AxumPath<(String, String)>,
    Query(query): Query<ProjectQuery>,
    Json(edit): Json<InputEdit>,
) -> ApiResponse {
    if edit.text.len() > 128 * 1024 {
        return ApiResponse::err(400, "input too large (max 128 KiB)");
    }
    update_input(
        &state,
        &id,
        &input_id,
        query.project.as_deref(),
        edit.revision,
        Some(edit.text),
    )
}
pub(super) async fn withdraw_input(
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
pub(super) fn update_input(
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
pub(super) async fn resume_input(
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
pub(super) async fn upload_attachment(
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
pub(super) async fn read_attachment(
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
