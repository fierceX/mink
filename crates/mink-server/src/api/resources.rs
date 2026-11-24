use super::*;

#[derive(Deserialize)]
pub(super) struct FilesQuery {
    project: Option<String>,
    path: Option<String>,
    #[serde(default)]
    raw: bool,
}

pub(super) async fn get_plan(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let plan = match resource_text(&dir, "plan.md").await {
        Ok(plan) => plan,
        Err(response) => return response,
    };
    let draft = match resource_text(&dir, "plan.draft").await {
        Ok(draft) => draft,
        Err(response) => return response,
    };
    ApiResponse::ok(json!({ "plan": plan, "draft": draft }))
}

pub(super) async fn get_todo(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let todo = match resource_text(&dir, "todos.json").await {
        Ok(todo) => todo,
        Err(response) => return response,
    };
    match todo {
        Some(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => ApiResponse::ok(json!({ "todos": v })),
            Err(e) => ApiResponse::err(500, format!("todos.json parse error: {e}")),
        },
        None => ApiResponse::ok(json!({ "todos": null })),
    }
}

pub(super) async fn list_artifacts(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ProjectQuery>,
) -> ApiResponse {
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let artifacts_dir = dir.join("artifacts");
    let index = match resource_text(&artifacts_dir, "index.jsonl").await {
        Ok(index) => index,
        Err(response) => return response,
    };
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

pub(super) const MAX_DIR_ENTRIES: usize = 2_000;

pub(super) enum BoundedReadError {
    NotFound,
    Symlink,
    NotAFile,
    TooLarge(u64),
    Utf8,
    Io(std::io::Error),
}

pub(super) fn open_no_follow_nonblock(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    opts.open(path)
}

pub(super) fn is_symlink_open_error(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ELOOP)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

pub(super) fn read_bounded_regular_file(
    path: &std::path::Path,
    max_bytes: u64,
) -> Result<String, BoundedReadError> {
    use std::io::Read;

    let file = match open_no_follow_nonblock(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(BoundedReadError::NotFound);
        }
        Err(error) if is_symlink_open_error(&error) => {
            return Err(BoundedReadError::Symlink);
        }
        Err(error) => return Err(BoundedReadError::Io(error)),
    };
    let meta = file.metadata().map_err(BoundedReadError::Io)?;
    if !meta.is_file() {
        return Err(BoundedReadError::NotAFile);
    }
    if meta.len() > max_bytes {
        return Err(BoundedReadError::TooLarge(meta.len()));
    }
    let mut buf = Vec::with_capacity((meta.len() as usize).min(64 * 1024));
    file.take(max_bytes + 1)
        .read_to_end(&mut buf)
        .map_err(BoundedReadError::Io)?;
    if buf.len() as u64 > max_bytes {
        return Err(BoundedReadError::TooLarge(buf.len() as u64));
    }
    String::from_utf8(buf).map_err(|_| BoundedReadError::Utf8)
}

pub(super) fn is_valid_artifact_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.contains('\0')
}

pub(super) async fn get_artifact(
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
    if !is_valid_artifact_name(&name) {
        return ApiResponse::err(400, "invalid artifact name");
    }
    let filename = if name.ends_with(".txt") {
        name.clone()
    } else {
        format!("{name}.txt")
    };
    let path = dir.join("artifacts").join(&filename);
    // artifact 正文可达 MB 级（超长工具输出落盘）：与 /files?raw 相同的
    // 读取上限，防止内存放大、符号链接穿透与阻塞特殊文件。
    match tokio::task::spawn_blocking(move || read_bounded_regular_file(&path, FILE_RAW_MAX_BYTES))
        .await
    {
        Ok(Ok(text)) => ApiResponse::ok(json!({ "name": filename, "content": text })),
        Ok(Err(BoundedReadError::NotFound)) => {
            ApiResponse::err(404, format!("artifact {name} not found"))
        }
        Ok(Err(BoundedReadError::TooLarge(len))) => ApiResponse::err(
            413,
            format!(
                "artifact {name} is {len} bytes; the REST API caps reads at {FILE_RAW_MAX_BYTES} bytes"
            ),
        ),
        Ok(Err(BoundedReadError::Symlink)) => {
            ApiResponse::err(400, format!("refusing symlink artifact {name}"))
        }
        Ok(Err(BoundedReadError::NotAFile)) => {
            ApiResponse::err(500, format!("failed to read artifact {name}: not a file"))
        }
        Ok(Err(BoundedReadError::Utf8)) => {
            ApiResponse::err(500, format!("artifact {name} is not valid UTF-8"))
        }
        Ok(Err(BoundedReadError::Io(error))) => {
            ApiResponse::err(500, format!("failed to read artifact {name}: {error}"))
        }
        Err(error) => ApiResponse::err(500, format!("artifact read task failed: {error}")),
    }
}

pub(super) async fn get_files(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<FilesQuery>,
) -> ApiResponse {
    let dir = match session_dir_or_err(&state, &id, query.project.as_deref()).await {
        Ok(dir) => dir,
        Err(response) => return response,
    };
    let rel = query.path.unwrap_or_default();
    let raw = query.raw;
    match tokio::task::spawn_blocking(move || -> ApiResponse {
        let cwd = match Registry::read_session_cwd_sync(&dir) {
            Ok(Some(c)) => std::path::PathBuf::from(c),
            Ok(None) => return ApiResponse::err(500, "session metadata cwd is missing"),
            Err(error) => {
                return ApiResponse::err(500, format!("failed to read session cwd: {error}"));
            }
        };
        let Some(target) = resolve_within(&cwd, &rel) else {
            return ApiResponse::err(400, "path escapes the workspace");
        };
        if raw {
            match read_bounded_regular_file(&target, FILE_RAW_MAX_BYTES) {
                Ok(text) => ApiResponse::ok(json!({ "path": rel, "content": text })),
                Err(BoundedReadError::NotFound) => ApiResponse::err(404, "file not found"),
                Err(BoundedReadError::Symlink) => ApiResponse::err(400, "refusing symlink"),
                Err(BoundedReadError::NotAFile) => ApiResponse::err(400, "not a file"),
                Err(BoundedReadError::TooLarge(len)) => {
                    ApiResponse::err(413, format!("file too large ({len} bytes, limit 1 MiB)"))
                }
                Err(BoundedReadError::Utf8) => ApiResponse::err(500, "file is not valid UTF-8"),
                Err(BoundedReadError::Io(e)) => ApiResponse::err(500, format!("read failed: {e}")),
            }
        } else {
            let entries = match std::fs::read_dir(&target) {
                Ok(entries) => entries,
                Err(_) => return ApiResponse::err(404, "directory not found"),
            };
            let mut items = Vec::new();
            let mut truncated = false;
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        return ApiResponse::err(500, format!("directory read failed: {error}"));
                    }
                };
                if items.len() >= MAX_DIR_ENTRIES {
                    truncated = true;
                    break;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                items.push(json!({ "name": name, "dir": is_dir }));
            }
            items.sort_by(|a, b| {
                let da = a["dir"].as_bool().unwrap_or(false);
                let db = b["dir"].as_bool().unwrap_or(false);
                db.cmp(&da)
                    .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
            });
            ApiResponse::ok(json!({
                "path": rel,
                "dir": true,
                "items": items,
                "truncated": truncated,
            }))
        }
    })
    .await
    {
        Ok(resp) => resp,
        Err(error) => ApiResponse::err(500, format!("files task failed: {error}")),
    }
}

/// Read an optional resource file.
///
/// Only `NotFound` means "absent". Every other failure — permission denied,
/// invalid UTF-8, a directory where a file belongs, an interrupted read — is
/// returned so the caller can report a real error instead of an empty state.
pub(super) async fn read_optional(path: &std::path::Path) -> std::io::Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Read one authoritative resource file, naming it when the read fails.
pub(super) async fn resource_text(
    dir: &std::path::Path,
    name: &str,
) -> Result<Option<String>, ApiResponse> {
    match read_optional(&dir.join(name)).await {
        Ok(text) => Ok(text),
        Err(error) => Err(ApiResponse::err(
            500,
            format!("failed to read {name}: {error}"),
        )),
    }
}

/// Join `rel` onto `root`, refusing any path that escapes `root`.
pub(super) fn resolve_within(root: &std::path::Path, rel: &str) -> Option<std::path::PathBuf> {
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
