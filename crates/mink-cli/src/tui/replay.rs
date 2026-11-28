use crate::replay::ReplayEventKind;
use crate::session::store::first_line;
use crate::tui::signal::TuiSignal;
use crate::tui::state::{TranscriptItem, TranscriptKind, TuiState, compact_user_input_for_display};
use crate::ui::{ArtifactDisplay, ToolPresentation, ToolResultKind};
use crate::util::truncate_display;
use std::collections::VecDeque;
use std::io::BufRead;
use std::path::Path;

pub(crate) struct LoadedHistory {
    pub(crate) lines: Vec<TranscriptItem>,
    pub(crate) sub_agent_lines: std::collections::HashMap<String, usize>,
}

pub(crate) fn load_session_history(events_path: &Path) -> LoadedHistory {
    // Guidance is authoritative conversation history, not a new user turn in
    // events.jsonl. Use complete formal turns when that window contains it.
    // Goal boundaries and native web results also live in committed rows.
    if let Some(directory) = events_path.parent()
        && let Ok(rows) = crate::runtime::session::SessionReader::new(directory).conversation_turns(
            1,
            crate::replay::REPLAY_TURNS,
            true,
            None,
        )
        && rows.iter().any(|row| {
            crate::runtime::session::SessionReader::row_needs_conversation_replay(row)
                || mink_goal::row_needs_goal_replay(row)
        })
    {
        return build_history_from_rows(&rows);
    }
    if !events_path.exists() {
        return LoadedHistory {
            lines: Vec::new(),
            sub_agent_lines: std::collections::HashMap::new(),
        };
    }
    let file = match std::fs::File::open(events_path) {
        Ok(file) => file,
        Err(_) => {
            return LoadedHistory {
                lines: Vec::new(),
                sub_agent_lines: std::collections::HashMap::new(),
            };
        }
    };
    let events = load_recent_turn_events(file);
    if events.is_empty() {
        return LoadedHistory {
            lines: Vec::new(),
            sub_agent_lines: std::collections::HashMap::new(),
        };
    }

    build_history_from_events(&events)
}

#[cfg(test)]
pub(crate) fn load_session(events_path: &Path) -> Vec<TranscriptItem> {
    load_session_history(events_path).lines
}

fn push_user_line(state: &mut TuiState, text: &str, guidance: bool) {
    state.finalize_stream();
    let display_input = if guidance {
        format!("[Added to context] {text}")
    } else {
        text.to_string()
    };
    // 粘贴图片的 marker 只保留 `[image]`，避免恢复会话时回显绝对路径。
    let compacted = compact_user_input_for_display(&display_input);
    let preview = truncate_display(first_line(&compacted), 77);
    if !preview.is_empty() {
        state.push_line(TranscriptItem::new(
            format!("> {preview}"),
            TranscriptKind::Info,
        ));
    }
}

fn finalize_loaded_history(mut state: TuiState) -> LoadedHistory {
    state.finalize_stream();
    for item in &mut state.transcript.lines {
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
    LoadedHistory {
        lines: state.transcript.lines,
        sub_agent_lines: state.transcript.sub_agents.line_by_session,
    }
}

fn apply_transcript_entry(state: &mut TuiState, entry: &crate::runtime::session::TranscriptEntry) {
    use crate::runtime::session::TranscriptEntry;
    match entry {
        TranscriptEntry::WebSource(source) => {
            state.finalize_stream();
            state.apply(TuiSignal::Info(source.clone()));
        }
        TranscriptEntry::User { text, guidance } => {
            push_user_line(state, text, *guidance);
        }
        TranscriptEntry::Thinking(content) => {
            state.apply(TuiSignal::Thinking(content.clone()));
        }
        TranscriptEntry::Text(content) => {
            state.apply(TuiSignal::Text(content.clone()));
        }
        TranscriptEntry::ToolCall {
            id,
            name,
            summary,
            input,
        } => {
            state.apply(TuiSignal::ToolCall {
                tool_use_id: (!id.is_empty()).then(|| id.clone()),
                tool_name: name.clone(),
                summary: summary.clone(),
                input: (!input.is_null()).then(|| input.clone()),
            });
        }
        TranscriptEntry::ToolResult {
            tool_use_id,
            tool_name,
            content,
            status,
            exit_code,
            result_kind,
            presentation,
            artifacts,
        } => {
            state.apply(TuiSignal::ToolResult {
                tool_use_id: tool_use_id.clone(),
                tool_name: tool_name.clone(),
                content: content.clone(),
                status: *status,
                exit_code: *exit_code,
                result_kind: *result_kind,
                presentation: presentation.clone(),
                artifacts: artifacts.clone(),
            });
        }
    }
}

fn build_history_from_rows(rows: &[serde_json::Value]) -> LoadedHistory {
    let mut state = TuiState::default();
    for row in rows {
        if let Some(boundary) = mink_goal::decode_goal_boundary(row) {
            match boundary {
                mink_goal::GoalBoundaryEvent::Card(message) => {
                    state.apply(TuiSignal::GoalCard(message));
                }
                mink_goal::GoalBoundaryEvent::Ended { goal_id, revision } => {
                    state.apply(TuiSignal::GoalEnded { goal_id, revision });
                }
            }
            continue;
        }
        for entry in crate::runtime::session::SessionReader::decode_conversation_row(row) {
            apply_transcript_entry(&mut state, &entry);
        }
    }
    finalize_loaded_history(state)
}

#[cfg(test)]
fn build_history_from_transcript(
    entries: &[crate::runtime::session::TranscriptEntry],
) -> LoadedHistory {
    let mut state = TuiState::default();
    for entry in entries {
        apply_transcript_entry(&mut state, entry);
    }
    finalize_loaded_history(state)
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

fn build_history_from_events(events: &[serde_json::Value]) -> LoadedHistory {
    let mut state = TuiState::default();
    for evt in events {
        let kind = crate::replay::classify_event(evt);
        let c = crate::replay::event_content(evt);
        match kind {
            // 前缀快照只用于离线重建，重放不渲染；goal 生命周期由 goal.json
            // 与实时 signal 提供，重放不重复渲染。
            ReplayEventKind::PrefixSnapshot
            | ReplayEventKind::Ignored
            | ReplayEventKind::GoalLifecycle => {}
            ReplayEventKind::UserInput => push_user_line(&mut state, c, false),
            ReplayEventKind::Thinking => state.apply(TuiSignal::Thinking(c.to_string())),
            ReplayEventKind::Text => state.apply(TuiSignal::Text(c.to_string())),
            ReplayEventKind::ToolCall => {
                let name = crate::replay::event_name(evt);
                let summary = crate::replay::build_tool_summary(name, evt);
                state.apply(TuiSignal::ToolCall {
                    tool_use_id: evt
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    tool_name: name.to_string(),
                    summary,
                    input: evt.get("input").cloned(),
                });
            }
            ReplayEventKind::SubAgent => {
                let session_id = evt["session_id"].as_str().unwrap_or_default();
                let status = evt["status"].as_str().unwrap_or("unknown");
                let in_tokens = evt["input_tokens"].as_u64().unwrap_or(0);
                let out_tokens = evt["output_tokens"].as_u64().unwrap_or(0);
                let label = evt["label"].as_str().map(str::to_string);
                let stop_reason = evt["stop_reason"].as_str().map(str::to_string);
                if !session_id.is_empty() {
                    state.apply(TuiSignal::SubAgentStatus {
                        session_id: session_id.to_string(),
                        status: status.to_string(),
                        label,
                        stop_reason,
                        in_tokens,
                        out_tokens,
                    });
                }
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
                state.apply(TuiSignal::ToolResult {
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
                state.apply(TuiSignal::Error(msg.to_string()));
            }
            ReplayEventKind::AssistantMessage | ReplayEventKind::Unknown => {}
        }
    }
    finalize_loaded_history(state)
}

#[cfg(test)]
pub(crate) fn build_lines_from_events(events: &[serde_json::Value]) -> Vec<TranscriptItem> {
    build_history_from_events(events).lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use mink_goal::GoalMessageKind;

    #[test]
    fn web_history_restores_calls_and_sources_from_conversation() {
        let rows = [serde_json::json!({"role":"assistant","content":[
            {"type":"deepseek_web","calls":[{
                "id":"web1", "tool":"DeepSeekReadUrl",
                "action":{"type":"read_link","url":"https://example.test/"},
                "status":"completed", "page":{"url":"https://example.test/"}
            }],"citations":[{"url":"https://example.test/","title":"source","start_index":0,"end_index":6}]},
            {"type":"text","text":"answer"}
        ]})];
        assert!(
            rows.iter()
                .any(crate::runtime::session::SessionReader::row_needs_conversation_replay)
        );
        let entries: Vec<_> = rows
            .iter()
            .flat_map(crate::runtime::session::SessionReader::decode_conversation_row)
            .collect();
        let loaded = build_history_from_transcript(&entries);
        assert!(
            loaded
                .lines
                .iter()
                .any(|line| line.text.contains("DeepSeekReadUrl"))
        );
        assert!(
            loaded
                .lines
                .iter()
                .any(|line| line.text.contains("[Web source] https://example.test/"))
        );
    }

    #[test]
    fn sub_agent_events_replay_preserves_session_index_and_restored_detail_fallback() {
        let events = vec![
            serde_json::json!({"type":"sub_agent","session_id":"sub_42","status":"ok","label":"audit","stop_reason":"completed","input_tokens":10,"output_tokens":5}),
        ];
        let loaded = build_history_from_events(&events);
        assert!(
            loaded.sub_agent_lines.contains_key("sub_42"),
            "replayed history must retain sub-agent session mapping"
        );
        // Also verify `detail_lines_for_session_with_width` renders
        // `restored_sub_detail` even when no live transcript line is mapped.
        let mut state = TuiState::default();
        state.mirrors.restored_sub_detail = Some((
            "sub_disk_only".into(),
            crate::tui::state::SubAgentDetail {
                thinking: "checked files".into(),
                text: "all good".into(),
            },
        ));
        let rendered = crate::tui::render::detail::detail_lines_for_session_with_width(
            &state,
            "sub_disk_only",
            80,
        );
        let joined: String = rendered
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Sub-agent sub_disk_only"), "{joined}");
        assert!(joined.contains("checked files"), "{joined}");
        assert!(joined.contains("all good"), "{joined}");
    }

    #[test]
    fn sub_agent_events_replay_as_transcript_lines() {
        let events = vec![
            serde_json::json!({"type":"sub_agent","session_id":"sub_1","status":"launched","label":"review"}),
            serde_json::json!({"type":"sub_agent","session_id":"sub_1","status":"running","label":"review"}),
            serde_json::json!({"type":"sub_agent","session_id":"sub_1","status":"ok","label":"review","stop_reason":"completed","input_tokens":10,"output_tokens":5}),
        ];
        let items = build_lines_from_events(&events);
        let line = items
            .iter()
            .find(|item| item.kind == TranscriptKind::SubAgent)
            .expect("sub-agent line");
        assert!(line.text.contains("review"), "{}", line.text);
        assert!(line.text.contains("completed"), "{}", line.text);
        assert!(line.text.contains("ok"), "{}", line.text);
        assert!(line.sealed, "terminal status must seal the line");
    }

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
        let items = build_history_from_rows(&rows).lines;
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
            live.apply(TuiSignal::ToolResult {
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
            assert_eq!(live.transcript.lines[0].tool_status, Some(status));
            assert_eq!(replay[0].tool_status, live.transcript.lines[0].tool_status);
        }
    }
}
