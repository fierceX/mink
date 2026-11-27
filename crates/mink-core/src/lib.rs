//! Public library entry points for embedding mink.
//!
//! The embedding surface is deliberately limited to [`runtime`], [`prelude`],
//! and [`sdk_protocol`]. The optional `config-file` feature also exposes the
//! shared `.minkrc` schema for host applications that need to parse it.

#[cfg(feature = "config-file")]
pub mod config_file;
pub mod runtime;
pub mod sdk_protocol;

/// Common imports for embedded Rust services.
///
/// This facade is the intended stable surface for services that embed mink as
/// a library. It deliberately re-exports runtime types only; lower-level agent,
/// tool, session, and LLM modules remain implementation details shared by the
/// CLI binaries and the library runtime.
pub mod prelude {
    pub use crate::runtime::goal::GoalController;
    pub use crate::runtime::{
        AgentEvent, AgentEventKind, AgentEventStream, AgentOptions, AgentRuntime,
        AgentRuntimeHandle, AgentTool, CompactOutcome, EventSink, LlmBackend, ModelSwitchOutcome,
        OpenAiCompatibleBackend, RuntimeError, RuntimeResult, SessionEvent, SessionEventSink,
        SessionInfo, SessionPolicy, SessionReader, ToolError, ToolOutput, TurnFailureReason,
        TurnId, TurnOutcome, TurnStatus,
    };
}

mod agent;
mod assets;
mod cancel;
mod capabilities;
mod config;
mod context;
mod errors;
mod events;
mod goal_review;
mod guard;
mod llm;
mod prompt;
mod protocol;
mod ptc;
mod repair;
mod resources;
mod safety;
mod sandbox;
mod session;
mod sse;
mod tools;
mod ui;

#[cfg(test)]
mod regression;
