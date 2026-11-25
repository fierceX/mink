//! Durable explicit goals; model reports never grant lifecycle authority.
pub use super::goal_protocol::*;
use crate::session::persistence::{PersistenceFault, publish_state};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

mod render;
mod store;
mod transitions;
mod types;

pub use render::{GOAL_MESSAGE_SOURCE, GoalMessage, GoalMessageKind};
pub use store::GoalStore;
pub use types::{
    GoalAcceptance, GoalBudget, GoalCompletionBasis, GoalIteration, GoalLimits, GoalOutcome,
    GoalPhase, GoalReport, GoalReportKind, GoalSnapshot, GoalSpec, GoalStatus, default_limits,
    goal_now, read_snapshot,
};

pub(crate) use render::{GoalStateKind, round_prompt, state_text};
#[allow(unused_imports)]
pub(crate) use store::{ApplyResult, IterationKey, SettlementFacts};
pub(crate) use transitions::{GoalChangeError, GoalReviewFailureStatus, GoalTransition};
pub(crate) use types::GOAL_FORMAT_VERSION;
#[cfg(test)]
use types::State;

#[cfg(test)]
#[path = "../goal_tests.rs"]
mod tests;

/// Old records predate the optional outcome marker and retain their existing semantics.
pub(crate) fn effect_outcome_known(metadata: Option<&serde_json::Value>) -> bool {
    metadata.and_then(|m| m.get("execution_outcome")).is_none()
}
