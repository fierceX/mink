//! Typed terminal failure reasons for one turn.
//!
//! Hosts decide retry, alerting and billing from these codes instead of
//! parsing `TurnOutcome.error`. The classification is deliberately conservative:
//! a failure without a typed source is `Internal`, never a guessed provider or
//! context category.

use crate::llm::recovery::{LlmUpstreamError, UpstreamFailureKind};
use crate::session::compaction::CompactionDeadlineExpired;
use crate::session::persistence::PersistenceFaultError;
use serde::{Deserialize, Serialize};

/// Why a turn ended without reaching a normal stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TurnFailureReason {
    /// The whole logical request deadline elapsed (including compaction work).
    RequestTimeout,
    /// Recoverable attempts exhausted the configured retry budget.
    RequestRetryExhausted,
    /// The provider permanently rejected the request (auth, schema, ...).
    ProviderRejected,
    /// The provider returned a terminal stop such as `error` or a refusal.
    ProviderStopped,
    /// The provider reported a context overflow that could not be shrunk.
    ContextOverflow,
    /// Compaction and the emergency checkpoint could not fit the request.
    ContextUnavailable,
    /// The response-format recovery window is exhausted.
    FormatRecoveryExhausted,
    /// The turn hit `max_turns` before the model ended it.
    MaxTurnsExceeded,
    /// The reliability policy aborted the turn without a handover.
    SignalAborted,
    /// The reliability policy handed the turn over to the user/host.
    SignalHandover,
    /// The goal review budget is exhausted.
    GoalBudgetExhausted,
    /// A session state publication fault latched the session.
    Persistence,
    /// No typed source matched; the error text is the only diagnostic.
    Internal,
}

impl TurnFailureReason {
    /// Stable wire code. Must match the serde representation exactly.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequestTimeout => "request_timeout",
            Self::RequestRetryExhausted => "request_retry_exhausted",
            Self::ProviderRejected => "provider_rejected",
            Self::ProviderStopped => "provider_stopped",
            Self::ContextOverflow => "context_overflow",
            Self::ContextUnavailable => "context_unavailable",
            Self::FormatRecoveryExhausted => "format_recovery_exhausted",
            Self::MaxTurnsExceeded => "max_turns_exceeded",
            Self::SignalAborted => "signal_aborted",
            Self::SignalHandover => "signal_handover",
            Self::GoalBudgetExhausted => "goal_budget_exhausted",
            Self::Persistence => "persistence",
            Self::Internal => "internal",
        }
    }
}

impl std::fmt::Display for TurnFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Internal marker attached to an error at the point where the cause is known.
#[derive(Debug)]
struct TurnFailureTagged {
    reason: TurnFailureReason,
    source: anyhow::Error,
}

impl std::fmt::Display for TurnFailureTagged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Keep the original diagnostic text unchanged for hosts and tests.
        write!(f, "{}", self.source)
    }
}

impl std::error::Error for TurnFailureTagged {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Attach a typed reason without discarding the original error chain.
pub(crate) fn tag_failure(reason: TurnFailureReason, error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(TurnFailureTagged {
        reason,
        source: error,
    })
}

/// Classify a fatal turn error. String matching is never used.
pub(crate) fn classify_turn_error(error: &anyhow::Error) -> TurnFailureReason {
    if let Some(tag) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TurnFailureTagged>())
    {
        return tag.reason;
    }
    for cause in error.chain() {
        if cause
            .downcast_ref::<crate::agent::turn::ContextOverflowError>()
            .is_some()
        {
            return TurnFailureReason::ContextOverflow;
        }
        if cause.downcast_ref::<CompactionDeadlineExpired>().is_some() {
            return TurnFailureReason::RequestTimeout;
        }
        if cause.downcast_ref::<PersistenceFaultError>().is_some() {
            return TurnFailureReason::Persistence;
        }
        if cause
            .downcast_ref::<crate::runtime::goal::review::GoalReviewBudgetExhausted>()
            .is_some()
        {
            return TurnFailureReason::GoalBudgetExhausted;
        }
        if let Some(upstream) = cause.downcast_ref::<LlmUpstreamError>() {
            return match upstream.kind() {
                UpstreamFailureKind::Permanent => TurnFailureReason::ProviderRejected,
                _ => TurnFailureReason::Internal,
            };
        }
    }
    TurnFailureReason::Internal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_matches_serde_for_every_variant() {
        let reasons = [
            TurnFailureReason::RequestTimeout,
            TurnFailureReason::RequestRetryExhausted,
            TurnFailureReason::ProviderRejected,
            TurnFailureReason::ProviderStopped,
            TurnFailureReason::ContextOverflow,
            TurnFailureReason::ContextUnavailable,
            TurnFailureReason::FormatRecoveryExhausted,
            TurnFailureReason::MaxTurnsExceeded,
            TurnFailureReason::SignalAborted,
            TurnFailureReason::SignalHandover,
            TurnFailureReason::GoalBudgetExhausted,
            TurnFailureReason::Persistence,
            TurnFailureReason::Internal,
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!("\"{}\"", reason.as_str()));
        }
    }

    #[test]
    fn tag_wins_over_other_sources() {
        let error = tag_failure(
            TurnFailureReason::ContextUnavailable,
            anyhow::anyhow!("over budget"),
        );
        assert_eq!(
            classify_turn_error(&error),
            TurnFailureReason::ContextUnavailable
        );
    }

    #[test]
    fn unknown_errors_are_internal() {
        assert_eq!(
            classify_turn_error(&anyhow::anyhow!("something broke")),
            TurnFailureReason::Internal
        );
    }

    #[test]
    fn typed_sources_classify_without_message_matching() {
        let provider = anyhow::Error::new(LlmUpstreamError::permanent("rejected"));
        assert_eq!(
            classify_turn_error(&provider),
            TurnFailureReason::ProviderRejected
        );

        let overflow = anyhow::Error::new(crate::agent::turn::ContextOverflowError::new(
            "maximum context length",
        ));
        assert_eq!(
            classify_turn_error(&overflow),
            TurnFailureReason::ContextOverflow
        );

        assert_eq!(
            classify_turn_error(&CompactionDeadlineExpired::error()),
            TurnFailureReason::RequestTimeout
        );

        let persistence = anyhow::Error::new(PersistenceFaultError {
            path: std::path::PathBuf::from("/tmp/state.json"),
            detail: "sync failed".into(),
        });
        assert_eq!(
            classify_turn_error(&persistence),
            TurnFailureReason::Persistence
        );

        let goal = anyhow::Error::new(crate::runtime::goal::review::GoalReviewBudgetExhausted {
            reason: "token budget".into(),
        });
        assert_eq!(
            classify_turn_error(&goal),
            TurnFailureReason::GoalBudgetExhausted
        );
    }
}
