use crate::store::{GOAL_MESSAGE_SOURCE, GoalMessage, GoalSnapshot};
use mink::runtime::TurnId;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const GOAL_UPDATED_EVENT: &str = "goal_updated";
pub const GOAL_FINISHED_EVENT: &str = "goal_finished";

#[derive(Debug, Clone)]
pub enum GoalEvent {
    Updated(Box<GoalSnapshot>),
    Finished(Box<GoalSnapshot>),
}

/// Decode an extension event payload published by [`crate::GoalEngine`] or [`crate::store`].
pub fn parse_goal_event(kind: &str, payload: &Value) -> Option<GoalEvent> {
    let raw = payload.get("goal").unwrap_or(payload);
    let snapshot: GoalSnapshot = serde_json::from_value(raw.clone()).ok()?;
    match kind {
        GOAL_UPDATED_EVENT => Some(GoalEvent::Updated(Box::new(snapshot))),
        GOAL_FINISHED_EVENT => Some(GoalEvent::Finished(Box::new(snapshot))),
        _ => None,
    }
}

pub fn is_goal_turn_id(id: &TurnId) -> bool {
    id.as_str().starts_with("goal-")
}

pub fn goal_path(session_dir: &Path) -> PathBuf {
    session_dir.join("goal.json")
}

#[derive(Debug, Clone, PartialEq)]
pub enum GoalBoundaryEvent {
    Card(Box<GoalMessage>),
    Ended { goal_id: String, revision: u64 },
}

/// Decode a goal boundary carried on a committed conversation row, if any.
pub fn decode_goal_boundary(row: &Value) -> Option<GoalBoundaryEvent> {
    let metadata = row.get("_mink")?;
    if let Some(raw) = metadata.get("goal_message") {
        let parsed: GoalMessage = serde_json::from_value(raw.clone()).ok()?;
        return Some(GoalBoundaryEvent::Card(Box::new(parsed)));
    }
    if metadata.get("goal_state_kind").and_then(Value::as_str) == Some("ended") {
        let goal_id = metadata.get("goal_id").and_then(Value::as_str)?.to_string();
        let revision = metadata
            .get("goal_revision")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        return Some(GoalBoundaryEvent::Ended { goal_id, revision });
    }
    None
}

/// Whether a committed conversation row contains goal boundary metadata that
/// transcript replay must preserve.
pub fn row_needs_goal_replay(row: &Value) -> bool {
    let metadata = &row["_mink"];
    metadata["source"] == GOAL_MESSAGE_SOURCE
        || metadata.get("goal_message").is_some()
        || metadata.get("goal_state_kind").is_some()
}
