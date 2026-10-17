use crate::replay::ReplayEventKind;
use crate::session::store::first_line;
use crate::tui::signal::TuiSignal;
use crate::tui::state::{TranscriptItem, TranscriptKind, TuiState, compact_user_input_for_display};
use crate::ui::{ArtifactDisplay, ToolPresentation, ToolResultKind};
use crate::util::truncate_display;
use std::collections::VecDeque;
use std::io::BufRead;
use std::path::Path;

pub(crate) fn load_session(events_path: &Path) -> Vec<TranscriptItem> {
    // Guidance is authoritative conversation history, not a new user turn in
    // events.jsonl. Use complete formal turns when that window contains it.
    // Goal boundaries are the same: they exist only as committed rows, so a
    // window holding one must be rebuilt from those rows.
    if let Some(directory) = events_path.parent()
        && let Ok(rows) = crate::runtime::session::SessionReader::new(directory).conversation_turns(
            1,
            crate::replay::REPLAY_TURNS,
            true,
            None,
        )
        && rows.iter().any(needs_conversation_rows)
    {
        return build_lines_from_events(&conversation_events(&rows));
    }
    if !events_path.exists() {
        return Vec::new();
    }
    let file = match std::fs::File::open(events_path) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let events = load_recent_turn_events(file);
    if events.is_empty() {
        return Vec::new();
    }

    build_lines_from_events(&events)
}

/// Whether a committed row carries something only conversation rows can show.
///
/// goals live in committed rows: the boundary card (`_mink.goal_message`), the
/// terminal facts row (`goal_state_kind`) and the round prompt all exist only
/// there. Guidance has the same property.
fn needs_conversation_rows(row: &serde_json::Value) -> bool {
    let metadata = &row["_mink"];
    metadata["guidance"] == true
        || metadata.get("goal_message").is_some()
        || metadata.get("goal_state_kind").is_some()
}

/// What one committed row means to the transcript.
#[derive(Debug, PartialEq, Eq)]
enum RowKind {
    /// A human row (or guidance) rendered as user input.
    User {
        guidance: bool,
    },
    /// A runtime-internal row with no transcript representation.
    Internal,
    GoalCard(Box<crate::runtime::GoalMessage>),
    GoalEnded {
        goal_id: String,
        revision: u64,
    },
}

/// Classify a committed row by its structured metadata, never by its prose.
fn classify_row(row: &serde_json::Value) -> RowKind {
    let metadata = &row["_mink"];
    if let Some(raw) = metadata.get("goal_message")
        && let Ok(message) = serde_json::from_value::<crate::runtime::GoalMessage>(raw.clone())
    {
        return RowKind::GoalCard(Box::new(message));
    }
    if metadata
        .get("goal_state_kind")
        .and_then(serde_json::Value::as_str)
        == Some("ended")
    {
        return RowKind::GoalEnded {
            goal_id: metadata["goal_id"].as_str().unwrap_or_default().to_string(),
            revision: metadata["goal_revision"].as_u64().unwrap_or(0),
        };
    }
    if is_internal_row(row) {
        return RowKind::Internal;
    }
    RowKind::User {
        guidance: metadata["guidance"] == true,
    }
}

/// Whether a row is runtime-internal.
///
/// Some families carry an explicit `internal` flag; others are identified by
/// their runtime-owned metadata (`todo_state_kind`, `plan_transaction_id`,
/// goal state rows, `source: goal_*`). None of them is a human turn.
fn is_internal_row(row: &serde_json::Value) -> bool {
    if row["internal"] == true {
        return true;
    }
    let metadata = &row["_mink"];
    if metadata["internal"] == true {
        return true;
    }
    metadata.get("todo_state_kind").is_some()
        || metadata.get("plan_transaction_id").is_some()
        || metadata.get("goal_state_kind").is_some()
        || metadata
            .get("source")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|source| source.starts_with("goal_"))
}

fn conversation_events(rows: &[serde_json::Value]) -> Vec<serde_json::Value> {
    use serde_json::json;
    let mut events = Vec::new();
    for row in rows {
        if row["role"] == "user"
            && let Some(text) = row["content"].as_str()
        {
            match classify_row(row) {
                RowKind::GoalCard(message) => events.push(json!({
                    "type": "goal_card",
                    "goal": message,
                })),
                RowKind::GoalEnded { goal_id, revision } => events.push(json!({
                    "type": "goal_ended",
                    "goal_id": goal_id,
                    "revision": revision,
                })),
                // Checkpoints, todo syncs, plan transitions and adjudication
                // prompts are runtime-internal: they are neither user prose nor
                // transcript cards.
                RowKind::Internal => {}
                RowKind::User { guidance } => {
                    let content = if guidance {
                        format!("[Added to context] {text}")
                    } else {
                        text.to_string()
                    };
                    events.push(json!({"type":"user_input","content":content}));
                }
            }
        }
        if let Some(blocks) = row["content"].as_array() {
            for block in blocks {
                let mut event = block.clone();
                match block["type"].as_str() {
                    Some("thinking") if row["role"] == "assistant" => {
                        event["content"] = block["thinking"].clone();
                    }
                    Some("text") if row["role"] == "assistant" => {
                        event["content"] = block["text"].clone();
                    }
                    Some("tool_use") if row["role"] == "assistant" => {
                        event["type"] = "tool_call".into();
                    }
                    Some("tool_result") => {
                        if let Some(metadata) = block["_mink"].as_object() {
                            event.as_object_mut().unwrap().extend(metadata.clone());
                            event["name"] = block["_mink"]["tool_name"].clone();
                        }
                    }
                    _ => continue,
                }
                events.push(event);
            }
        }
    }
    events
}

fn load_recent_turn_events(file: std::fs::File) -> Vec<serde_json::Value> {
    let mut turns: VecDeque<Vec<serde_json::Value>> = VecDeque::new();
    let mut current: Vec<serde_json::Value> = Vec::new();
    let mut seen_turn = false;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else {
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(evt) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if crate::replay::classify_event(&evt) == ReplayEventKind::UserInput {
            if seen_turn && !current.is_empty() {
                turns.push_back(std::mem::take(&mut current));
                while turns.len() > crate::replay::REPLAY_TURNS {
                    turns.pop_front();
                }
            }
            seen_turn = true;
        }
        if seen_turn {
            current.push(evt);
        }
    }
    if seen_turn && !current.is_empty() {
        turns.push_back(current);
        while turns.len() > crate::replay::REPLAY_TURNS {
            turns.pop_front();
        }
    }
    turns.into_iter().flatten().collect()
}

fn build_lines_from_events(events: &[serde_json::Value]) -> Vec<TranscriptItem> {
    let mut state = TuiState::default();
    for evt in events {
        match crate::replay::event_type(evt) {
            // Boundary rows produced by `conversation_events`; they carry the
            // structured payload through the same reducer the live path uses.
            "goal_card" => {
                if let Ok(message) =
                    serde_json::from_value::<crate::runtime::GoalMessage>(evt["goal"].clone())
                {
                    state.apply(&TuiSignal::GoalCard(Box::new(message)));
                }
                continue;
            }
            "goal_ended" => {
                state.apply(&TuiSignal::GoalEnded {
                    goal_id: evt["goal_id"].as_str().unwrap_or_default().to_string(),
                    revision: evt["revision"].as_u64().unwrap_or(0),
                });
                continue;
            }
            _ => {}
        }
        let kind = crate::replay::classify_event(evt);
        let c = crate::replay::event_content(evt);
        match kind {
            // 前缀快照只用于离线重建，重放不渲染；goal 生命周期由 goal.json
            // 与实时 signal 提供，重放不重复渲染。
            ReplayEventKind::PrefixSnapshot
            | ReplayEventKind::Ignored
            | ReplayEventKind::GoalLifecycle => {}
            ReplayEventKind::UserInput => {
                state.finalize_stream();
                // 粘贴图片的 marker 只保留 `[image]`，避免恢复会话时回显绝对路径。
                let compacted = compact_user_input_for_display(c);
                let preview = truncate_display(first_line(&compacted), 77);
                if !preview.is_empty() {
                    state.push_line(TranscriptItem::new(
                        format!("> {preview}"),
                        TranscriptKind::Info,
                    ));
                }
            }
            ReplayEventKind::Thinking => state.apply(&TuiSignal::Thinking(c.to_string())),
            ReplayEventKind::Text => state.apply(&TuiSignal::Text(c.to_string())),
            ReplayEventKind::ToolCall => {
                let name = crate::replay::event_name(evt);
                let summary = crate::replay::build_tool_summary(name, evt);
                state.apply(&TuiSignal::ToolCall {
                    tool_use_id: evt
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    tool_name: name.to_string(),
                    summary,
                    input: None,
                });
            }
            ReplayEventKind::ToolResult => {
                let tool_name = crate::replay::event_name(evt);
                let presentation = evt
                    .get("presentation")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<ToolPresentation>(value).ok());
                let artifacts = evt
                    .get("artifacts")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<Vec<ArtifactDisplay>>(value).ok())
                    .unwrap_or_default();
                let result_kind = evt
                    .get("result_kind")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<ToolResultKind>(value).ok())
                    .unwrap_or(ToolResultKind::Text);
                state.apply(&TuiSignal::ToolResult {
                    tool_use_id: evt
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    tool_name: tool_name.to_string(),
                    content: c.to_string(),
                    status: evt
                        .get("status")
                        .cloned()
                        .and_then(|value| serde_json::from_value(value).ok()),
                    exit_code: evt
                        .get("exit_code")
                        .and_then(serde_json::Value::as_i64)
                        .and_then(|value| i32::try_from(value).ok()),
                    result_kind,
                    presentation,
                    artifacts,
                });
            }
            ReplayEventKind::Error => {
                let msg = crate::replay::event_message(evt);
                state.apply(&TuiSignal::Error(msg.to_string()));
            }
            ReplayEventKind::AssistantMessage | ReplayEventKind::Unknown => {}
        }
    }
    state.finalize_stream();
    for item in &mut state.lines {
        if !item.sealed {
            item.sealed = true;
            if item.kind == TranscriptKind::Tool {
                item.tool_status = None;
                if item.text.starts_with("[tool]") {
                    item.text
                        .push_str("\nResult unavailable in the persisted event log.");
                }
            }
        }
    }
    state.lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::GoalMessageKind;

    fn goal_row(kind: &str, feedback: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "role": "user",
            // The model-visible prose is deliberately the shape that used to be
            // echoed as a user turn.
            "content": "<current-goal>\nGoal ID: goal-1\nRound: 1/20\n</current-goal>",
            "_mink": {
                "internal": true,
                "source": "goal_continuation",
                "goal_message": {
                    "kind": kind,
                    "goal_id": "goal-1",
                    "contract_revision": 2,
                    "iteration": if kind == "draft" { 0 } else { 1 },
                    "objective": "ship it",
                    "acceptance_criteria": ["a"],
                    "constraints": [],
                    "prior_decision": null,
                    "feedback": feedback,
                },
            }
        })
    }

    #[test]
    fn internal_rows_do_not_replay_as_user_prose() {
        let rows = vec![
            serde_json::json!({"role":"user","content":"real task"}),
            serde_json::json!({
                "role":"user",
                "content":"This is a runtime-generated checkpoint...<compacted-summary>\nold history\n</compacted-summary>",
                "internal": true
            }),
            serde_json::json!({
                "role":"user",
                "content":"<current-todos>\n- do it\n</current-todos>",
                "_mink":{"todo_revision":3,"todo_state_kind":"sync"}
            }),
            serde_json::json!({
                "role":"user",
                "content":"plan confirmed transition",
                "internal": true,
                "_mink":{"plan_transaction_id":"tx-1"}
            }),
            serde_json::json!({
                "role":"user",
                "content":"<goal-review>host adjudication prompt</goal-review>",
                "_mink":{"internal":true,"source":"goal_adjudication","candidate":{"goal_id":"goal-1"}}
            }),
            serde_json::json!({"role":"user","content":"guidance text","_mink":{"guidance":true,"input_id":"i1"}}),
            goal_row("draft", Some("add evidence")),
            goal_row("start", None),
            serde_json::json!({
                "role":"user",
                "content":"<current-goal>ended</current-goal>",
                "_mink":{"internal":true,"goal_id":"goal-1","goal_revision":9,"goal_state_kind":"ended"}
            }),
        ];
        let items = build_lines_from_events(&conversation_events(&rows));
        let text = items
            .iter()
            .map(|item| item.text.clone())
            .collect::<Vec<_>>()
            .join("\n");

        for payload in [
            "<current-goal>",
            "<compacted-summary>",
            "<current-todos>",
            "<goal-review>",
        ] {
            assert!(
                !text.contains(payload),
                "{payload} leaked into the transcript: {text}"
            );
        }
        assert!(text.contains("> real task"), "{text}");
        assert!(text.contains("[Added to context] guidance text"), "{text}");
        assert!(text.contains("目标草案讨论"), "{text}");
        assert!(text.contains("开始目标"), "{text}");
        assert!(text.contains("Goal goal-1 ended (revision 9)"), "{text}");
        assert!(!text.contains("第 0 轮"), "a draft has no round: {text}");

        let card = items
            .iter()
            .find(|item| item.kind == TranscriptKind::Goal)
            .expect("goal boundary card");
        assert_eq!(
            card.goal_message.as_ref().map(|message| message.kind),
            Some(GoalMessageKind::Draft)
        );
    }

    #[test]
    fn goal_lifecycle_events_are_classified_not_unknown() {
        assert_eq!(
            crate::replay::classify_event(&serde_json::json!({"type":"goal_updated"})),
            ReplayEventKind::GoalLifecycle
        );
        assert_eq!(
            crate::replay::classify_event(&serde_json::json!({"type":"usage"})),
            ReplayEventKind::Ignored
        );
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use crate::runtime::{ToolBlocker, ToolFailureKind, ToolStatus};

    #[test]
    fn live_and_replay_preserve_all_authoritative_tool_statuses() {
        for status in [
            ToolStatus::Succeeded,
            ToolStatus::Failed(ToolFailureKind::Timeout),
            ToolStatus::Blocked(ToolBlocker::RecoveryGuard),
            ToolStatus::Interrupted,
        ] {
            let mut live = TuiState::default();
            live.apply(&TuiSignal::ToolResult {
                tool_use_id: Some("call".into()),
                tool_name: "Bash".into(),
                content: "body".into(),
                status: Some(status),
                exit_code: None,
                result_kind: ToolResultKind::Text,
                presentation: None,
                artifacts: Vec::new(),
            });
            let replay = build_lines_from_events(&[serde_json::json!({
                "type": "tool_result", "tool_use_id": "call", "name": "Bash", "content": "body", "status": status,
            })]);
            assert_eq!(live.lines[0].tool_status, Some(status));
            assert_eq!(replay[0].tool_status, live.lines[0].tool_status);
        }
    }
}
