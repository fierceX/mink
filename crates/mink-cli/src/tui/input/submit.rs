use super::*;

pub(super) fn handle_enter(
    state: &mut TuiState,
    orch_tx: &tokio::sync::mpsc::UnboundedSender<RuntimeCmd>,
) -> bool {
    state.input.clamp_cursor();
    if state.edit_input.is_some() {
        if let Err(error) = state.submit_human_input(orch_tx) {
            state.input_rejected(error.to_string());
        }
        return false;
    }
    let parsed = parse_slash_command(&state.input.buf);
    if let Ok(Some(
        command @ (SlashCommand::Help
        | SlashCommand::Status
        | SlashCommand::Inputs
        | SlashCommand::Details
        | SlashCommand::Latest),
    )) = &parsed
    {
        match command {
            SlashCommand::Help => state.add_help(),
            SlashCommand::Status => {
                state.view = View::Panel {
                    panel: PanelKind::Status,
                    scroll: 0,
                    selected: 0,
                }
            }
            SlashCommand::Inputs => {
                state.refresh_inputs();
                state.view = View::Panel {
                    panel: PanelKind::Inputs,
                    scroll: 0,
                    selected: 0,
                };
            }
            SlashCommand::Details => {
                state.view = View::Panel {
                    panel: PanelKind::Details,
                    scroll: 0,
                    selected: 0,
                }
            }
            SlashCommand::Latest => {
                state.viewport.auto_scroll = true;
                state.viewport.anchor = None;
            }
            _ => unreachable!(),
        }
        state.input.buf.clear();
        state.input.cursor = 0;
        return false;
    }
    if state.runtime.is_some() {
        if matches!(parsed, Ok(None)) {
            if let Err(error) = state.submit_human_input(orch_tx) {
                // Admission failures must not finalize the current stream or
                // erase the draft; notices wait for its accepted boundary.
                state.input_rejected(format!("Input rejected: {error}"));
            }
            return false;
        }
        if let Ok(Some(
            command @ (SlashCommand::Inputs | SlashCommand::Resume(_) | SlashCommand::Withdraw(_)),
        )) = &parsed
        {
            if let Err(error) = state.inbox_command(command, orch_tx) {
                state.input_rejected(error.to_string());
            }
            return false;
        }
        if state.active_turn_id.is_some()
            && matches!(
                parsed,
                Ok(Some(
                    SlashCommand::Compact
                        | SlashCommand::Flash
                        | SlashCommand::Pro
                        | SlashCommand::Model(_)
                ))
            )
        {
            state.input_rejected("Control command unavailable during a turn; draft kept.".into());
            return false;
        }
    }
    let typed = std::mem::take(&mut state.input.buf);
    let images = std::mem::take(&mut state.input.pending_images);
    state.input.cursor = 0;
    if typed.is_empty() && images.is_empty() {
        return false;
    }
    if !typed.is_empty() {
        state.input.push_history(typed.clone());
    }
    state.input.history_idx = None;
    // 提交新输入前先封口上一轮未结束的流式内容，保证用户输入始终显示在
    // 已展示内容之后，避免被后续到达的 finalize 插入到错误位置。
    state.finalize_stream();
    state.push_line(TranscriptItem::new(
        format!("> {}", display_user_input(&typed, &images)),
        TranscriptKind::Info,
    ));
    match parse_slash_command(&typed) {
        Ok(Some(command)) => {
            if !images.is_empty() {
                // Slash commands are local UI/runtime actions, not model
                // turns: keep the images queued for the next real message.
                state.input.pending_images = images;
                state.push_line(TranscriptItem::new(
                    "Queued image(s) were not attached to a slash command; they stay queued for the next message."
                        .into(),
                    TranscriptKind::Info,
                ));
            }
            match command {
                SlashCommand::Goal(command) => {
                    let _ = orch_tx.send(RuntimeCmd::Goal(command));
                }
                SlashCommand::Flash => {
                    let _ = orch_tx.send(RuntimeCmd::SetModel("flash".into()));
                }
                SlashCommand::Pro => {
                    let _ = orch_tx.send(RuntimeCmd::SetModel("pro".into()));
                }
                SlashCommand::Model(model) => {
                    let _ = orch_tx.send(RuntimeCmd::SetModel(model));
                }
                SlashCommand::Compact => {
                    if orch_tx.send(RuntimeCmd::Compact).is_ok() {
                        state.arm_task_notification();
                        state.work_state = WorkState::Compacting;
                    } else {
                        state.push_line(TranscriptItem::new(
                            "Failed to send compact command.".into(),
                            TranscriptKind::Error,
                        ));
                    }
                }
                SlashCommand::Help => state.add_help(),
                SlashCommand::Skills => state.show_skills(),
                SlashCommand::Plan => state.view = View::Plan { scroll: 0 },
                SlashCommand::Goals => state.view = View::Goal { scroll: 0 },
                SlashCommand::Todos => state.view = View::Todos { scroll: 0 },
                SlashCommand::Status
                | SlashCommand::Latest
                | SlashCommand::Details
                | SlashCommand::Inputs
                | SlashCommand::Resume(_)
                | SlashCommand::Withdraw(_) => {
                    state.push_line(TranscriptItem::new(
                        "TUI runtime unavailable.".into(),
                        TranscriptKind::Error,
                    ));
                }
                SlashCommand::SubAgent(session_id) => state.open_sub_agent(&session_id),
                SlashCommand::Artifact(id) => state.open_artifact(&id),
                SlashCommand::Quit => {
                    state.quit = true;
                    return true;
                }
            }
        }
        Ok(None) => {
            let input = submitted_user_input(&typed, &images);
            if orch_tx.send(RuntimeCmd::Run { input, done: None }).is_ok() {
                state.arm_task_notification();
                state.work_state = WorkState::WaitingModel;
            } else {
                // The runtime channel is closed: put the text and the queued
                // images back so a failed send never silently discards them.
                state.input.buf = typed;
                state.input.cursor = state.input.buf.len();
                state.input.pending_images = images;
                state.push_line(TranscriptItem::new(
                    "Failed to send user input.".into(),
                    TranscriptKind::Error,
                ));
            }
        }
        Err(error) => {
            if !images.is_empty() {
                state.input.pending_images = images;
            }
            // A parser that knows why says so (a mistyped subcommand names the
            // command it probably meant); everything else keeps the generic hint.
            state.push_line(TranscriptItem::new(
                match error.message {
                    Some(message) => format!("{message} Prefix with a space to send it as text."),
                    None => "Unknown command. Prefix with a space to send it as text.".into(),
                },
                TranscriptKind::Info,
            ));
        }
    }
    state.viewport.auto_scroll = true;
    false
}
