use super::*;

impl Registry {
    pub async fn list(&self) -> Result<Vec<SessionSummary>> {
        let active_runtimes = self
            .lock_active_state()?
            .iter()
            .map(|(locator, session)| (locator.clone(), session.runtime.clone()))
            .collect::<HashMap<_, _>>();
        let home = self.home.clone();
        let mut out = tokio::task::spawn_blocking(move || -> Result<Vec<SessionSummary>> {
            Ok(scan_all_sessions(&home)?
                .into_iter()
                .map(|(dir, metadata, modified)| {
                    let usage = summarize_usage(&dir);
                    summary_from_metadata(metadata, modified, &dir, usage)
                })
                .collect())
        })
        .await??;
        for summary in &mut out {
            let locator = SessionLocator {
                project_key: summary.project_key.clone(),
                id: summary.id.clone(),
            };
            if let Some(runtime) = active_runtimes.get(&locator) {
                summary.status = if runtime.running() {
                    "running"
                } else {
                    "active"
                };
                let detail = runtime.detail();
                summary.last_final = detail.get("last_final").filter(|v| !v.is_null()).cloned();
                summary.phase = detail["phase"].as_str().unwrap_or("closed").into();
            }
        }
        out.sort_by(|a, b| {
            b.modified_secs
                .unwrap_or(0)
                .cmp(&a.modified_secs.unwrap_or(0))
        });
        Ok(out)
    }

    /// Create a session on disk. `name` becomes the session alias (mink's
    /// `UseOrCreate` semantics): the runtime builds the session directory +
    /// metadata, then we shut it down, leaving a disk session visible to both
    /// `list()` and the TUI.
    pub async fn create(&self, name: &str, cwd: &Path) -> RegistryResult<SessionSummary> {
        let name = name.trim().to_string();
        self.create_seeded(&name, cwd, Some(name.clone()), Some(name.clone()), false)
            .await
            .map_err(RegistryError::from_resolution)
    }

    /// Create (or idempotently resolve) a workbench conversation from a client
    /// draft id. The alias stays internal; the display title comes from the
    /// first prompt when there is one. Without a prompt the session is marked
    /// untitled so a later runtime open does not replace it with the alias.
    pub async fn create_draft(
        &self,
        draft_id: &str,
        cwd: &Path,
        first_prompt: Option<&str>,
    ) -> RegistryResult<SessionSummary> {
        let prompt = first_prompt
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .map(str::to_string);
        let title = prompt
            .as_deref()
            .and_then(runtime_session::title_from_prompt);
        self.create_seeded(draft_id, cwd, title, prompt.clone(), prompt.is_none())
            .await
            .map_err(RegistryError::from_resolution)
    }

    /// Fill a display title from the first real text message. The operation
    /// shares open/delete's per-session lock, so a deleted session cannot be
    /// recreated by a late metadata write. An empty prompt (image-only input)
    /// leaves the session untitled for the next text message.
    pub async fn backfill_title(
        &self,
        id: &str,
        project: Option<&str>,
        prompt: &str,
    ) -> RegistryResult<bool> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Ok(false);
        }
        let dir = self.session_dir(id, project).await?;
        let locator = locator_from_dir(&dir, id);
        let operation_lock = self.operation_lock(&locator);
        let _operation = operation_lock.lock().await;
        if !dir.is_dir() {
            return Ok(false);
        }
        let metadata = read_session_metadata(&dir).map_err(RegistryError::Internal)?;
        if metadata.first_prompt.is_some() {
            return Ok(false);
        }
        let Some(title) = runtime_session::title_from_prompt(prompt) else {
            return Ok(false);
        };
        let paths = runtime_session::SessionPaths::from_session_dir(id.to_string(), dir);
        runtime_session::ensure_metadata(
            &paths,
            Path::new(""),
            runtime_session::SessionSeed {
                title: Some(title),
                first_prompt: Some(prompt.to_string()),
                ..Default::default()
            },
        )
        .await
        .map_err(RegistryError::Internal)?;
        Ok(true)
    }

    async fn create_seeded(
        &self,
        alias: &str,
        cwd: &Path,
        title: Option<String>,
        first_prompt: Option<String>,
        untitled: bool,
    ) -> Result<SessionSummary> {
        let alias = alias.trim();
        if alias.is_empty() {
            anyhow::bail!("session alias must not be empty");
        }
        let alias = runtime_session::sanitize_alias(alias)
            .ok_or_else(|| anyhow!("invalid session alias: {alias}"))?;
        let locator = CreateLocator {
            project_key: runtime_session::project_key(cwd),
            alias,
        };
        let create_lock = self.create_lock(&locator);
        let _create = create_lock.lock().await;

        if let Some(record) = runtime_session::resolve_record(
            &self.home,
            cwd,
            &locator.alias,
            mink::runtime::SessionLayout::ProjectScoped,
        )
        .await?
        {
            let session_locator = locator_from_dir(&record.path, &record.id);
            let mut summary = summary_from_metadata(
                Some(record.metadata),
                Some(record.modified),
                &record.path,
                summarize_usage(&record.path),
            );
            if let Some(status) = self.active_status(&session_locator) {
                summary.status = status;
                return Ok(summary);
            }
            let operation_lock = self.operation_lock(&session_locator);
            let _operation = operation_lock.lock().await;
            if let Some(status) = self.active_status(&session_locator) {
                summary.status = status;
            }
            return Ok(summary);
        }

        let session_id = runtime_session::new_session_id();
        let paths = runtime_session::paths_for(&self.home, cwd, &session_id);
        std::fs::create_dir_all(&paths.base_dir)?;
        std::fs::create_dir(&paths.session_dir).map_err(|error| {
            anyhow!(
                "failed to reserve session {}: {error}",
                paths.session_dir.display()
            )
        })?;
        let session_locator = locator_from_dir(&paths.session_dir, &session_id);
        let operation_lock = self.operation_lock(&session_locator);
        let _operation = operation_lock.lock().await;
        let result = async {
            let seed_lease = SessionLease::acquire(paths.session_dir.join(LOCK_FILE))?;
            runtime_session::ensure_metadata(
                &paths,
                cwd,
                runtime_session::SessionSeed {
                    alias: Some(locator.alias.clone()),
                    title,
                    first_prompt,
                    untitled,
                    ..Default::default()
                },
            )
            .await?;
            drop(seed_lease);
            let runtime = SessionRuntime::open_with_goal_plugin(
                self.build_options(&session_id, cwd)?
                    .with_session(SessionPolicy::Resume(session_id.clone())),
                self.build_goal_plugin(),
            )
            .await?;
            runtime.shutdown().await?;
            let metadata = read_session_metadata(&paths.session_dir)?;
            Ok(summary_from_metadata(
                Some(metadata),
                std::fs::metadata(&paths.events)
                    .and_then(|metadata| metadata.modified())
                    .ok(),
                &paths.session_dir,
                summarize_usage(&paths.session_dir),
            ))
        }
        .await;
        if result.is_err()
            && let Ok(_cleanup_lease) = SessionLease::acquire(paths.session_dir.join(LOCK_FILE))
        {
            let _ = std::fs::remove_dir_all(&paths.session_dir);
        }
        result
    }

    /// Open a session: take an exclusive lease, build the runtime, then publish
    /// it in the active map. Idempotent for sessions this server already holds.
    pub(crate) fn read_session_cwd_sync(dir: &Path) -> Result<Option<String>> {
        let metadata = read_session_metadata(dir)?;
        if metadata.cwd.trim().is_empty() {
            anyhow::bail!("session metadata cwd is empty");
        }
        Ok(Some(metadata.cwd))
    }

    /// Find the disk path of a session.
    /// Uses direct O(1) lookup when a safe `project` key is supplied, falling
    /// back to a full-home scan otherwise.
    pub async fn session_dir(&self, id: &str, project: Option<&str>) -> RegistryResult<PathBuf> {
        let home = self.home.clone();
        let id_owned = id.to_string();
        let project_owned = project.map(str::to_string);
        let direct = tokio::task::spawn_blocking(move || -> Option<PathBuf> {
            let proj = project_owned.as_deref()?;
            if !is_safe_path_segment(proj)
                || !is_safe_path_segment(&id_owned)
                || id_owned.starts_with("sub_")
                || id_owned.starts_with("replan_")
            {
                return None;
            }
            let dir = home
                .join(".mink")
                .join("projects")
                .join(proj)
                .join(&id_owned);
            if !dir.is_dir() {
                return None;
            }
            let metadata = read_session_metadata(&dir).ok()?;
            (metadata.id == id_owned && metadata.parent.is_none()).then_some(dir)
        })
        .await
        .map_err(|error| RegistryError::Internal(error.into()))?;
        if let Some(dir) = direct {
            return Ok(dir);
        }

        let home = self.home.clone();
        let scanned = tokio::task::spawn_blocking(move || scan_all_sessions(&home))
            .await
            .map_err(|error| RegistryError::Internal(error.into()))?
            .map_err(RegistryError::Internal)?;
        let candidates = scanned
            .into_iter()
            .filter(|(dir, metadata, _)| {
                metadata.as_ref().is_some_and(|metadata| metadata.id == id)
                    && project.is_none_or(|project| project_key_from_dir(dir) == project)
            })
            .map(|(dir, _, _)| dir)
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [] => Err(RegistryError::NotFound(format!("session {id} not found"))),
            [dir] => Ok(dir.clone()),
            many => Err(RegistryError::Ambiguous(format!(
                "session {id} is ambiguous across projects: {}; pass ?project=<key>",
                many.iter()
                    .map(|path| project_key_from_dir(path))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }
}

pub(super) fn summary_from_metadata(
    metadata: Option<SessionMetadata>,
    modified: Option<SystemTime>,
    dir: &Path,
    usage: (u64, u64, u64, u64),
) -> SessionSummary {
    let corrupt = dir.join("session.json").exists() && metadata.is_none();
    let fallback_id = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let m = metadata.unwrap_or_else(|| SessionMetadata {
        id: fallback_id.clone(),
        alias: None,
        title: None,
        created_at: String::new(),
        updated_at: String::new(),
        cwd: String::new(),
        parent: None,
        first_prompt: None,
        untitled: false,
        summary: None,
        subagent: None,
    });
    let (tokens_in, tokens_out, cache_read_tokens, last_context_tokens) = usage;
    SessionSummary {
        project_key: project_key_from_dir(dir),
        corrupt,
        id: m.id.clone(),
        alias: m.alias.clone(),
        title: m.title.clone(),
        first_prompt: summary_first_prompt(m.first_prompt.as_deref()),
        untitled: m.untitled,
        cwd: m.cwd.clone(),
        created_at: m.created_at.clone(),
        updated_at: m.updated_at.clone(),
        modified_secs: modified
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
        status: "free",
        phase: "closed".into(),
        last_final: None,
        pending_input_count: mink::runtime::session::SessionReader::new(dir)
            .inputs()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|e| {
                        matches!(
                            e.status,
                            mink::runtime::InputStatus::Pending
                                | mink::runtime::InputStatus::Applying
                                | mink::runtime::InputStatus::Unapplied
                        )
                    })
                    .count()
            })
            .unwrap_or(0),
        path: dir.display().to_string(),
        tokens_in,
        tokens_out,
        cache_read_tokens,
        last_context_tokens,
    }
}

pub(super) fn summary_first_prompt(prompt: Option<&str>) -> Option<String> {
    pub(super) const MAX_BYTES: usize = 512;
    let prompt = prompt?;
    if prompt.len() <= MAX_BYTES {
        return Some(prompt.to_string());
    }
    let mut end = MAX_BYTES;
    while end > 0 && !prompt.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…", &prompt[..end]))
}

pub(super) fn is_safe_path_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
}

pub(super) fn project_key_from_dir(dir: &Path) -> String {
    dir.parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

pub(super) fn locator_from_dir(dir: &Path, id: &str) -> SessionLocator {
    SessionLocator {
        project_key: project_key_from_dir(dir),
        id: id.to_string(),
    }
}

pub(super) fn session_cwd_from_dir(dir: &Path) -> Result<PathBuf> {
    let metadata = read_session_metadata(dir)?;
    let cwd = PathBuf::from(metadata.cwd);
    if cwd.as_os_str().is_empty() {
        anyhow::bail!("session metadata cwd is empty in {}", dir.display());
    }
    Ok(cwd)
}

pub(super) fn read_session_metadata(dir: &Path) -> Result<SessionMetadata> {
    let path = dir.join("session.json");
    let text = std::fs::read_to_string(&path).map_err(|error| {
        anyhow!(
            "failed to read session metadata {}: {error}",
            path.display()
        )
    })?;
    serde_json::from_str(&text)
        .map_err(|error| anyhow!("corrupt session metadata {}: {error}", path.display()))
}

/// Scan every project under `~/.mink/projects/` for session directories.
/// Returns (session dir, parsed metadata, mtime of events.jsonl).
pub(super) fn scan_all_sessions(home: &Path) -> Result<Vec<ScannedSession>> {
    let projects = home.join(".mink").join("projects");
    let entries = match std::fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut out = Vec::new();
    for project in entries {
        let project = project?;
        let project_dir = project.path();
        if !project_dir.is_dir() {
            continue;
        }
        let sessions = std::fs::read_dir(&project_dir)?;
        for session in sessions {
            let session = session?;
            let dir = session.path();
            if !dir.is_dir() {
                continue;
            }
            // Legacy children can live beside user sessions, with missing or
            // inherited metadata. Their directory identity remains authoritative.
            let is_child_id = |id: &str| id.starts_with("sub_") || id.starts_with("replan_");
            if is_child_id(&session.file_name().to_string_lossy()) {
                continue;
            }
            let metadata = match std::fs::read_to_string(dir.join("session.json")) {
                Ok(text) => serde_json::from_str::<SessionMetadata>(&text).ok(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if metadata
                .as_ref()
                .is_some_and(|m| is_child_id(&m.id) || m.parent.is_some())
            {
                continue;
            }
            // 活动时间：events.jsonl mtime（更精确），回退到目录 mtime
            let modified = std::fs::metadata(dir.join("events.jsonl"))
                .ok()
                .and_then(|m| m.modified().ok())
                .or_else(|| dir.metadata().ok().and_then(|m| m.modified().ok()));
            out.push((dir, metadata, modified));
        }
    }
    Ok(out)
}
