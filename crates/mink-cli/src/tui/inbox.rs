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
        self.input_notice = Some(super::sanitize::sanitize_tui_text(&message));
        if self.streaming {
            self.pending_infos.push(message);
        } else {
            self.push_line(TranscriptItem::new(message, TranscriptKind::Error));
        }
    }
    pub(crate) fn refresh_inputs(&mut self) {
        let Some(runtime) = &self.runtime else { return };
        let inputs = runtime.handle.input_inbox().outstanding();
        if self
            .inputs
            .iter()
            .map(|i| (&i.input_id, i.revision, i.status))
            .collect::<Vec<_>>()
            != inputs
                .iter()
                .map(|i| (&i.input_id, i.revision, i.status))
                .collect::<Vec<_>>()
        {
            self.inputs = inputs;
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
        self.viewport.auto_scroll = true;
        Ok(())
    }

    pub(crate) fn submit_human_input(
        &mut self,
        tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
    ) -> anyhow::Result<()> {
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
                let lines = self
                    .inputs
                    .iter()
                    .map(|i| {
                        format!(
                            "{} · {} · {}",
                            i.input_id,
                            input_status(i.status),
                            compact_user_input_for_display(&i.input.text)
                        )
                    })
                    .collect::<Vec<_>>();
                self.push_line(TranscriptItem::new(
                    "Inputs: /resume ID starts a new turn; /withdraw ID removes a pending input."
                        .into(),
                    TranscriptKind::Info,
                ));
                for line in lines {
                    self.push_line(TranscriptItem::new(line, TranscriptKind::Info));
                }
            }
            SlashCommand::Resume(id) | SlashCommand::Withdraw(id) => {
                let receipt = self
                    .inputs
                    .iter()
                    .find(|i| &i.input_id == id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Outstanding input not found; use /inputs."))?;
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
        self.input.buf.clear();
        self.input.cursor = 0;
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
