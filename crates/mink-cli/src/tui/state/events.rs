use super::*;

/// Result of a background clipboard read, delivered back to the TUI loop.
pub(crate) enum TuiUiEvent {
    ImageCaptured(PendingImage),
    ClipboardFailed(String),
    FilePickerReady {
        generation: u64,
        picker: FilePickerState,
    },
    ArtifactReady {
        generation: u64,
        id: String,
        result: Result<ArtifactDetail, String>,
    },
    HistoryLoaded {
        generation: u64,
        state: Box<TuiState>,
    },
    Admission(Box<crate::tui::inbox::AdmissionResult>),
    SubAgentReady {
        generation: u64,
        id: String,
        result: Result<SubAgentDetail, String>,
    },
    SkillsLoaded(Result<Vec<String>, String>),
}
impl Debug for TuiUiEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ImageCaptured(_) => "ImageCaptured",
            Self::ClipboardFailed(_) => "ClipboardFailed",
            Self::FilePickerReady { .. } => "FilePickerReady",
            Self::ArtifactReady { .. } => "ArtifactReady",
            Self::HistoryLoaded { .. } => "HistoryLoaded",
            Self::Admission(_) => "Admission",
            Self::SubAgentReady { .. } => "SubAgentReady",
            Self::SkillsLoaded(_) => "SkillsLoaded",
        })
    }
}

/// Injectable clipboard reader (tests); production uses the platform impl.
pub(crate) type ClipboardReader = std::sync::Arc<
    dyn Fn(
            &std::path::Path,
            &crate::runtime::OpenAiChatImageUrlLimits,
        ) -> anyhow::Result<crate::tui::clipboard::ClipboardPng>
        + Send
        + Sync,
>;

impl TuiState {
    /// Apply one background clipboard result. Failed reads are reported once;
    /// a captured image only grows the pending list (the chip row is the
    /// visible feedback).
    pub(crate) fn apply_ui_event(&mut self, event: TuiUiEvent) {
        match event {
            TuiUiEvent::ImageCaptured(image) => {
                self.async_load.clipboard_started = None;
                // The same bytes stage to the same content-addressed path: a
                // duplicate paste would make the model receive one picture
                // twice (double vision tokens) for no benefit.
                if self
                    .input
                    .pending_images
                    .iter()
                    .any(|pending| pending.path == image.path)
                {
                    // No path in the notice: the transcript would leak the
                    // absolute session path and wrap over several rows.
                    self.push_line(TranscriptItem::new(
                        "Image already queued (same content).".into(),
                        TranscriptKind::Info,
                    ));
                } else {
                    self.input.pending_images.push(image);
                    self.input.revision = self.input.revision.wrapping_add(1);
                }
            }
            TuiUiEvent::ClipboardFailed(message) => {
                self.async_load.clipboard_started = None;
                self.push_line(TranscriptItem::new(
                    format!("Clipboard image unavailable: {message}"),
                    TranscriptKind::Error,
                ));
            }
            TuiUiEvent::FilePickerReady { generation, picker } => {
                if generation == self.async_load.picker_generation && self.overlay.is_some() {
                    self.overlay = Some(ActiveOverlay::FilePicker(picker));
                }
            }
            TuiUiEvent::ArtifactReady {
                generation,
                id,
                result,
            } => {
                if generation == self.async_load.artifact_generation
                    && matches!(self.view, View::Artifact { .. })
                {
                    self.invalidate_detail_resource("artifact:");
                    match result {
                        Ok(detail) => self.mirrors.artifact_detail = Some(detail),
                        Err(error) => {
                            self.mirrors.artifact_detail = Some(ArtifactDetail {
                                id,
                                content: error,
                                truncated: false,
                            });
                        }
                    }
                }
            }
            TuiUiEvent::HistoryLoaded { generation, state } => {
                if generation == self.async_load.background_generation
                    && self.async_load.loading_history
                {
                    let mut history = state.transcript.lines;
                    let offset = history.len();
                    history.append(&mut self.transcript.lines);
                    self.transcript.lines = history;
                    for index in self.transcript.sub_agents.line_by_session.values_mut() {
                        *index += offset;
                    }
                    for (session_id, index) in state.transcript.sub_agents.line_by_session {
                        self.transcript
                            .sub_agents
                            .line_by_session
                            .entry(session_id)
                            .or_insert(index);
                    }
                    for index in self.transcript.active_tools.values_mut() {
                        *index += offset;
                    }
                    self.transcript.unsealed = self
                        .transcript
                        .unsealed
                        .iter()
                        .map(|index| *index + offset)
                        .collect();
                    if self.mirrors.plan.is_none() {
                        self.mirrors.plan = state.mirrors.plan;
                    }
                    if self.mirrors.todos.is_none() {
                        self.mirrors.todos = state.mirrors.todos;
                    }
                    // The read-only goal mirror is loaded from `goal.json` by
                    // the background state; a restored session must show its
                    // goal marker before any `GoalUpdated` arrives.
                    if self.mirrors.goal.is_none() {
                        self.mirrors.goal = state.mirrors.goal;
                    }
                    self.async_load.loading_history = false;
                    self.invalidate_all_cache();
                }
            }
            TuiUiEvent::Admission(result) => self.finish_admission(*result),
            TuiUiEvent::SubAgentReady {
                generation,
                id,
                result,
            } => {
                if generation == self.async_load.sub_detail_generation
                    && matches!(&self.view, View::SubAgentDetail { session_id, .. } if session_id == &id)
                {
                    self.invalidate_detail_resource(&format!("sub:{id}"));
                    self.mirrors.restored_sub_detail = Some((
                        id,
                        result.unwrap_or_else(|error| SubAgentDetail {
                            thinking: String::new(),
                            text: sanitize_tui_text(&error),
                        }),
                    ));
                }
            }
            TuiUiEvent::SkillsLoaded(result) => {
                self.render_skills_result(result);
            }
        }
    }

    pub(crate) fn push_failed_draft(&mut self, draft: InputState) {
        const MAX_FAILED_DRAFTS: usize = 50;
        self.admission.failed_drafts.push(draft);
        if self.admission.failed_drafts.len() > MAX_FAILED_DRAFTS {
            let excess = self.admission.failed_drafts.len() - MAX_FAILED_DRAFTS;
            self.admission.failed_drafts.drain(..excess);
        }
    }

    fn render_skills_result(&mut self, result: Result<Vec<String>, String>) {
        self.push_line(TranscriptItem::new(
            "=== Skills ===".into(),
            TranscriptKind::Info,
        ));
        match result {
            Ok(lines) => {
                for line in lines {
                    self.push_line(TranscriptItem::new(line, TranscriptKind::Text));
                }
            }
            Err(error) => {
                self.push_line(TranscriptItem::new(
                    format!("Error loading skills: {error}"),
                    TranscriptKind::Error,
                ));
            }
        }
        self.push_line(TranscriptItem::new(
            crate::local::SKILL_LOAD_HINT.into(),
            TranscriptKind::Info,
        ));
    }

    pub(crate) fn arm_task_notification(&mut self) {
        self.stream.task_notification_armed = true;
        self.stream.pending_task_notification = None;
    }

    pub(crate) fn finish_task_notification(&mut self, kind: TaskNotificationKind) {
        if !self.stream.task_notification_armed {
            return;
        }
        self.stream.task_notification_armed = false;
        self.stream.pending_task_notification =
            Some(TaskNotification::new(kind, &self.mirrors.model));
    }

    pub(crate) fn take_task_notification(&mut self) -> Option<TaskNotification> {
        self.stream.pending_task_notification.take()
    }

    pub(crate) fn add_help(&mut self) {
        self.view = View::Panel {
            panel: PanelKind::Help,
            scroll: 0,
            selected: 0,
        };
    }
    pub(crate) fn show_skills(&mut self) {
        if let Some(tx) = self.async_load.ui_tx.clone() {
            crate::tui::spawn_blocking_work(move || {
                let result =
                    crate::local::discoverable_skill_lines().map_err(|error| error.to_string());
                let _ = tx.send(TuiUiEvent::SkillsLoaded(result));
            });
            return;
        }
        let result = crate::local::discoverable_skill_lines().map_err(|error| error.to_string());
        self.render_skills_result(result);
    }

    pub(crate) fn open_sub_agent(&mut self, id: &str) {
        self.invalidate_detail_resource(&format!("sub:{id}"));
        self.view = View::SubAgentDetail {
            session_id: id.to_string(),
            scroll: 0,
        };
        self.async_load.sub_detail_generation =
            self.async_load.sub_detail_generation.wrapping_add(1);
        let available = self
            .transcript
            .sub_agents
            .line_by_session
            .get(id)
            .and_then(|index| self.transcript.lines.get(*index))
            .is_some_and(|item| item.sub_detail.is_some());
        if available {
            return;
        }
        let Some(tx) = self.async_load.ui_tx.clone() else {
            return;
        };
        let reader = crate::runtime::session::SessionReader::new(&self.mirrors.session_dir);
        let id = id.to_string();
        let generation = self.async_load.sub_detail_generation;
        self.mirrors.restored_sub_detail = Some((
            id.clone(),
            SubAgentDetail {
                thinking: String::new(),
                text: "Loading…".into(),
            },
        ));
        crate::tui::spawn_blocking_work(move || {
            let result = (|| -> anyhow::Result<SubAgentDetail> {
                let rows = reader
                    .sub_agent(&id)?
                    .conversation_turns(1, 1, true, None)?;
                let mut detail = SubAgentDetail {
                    thinking: String::new(),
                    text: String::new(),
                };
                for row in rows {
                    let (row_thinking, row_text) =
                        crate::runtime::session::SessionReader::decode_subagent_assistant_output(
                            &row,
                        );
                    detail.thinking.push_str(&row_thinking);
                    detail.text.push_str(&row_text);
                }
                detail.text = sanitize_tui_text(&detail.text);
                detail.thinking = sanitize_tui_text(&detail.thinking);
                Ok(detail)
            })()
            .map_err(|error| format!("Cannot load sub-agent {id}: {error}"));
            let _ = tx.send(TuiUiEvent::SubAgentReady {
                generation,
                id,
                result,
            });
        });
    }

    pub(crate) fn open_artifact(&mut self, id: &str) {
        self.invalidate_detail_resource("artifact:");
        const MAX_ARTIFACT_DETAIL_BYTES: usize = 256 * 1024;
        let manager =
            crate::session::artifacts::ArtifactManager::new(self.mirrors.artifacts_dir.clone());
        if let Some(tx) = self.async_load.ui_tx.clone() {
            self.async_load.artifact_generation =
                self.async_load.artifact_generation.wrapping_add(1);
            let generation = self.async_load.artifact_generation;
            let id = id.to_string();
            self.mirrors.artifact_detail = Some(ArtifactDetail {
                id: id.clone(),
                content: "Loading…".into(),
                truncated: false,
            });
            self.view = View::Artifact { scroll: 0 };
            crate::tui::spawn_blocking_work(move || {
                let result = manager
                    .read_text_prefix(&id, MAX_ARTIFACT_DETAIL_BYTES)
                    .map(|(content, truncated)| ArtifactDetail {
                        id: id.clone(),
                        content: sanitize_tui_text(&content),
                        truncated,
                    })
                    .map_err(|error| {
                        sanitize_tui_text(&format!("Cannot open artifact://{id}: {error}"))
                    });
                let _ = tx.send(TuiUiEvent::ArtifactReady {
                    generation,
                    id,
                    result,
                });
            });
            return;
        }
        match manager.read_text_prefix(id, MAX_ARTIFACT_DETAIL_BYTES) {
            Ok((content, truncated)) => {
                self.mirrors.artifact_detail = Some(ArtifactDetail {
                    id: id.to_string(),
                    content: sanitize_tui_text(&content),
                    truncated,
                });
                self.view = View::Artifact { scroll: 0 };
            }
            Err(error) => {
                self.push_line(TranscriptItem::new(
                    format!("Cannot open artifact://{id}: {error}"),
                    TranscriptKind::Error,
                ));
            }
        }
    }
}

/// 识别 LLM 等待心跳消息并提取精简状态标签（如 `·30s`）。
/// 消息格式由 mink-core 统一（`mink::runtime::llm_wait_heartbeat_message`）；
/// 非心跳消息返回 None（按普通告警处理）。
pub(crate) fn heartbeat_status_label(msg: &str) -> Option<String> {
    let elapsed = mink::runtime::parse_llm_wait_heartbeat_elapsed(msg)?;
    Some(format!("·{elapsed}s"))
}

pub(super) fn upsert_todo(
    items: &mut Vec<crate::ui::TodoItemDisplay>,
    update: crate::ui::TodoItemDisplay,
) {
    if let Some(item) = items.iter_mut().find(|item| item.id == update.id) {
        *item = update;
    } else {
        items.push(update);
    }
}

pub(super) fn set_todo_status(
    items: &mut [crate::ui::TodoItemDisplay],
    id: &str,
    status: TodoStatusDisplay,
) {
    if let Some(item) = items.iter_mut().find(|item| item.id == id) {
        item.status = status;
    }
}
