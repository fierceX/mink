//! TUI admission uses the same durable runtime inbox as the Web surface.
use super::command::SlashCommand;
use super::state::{
    TranscriptItem, TranscriptKind, TuiState, WorkState, compact_user_input_for_display,
    display_user_input, submitted_user_input,
};
use crate::cli::RuntimeCmd;
use crate::runtime::{HumanInput, InputReceipt, InputStatus};

impl TuiState {
    pub(crate) fn input_rejected(&mut self, message: String) {
        self.invalidate_detail_resource("panel:Inputs");
        self.input_notice = Some(super::sanitize::sanitize_tui_text(&message));
        if self.streaming {
            self.pending_infos.push(message);
        } else {
            self.push_line(TranscriptItem::new(message, TranscriptKind::Error));
        }
    }
    pub(crate) fn refresh_inputs(&mut self) {
        let Some(runtime) = &self.runtime else { return };
        let source = runtime.handle.input_inbox();
        if self
            .inbox_source
            .as_ref()
            .is_none_or(|old| !std::sync::Arc::ptr_eq(old, source))
        {
            self.inbox_source = Some(source.clone());
            let mut subscription = source.subscribe();
            self.inputs = subscription.borrow_and_update().as_ref().clone();
            self.input_subscription = Some(subscription);
            self.invalidate_detail_resource("panel:Inputs");
            self.invalidate_detail_resource("panel:Status");
            self.dirty = true;
        } else if let Some(subscription) = self.input_subscription.as_mut()
            && subscription.has_changed().unwrap_or(false)
        {
            self.inputs = subscription.borrow_and_update().as_ref().clone();
            self.invalidate_detail_resource("panel:Inputs");
            self.invalidate_detail_resource("panel:Status");
            self.dirty = true;
        }
    }

    fn accept_stream(
        &mut self,
        receipt: &InputReceipt,
        stream: Option<crate::runtime::AgentEventStream>,
        tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
    ) -> anyhow::Result<()> {
        if let Some(stream) = stream {
            let signals = self.runtime.as_ref().expect("TUI runtime").signals.clone();
            tx.send(RuntimeCmd::RenderTuiStream { stream, signals })
                .map_err(|_| {
                    anyhow::anyhow!("Runtime channel closed; input retained in /inputs.")
                })?;
            self.active_turn_id = Some(receipt.turn_id.clone());
            self.stopping = false;
            self.work_state = WorkState::WaitingModel;
            self.arm_task_notification();
        }
        self.refresh_inputs();
        Ok(())
    }

    pub(crate) fn submit_human_input(
        &mut self,
        tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
    ) -> anyhow::Result<()> {
        if self.admission_pending {
            return Ok(());
        }
        if self.edit_input.is_some() {
            return self.save_input_edit(tx);
        }
        if self.stopping {
            anyhow::bail!("Turn is stopping; draft kept.")
        }
        if self.input.buf.is_empty() && self.input.pending_images.is_empty() {
            return Ok(());
        }
        if tx.is_closed() {
            anyhow::bail!("Runtime channel closed; draft kept.")
        }
        let runtime = &self.runtime.as_ref().expect("TUI runtime").handle;
        let request_id = format!(
            "tui-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        );
        let input = HumanInput {
            request_id,
            text: submitted_user_input(&self.input.buf, &self.input.pending_images),
            target_turn_id: self.active_turn_id.clone(),
            attachment_ids: self
                .input
                .pending_images
                .iter()
                .filter_map(|image| {
                    image
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(str::to_owned)
                })
                .collect(),
        };
        if let Some(ui_tx) = self.ui_tx.clone() {
            self.admission_pending = true;
            self.admission_cancelled = false;
            if self.active_turn_id.is_none() {
                self.work_state = WorkState::WaitingModel;
            }
            self.viewport.auto_scroll = true;
            self.viewport.anchor = None;
            let runtime = runtime.clone();
            let handle = tokio::runtime::Handle::current();
            let tx = tx.clone();
            let draft = self.input.draft_snapshot();
            std::thread::spawn(move || {
                let _entered = handle.enter();
                let result = runtime
                    .stream_input(input)
                    .map_err(|e| format!("Input rejected: {e}"));
                let _ = ui_tx.send(super::state::TuiUiEvent::Admission(Box::new(
                    AdmissionResult {
                        draft,
                        result,
                        tx,
                        action: AdmissionAction::Submit,
                    },
                )));
            });
            return Ok(());
        }
        let (receipt, stream) = runtime.stream_input(input)?;
        self.accept_stream(&receipt, stream, tx)?;
        if !receipt.guidance {
            self.finalize_stream();
            self.push_line(TranscriptItem::new(
                format!(
                    "> {}",
                    display_user_input(&self.input.buf, &self.input.pending_images)
                ),
                TranscriptKind::Info,
            ));
        }
        if !self.input.buf.is_empty() {
            self.input.history.push(self.input.buf.clone());
        }
        self.input.buf.clear();
        self.input.cursor = 0;
        self.input.pending_images.clear();
        self.input.history_idx = None;
        self.input_notice = None;
        self.input.undo.clear();
        self.input.redo.clear();
        self.viewport.auto_scroll = true;
        self.viewport.anchor = None;
        Ok(())
    }

    pub(crate) fn finish_admission(&mut self, completion: AdmissionResult) {
        self.admission_pending = false;
        let same_draft = self.input.revision == completion.draft.revision
            && self.input.buf == completion.draft.buf
            && self.input.pending_images == completion.draft.pending_images;
        match completion.result {
            Ok((receipt, stream)) => {
                let consumes_text = matches!(
                    completion.action,
                    AdmissionAction::Submit | AdmissionAction::Edit
                ) || super::command::parse_slash_command(&completion.draft.buf)
                    .ok()
                    .flatten()
                    .is_some_and(|command| match (&completion.action, command) {
                        (AdmissionAction::Resume, SlashCommand::Resume(id))
                        | (AdmissionAction::Update, SlashCommand::Withdraw(id)) => {
                            id == receipt.input_id
                        }
                        _ => false,
                    });
                if matches!(
                    completion.action,
                    AdmissionAction::Update | AdmissionAction::Edit
                ) {
                    self.refresh_inputs();
                    if matches!(completion.action, AdmissionAction::Edit) {
                        self.edit_input = None;
                    }
                } else if let Err(error) = self.accept_stream(&receipt, stream, &completion.tx) {
                    self.input_rejected(error.to_string());
                    return;
                }
                if self.admission_cancelled {
                    if let Some(runtime) = &self.runtime {
                        runtime.handle.interrupt_current_turn();
                    }
                    self.stopping = self.active_turn_id.is_some();
                    self.admission_cancelled = false;
                }
                if !receipt.guidance && matches!(completion.action, AdmissionAction::Submit) {
                    self.finalize_stream();
                    self.push_line(TranscriptItem::new(
                        format!(
                            "> {}",
                            display_user_input(
                                &completion.draft.buf,
                                &completion.draft.pending_images
                            )
                        ),
                        TranscriptKind::Info,
                    ));
                }
                if matches!(completion.action, AdmissionAction::Resume) {
                    self.finalize_stream();
                    self.push_line(TranscriptItem::new(
                        format!("> {}", compact_user_input_for_display(&receipt.input.text)),
                        TranscriptKind::Info,
                    ));
                }
                if matches!(completion.action, AdmissionAction::Submit)
                    && !completion.draft.buf.is_empty()
                {
                    self.input.history.push(completion.draft.buf.clone());
                } else if matches!(completion.action, AdmissionAction::Resume) {
                    self.input
                        .history
                        .push(compact_user_input_for_display(&receipt.input.text));
                }
                if same_draft && consumes_text {
                    self.input.buf.clear();
                    self.input.cursor = 0;
                    if matches!(completion.action, AdmissionAction::Submit) {
                        self.input.pending_images.clear();
                    }
                    self.input.undo.clear();
                    self.input.redo.clear();
                    self.input.revision = self.input.revision.wrapping_add(1);
                }
                if consumes_text {
                    self.input.history_idx = None;
                }
                self.input_notice = None;
            }
            Err(error) => {
                if self.active_turn_id.is_none() {
                    self.work_state = WorkState::Idle;
                    self.stopping = false;
                }
                self.admission_cancelled = false;
                if !same_draft && matches!(completion.action, AdmissionAction::Submit) {
                    self.failed_drafts.push(completion.draft);
                }
                self.input_rejected(error);
            }
        }
    }

    fn save_input_edit(
        &mut self,
        tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
    ) -> anyhow::Result<()> {
        let (id, revision) = self.edit_input.clone().expect("edit target");
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("TUI runtime unavailable"))?
            .handle
            .clone();
        let draft = self.input.draft_snapshot();
        if let Some(ui_tx) = self.ui_tx.clone() {
            self.admission_pending = true;
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result = runtime
                    .input_inbox()
                    .update(&id, revision, Some(draft.buf.clone()))
                    .map(|receipt| (receipt, None))
                    .map_err(|error| error.to_string());
                let _ = ui_tx.send(super::state::TuiUiEvent::Admission(Box::new(
                    AdmissionResult {
                        draft,
                        result,
                        tx,
                        action: AdmissionAction::Edit,
                    },
                )));
            });
        } else {
            let receipt = runtime
                .input_inbox()
                .update(&id, revision, Some(draft.buf.clone()))?;
            self.finish_admission(AdmissionResult {
                draft,
                result: Ok((receipt, None)),
                tx: tx.clone(),
                action: AdmissionAction::Edit,
            });
        }
        Ok(())
    }

    pub(crate) fn inbox_command(
        &mut self,
        command: &SlashCommand,
        tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
    ) -> anyhow::Result<()> {
        self.refresh_inputs();
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("TUI runtime unavailable"))?
            .handle
            .clone();
        match command {
            SlashCommand::Inputs => {
                self.view = super::state::View::Panel {
                    panel: super::state::PanelKind::Inputs,
                    scroll: 0,
                    selected: 0,
                };
            }
            SlashCommand::Resume(id) | SlashCommand::Withdraw(id) => {
                let receipt = self
                    .inputs
                    .iter()
                    .find(|i| &i.input_id == id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Outstanding input not found; use /inputs."))?;
                if self.admission_pending {
                    return Ok(());
                }
                if let Some(ui_tx) = self.ui_tx.clone() {
                    let resume = matches!(command, SlashCommand::Resume(_));
                    if resume
                        && (self.active_turn_id.is_some()
                            || self.work_state.is_working()
                            || tx.is_closed())
                    {
                        anyhow::bail!("Cannot resume while working or disconnected.");
                    }
                    self.admission_pending = true;
                    if resume {
                        self.viewport.auto_scroll = true;
                        self.viewport.anchor = None;
                    }
                    let tx = tx.clone();
                    let draft = self.input.draft_snapshot();
                    let handle = tokio::runtime::Handle::current();
                    let id = id.clone();
                    std::thread::spawn(move || {
                        let _entered = handle.enter();
                        let result = if resume {
                            runtime
                                .resume_input(&id, receipt.revision)
                                .map_err(|e| e.to_string())
                        } else {
                            runtime
                                .input_inbox()
                                .update(&id, receipt.revision, None)
                                .map(|receipt| (receipt, None))
                                .map_err(|e| e.to_string())
                        };
                        let _ = ui_tx.send(super::state::TuiUiEvent::Admission(Box::new(
                            AdmissionResult {
                                draft,
                                result: result.map_err(|e| e.to_string()),
                                tx,
                                action: if resume {
                                    AdmissionAction::Resume
                                } else {
                                    AdmissionAction::Update
                                },
                            },
                        )));
                    });
                    return Ok(());
                }
                if matches!(command, SlashCommand::Resume(_)) {
                    if self.active_turn_id.is_some()
                        || self.work_state.is_working()
                        || tx.is_closed()
                    {
                        anyhow::bail!("Cannot resume while working or disconnected.")
                    }
                    let (receipt, stream) = runtime.resume_input(id, receipt.revision)?;
                    self.accept_stream(&receipt, stream, tx)?;
                    self.finalize_stream();
                    self.push_line(TranscriptItem::new(
                        format!("> {}", compact_user_input_for_display(&receipt.input.text)),
                        TranscriptKind::Info,
                    ));
                } else {
                    runtime.input_inbox().update(id, receipt.revision, None)?;
                    self.refresh_inputs();
                }
            }
            _ => unreachable!("inbox command"),
        }
        if super::command::parse_slash_command(&self.input.buf)
            .ok()
            .flatten()
            .as_ref()
            == Some(command)
        {
            self.input.buf.clear();
            self.input.cursor = 0;
        }
        self.input_notice = None;
        Ok(())
    }
}

pub(crate) fn input_status(status: InputStatus) -> &'static str {
    match status {
        InputStatus::Pending => "Accepted; waiting for safe boundary",
        InputStatus::Applying => "Adding to context",
        InputStatus::Unapplied => "Unapplied; /resume ID",
        InputStatus::Applied => "Added to this turn's context",
        InputStatus::Withdrawn => "Withdrawn",
    }
}

pub(crate) struct AdmissionResult {
    pub action: AdmissionAction,
    pub draft: super::state::InputState,
    pub result: Result<(InputReceipt, Option<crate::runtime::AgentEventStream>), String>,
    pub tx: tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
}

#[derive(Clone, Copy)]
pub(crate) enum AdmissionAction {
    Submit,
    Resume,
    Update,
    Edit,
}
