use super::*;
use crate::tui::command::{SlashCommand, parse_slash_command};
use crate::tui::file_picker::{FilePickCandidate, FilePickerPolicy, FilePickerState};
use crate::tui::input::{handle_ctrl_c, handle_event};
use crate::tui::markdown::{
    InlineNode, MdBlock, TableAlign, TableRows, normalize_markdown_input, parse_blocks, push_msg,
    push_msg_with_tool, push_msg_with_width, render_table, strip_ansi, wrap_lines_word,
};
use crate::tui::notify::TaskNotificationKind;
use crate::tui::render::status::status_text_width;
use crate::tui::render::{
    build_status_line, build_status_spans, collapsed_summary, content_viewport_height,
    detail_lines_for_session, detail_viewport_height, split_at_visual_width, status_goal_span,
    transcript_item_lines, visible_lines,
};
use crate::tui::state::{
    ActiveOverlay, CollapsePolicy, TranscriptItem, TranscriptKind, TuiState, TuiUiEvent, View,
    WorkState,
};
use crate::ui::GoalDisplay;

use crate::ui::{Display, ToolResultDisplay, ToolResultKind};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    style::{Color, Style},
    text::{Line, Span},
};
use std::path::PathBuf;

fn unique_test_dir(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mink-tui-{name}-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

fn pending_image(name: &str) -> crate::tui::state::PendingImage {
    crate::tui::state::PendingImage {
        path: PathBuf::from(format!("/tmp/{name}.png")),
        width: 12,
        height: 8,
        bytes: 3456,
    }
}

fn fake_clipboard_reader() -> crate::tui::state::ClipboardReader {
    std::sync::Arc::new(
        |_: &std::path::Path,
         _: &mink::runtime::OpenAiChatImageUrlLimits|
         -> anyhow::Result<crate::tui::clipboard::ClipboardPng> {
            Ok(crate::tui::clipboard::ClipboardPng {
                bytes: b"fake-png".to_vec(),
                width: 12,
                height: 8,
            })
        },
    )
}

fn line_text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn recv_ui_event_timeout(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<TuiUiEvent>,
    timeout: std::time::Duration,
) -> Option<TuiUiEvent> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match rx.try_recv() {
            Ok(event) => return Some(event),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
}

fn goal_display() -> GoalDisplay {
    GoalDisplay {
        draft_duration_secs: None,
        revision: 1,
        original_request: "ship the release".into(),
        subgoals: Vec::new(),
        criteria: Vec::new(),
        contract_revision: 1,
        reviews: Vec::new(),
        review_stage: None,
        continuations: Vec::new(),
        review_candidate: None,
        id: "goal-1".into(),
        // Core derives this; the TUI renders it rather than recomputing.
        budget: mink::runtime::GoalBudget {
            max_turns: 20,
            turns_used: 8,
            remaining_turns: 12,
            deadline_unix_secs: 1_800_000_000,
            remaining_secs: 5_400,
            blocked_after_consecutive_rounds: 3,
            blocked_rounds: 0,
            blocker_rounds: 0,
            soft_token_limit: None,
        },
        status: "active".into(),
        phase: "executing".into(),
        objective: "ship the release".into(),
        acceptance: "model_reported".into(),
        acceptance_criteria: vec!["cut a tag".into()],
        scope: String::new(),
        constraints: vec!["no network".into()],
        completion_basis: None,
        remaining_turns: 12,
        max_turns: 20,
        deadline_unix_secs: 1_800_000_000,
        soft_token_limit: None,
        iterations: 8,
        reason: None,
        hidden: false,
    }
}

fn command_name(command: &super::command::SlashCommand) -> &'static str {
    use super::command::SlashCommand::*;
    match command {
        Goal(_) => "goal",
        Model(_) => "model",
        Resume(_) => "resume",
        Withdraw(_) => "withdraw",
        SubAgent(_) => "subagent",
        Artifact(_) => "artifact",
        Flash => "flash",
        Pro => "pro",
        Compact => "compact",
        Help => "help",
        Skills => "skills",
        Plan => "plan",
        Todos => "todos",
        Inputs => "inputs",
        Status => "status",
        Latest => "latest",
        Details => "details",
        Goals => "goals",
        Quit => "quit",
    }
}

fn goal_message(
    kind: crate::runtime::GoalMessageKind,
    iteration: u32,
    feedback: Option<&str>,
) -> crate::runtime::GoalMessage {
    crate::runtime::GoalMessage {
        kind,
        goal_id: "goal-1".into(),
        contract_revision: 2,
        iteration,
        objective: "ship the parser".into(),
        acceptance_criteria: vec!["parses fixtures".into()],
        constraints: vec!["no network".into()],
        prior_decision: None,
        feedback: feedback.map(str::to_owned),
    }
}

fn goal_card_lines(item: &TranscriptItem, width: u16) -> String {
    transcript_item_lines(item, width)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One committed goal boundary row, shaped like `OrchActor::publish_goal_boundary`
/// and `publish_goal_draft_discussion` write it.
fn goal_boundary_row(
    kind: &str,
    iteration: u32,
    feedback: Option<&str>,
    content: &str,
) -> serde_json::Value {
    serde_json::json!({
        "role": "user",
        "content": content,
        "_mink": {
            "internal": true,
            "source": if kind == "draft" { "goal_draft" } else { "goal_continuation" },
            "goal_message": {
                "kind": kind,
                "goal_id": "goal-1",
                "contract_revision": 2 + iteration,
                "iteration": iteration,
                "objective": "ship the TUI matrix",
                "acceptance_criteria": ["cards render"],
                "constraints": ["no core changes"],
                "prior_decision": null,
                "feedback": feedback,
            },
        }
    })
}

fn temp_session_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "mink_tui_{tag}_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Screen symbols joined row-major; CJK cells carry a trailing blank, so
/// comparisons for non-ASCII text use the space-stripped form.
fn screen_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

mod file_picker;
mod goal_and_status;
mod input_and_clipboard;
mod markdown;
mod rendering_and_replay;
