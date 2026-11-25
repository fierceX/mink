//! Language-agnostic capability seam for PTC execution runtimes.

use super::PtcBackendKind;
use crate::cancel::{CancellationToken, InterruptSignal};
use crate::config::SandboxConfig;
use crate::tools::metadata::ToolFailureKind;
use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Orthogonal failure taxonomy for PTC program runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PtcFailureKind {
    Exception,
    Timeout,
    Abort,
    WorkerExit,
    Protocol,
    SandboxUnavailable,
}

impl PtcFailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exception => "exception",
            Self::Timeout => "timeout",
            Self::Abort => "abort",
            Self::WorkerExit => "worker_exit",
            Self::Protocol => "protocol",
            Self::SandboxUnavailable => "sandbox_unavailable",
        }
    }
}

/// Structured failure outcome returned on `PtcRunResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtcRunFailure {
    pub kind: PtcFailureKind,
    pub message: String,
}

/// Ordered output entry emitted by a PTC program via `inspect(...)` or stdout/stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtcOutputItem {
    Text(String),
    Image { image_id: String },
}

/// Host-issued descriptor for an image readable inside the execution environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtcImageDescriptor {
    pub image_id: String,
    pub path: String,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

/// Successful host binding resolution delivered back to the guest program.
#[derive(Debug, Clone)]
pub struct PtcBindingSuccess {
    pub value: serde_json::Value,
    pub inspection_text: Option<String>,
    pub image: Option<PtcImageDescriptor>,
}

/// Program-visible failure raised inside the guest as `ToolCallError`.
#[derive(Debug, Clone)]
pub struct PtcBindingError {
    pub tool_name: String,
    pub kind: ToolFailureKind,
    pub message: String,
}

/// Host-side binding dispatcher invoked by a `PtcRuntime` whenever the guest calls `tools.<name>(...)`.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait PtcBindingDispatcher: Send + Sync {
    async fn invoke(
        &self,
        tool_name: &str,
        args: serde_json::Value,
    ) -> std::result::Result<PtcBindingSuccess, PtcBindingError>;
}

/// Unresolved run request passed by `RunCode` (or a host caller) into `PtcRuntime::resolve`.
#[derive(Clone)]
pub struct PtcRunRequest {
    pub code: String,
    pub cwd: PathBuf,
    pub tool_names: Vec<String>,
    pub timeout_secs: Option<u64>,
    pub cancel: CancellationToken,
    pub interrupt: InterruptSignal,
    pub sandbox: Option<SandboxConfig>,
}

/// Fully resolved execution specification returned by `PtcRuntime::resolve`.
#[derive(Clone)]
pub struct PtcRunSpec {
    pub code: String,
    pub cwd: PathBuf,
    pub tool_names: Vec<String>,
    pub timeout: Duration,
    pub cancel: CancellationToken,
    pub interrupt: InterruptSignal,
    pub sandbox: Option<SandboxConfig>,
}

/// Terminal outcome of one PTC run. Program/timeout/abort/protocol errors live in `error`.
#[derive(Debug, Clone)]
pub struct PtcRunResult {
    pub output: Vec<PtcOutputItem>,
    pub output_truncated: bool,
    pub error: Option<PtcRunFailure>,
}

/// Pluggable execution runtime seam for PTC programs.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait PtcRuntime: Send + Sync {
    fn kind(&self) -> PtcBackendKind;
    fn resolve(&self, request: PtcRunRequest) -> Result<PtcRunSpec>;
    async fn run(
        &self,
        spec: PtcRunSpec,
        dispatcher: Arc<dyn PtcBindingDispatcher>,
    ) -> Result<PtcRunResult>;
}
