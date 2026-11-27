//! Programmatic Tool Calling (PTC) runtime, protocol, SDK generation, and bridge.

pub mod bridge;
pub mod protocol;
pub mod runtime;
pub mod sdk;
pub mod subprocess_py;
pub mod tool;
#[cfg(feature = "python-sandbox")]
pub mod wasi_py;

use crate::config::SandboxPythonConfig;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub use runtime::{
    PtcBindingDispatcher, PtcBindingError, PtcBindingSuccess, PtcFailureKind, PtcImageDescriptor,
    PtcOutputItem, PtcRunFailure, PtcRunRequest, PtcRunResult, PtcRunSpec, PtcRuntime,
};
pub use subprocess_py::SubprocessPythonPtcRuntime;
#[cfg(feature = "python-sandbox")]
pub use wasi_py::WasiPythonPtcRuntime;

/// Activation mode for Programmatic Tool Calling (`RunCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PtcMode {
    /// Disabled: byte-for-byte native tool surface, prompts, and schemas.
    #[default]
    Off,
    /// Recommended Hybrid mode: native tools remain directly callable alongside `RunCode`
    /// and a deduplicated `<ptc-sdk>` for loops, filtering, and multi-step orchestration.
    #[serde(alias = "hybrid")]
    Tool,
    /// Strict/embedded domain mode: wire tools expose `RunCode` + planning/control tools
    /// (`Plan*`, `Todo*`, `Goal*`, `SubAgent`) while workspace/exec tools live inside `RunCode`.
    Exclusive,
}

impl PtcMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" => Ok(Self::Off),
            "tool" | "additive" | "hybrid" => Ok(Self::Tool),
            "exclusive" | "on" | "true" | "1" => Ok(Self::Exclusive),
            other => {
                bail!("invalid ptc mode {other:?}; expected 'off', 'tool'/'hybrid', or 'exclusive'")
            }
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Tool => "tool",
            Self::Exclusive => "exclusive",
        }
    }
}

/// Built-in PTC execution backend selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PtcBackendKind {
    /// Managed `python3` child process with inherited FD control channel and optional OS sandbox.
    #[default]
    SubprocessPython,
    /// In-process Wasmtime CPython WASI sandbox (`python-sandbox` feature).
    WasiPython,
}

impl PtcBackendKind {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "subprocess_python" | "subprocess-python" | "subprocess" | "python" => {
                Ok(Self::SubprocessPython)
            }
            "wasi_python" | "wasi-python" | "wasi" => Ok(Self::WasiPython),
            other => bail!(
                "invalid ptc backend {other:?}; expected 'subprocess_python' or 'wasi_python'"
            ),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SubprocessPython => "subprocess_python",
            Self::WasiPython => "wasi_python",
        }
    }
}

/// Runtime configuration for PTC (`[ptc]` in `.minkrc` and `AgentOptions::with_ptc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PtcConfig {
    pub mode: PtcMode,
    pub backend: PtcBackendKind,
    pub python_executable: Option<String>,
    pub timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub max_parallel_sub_calls: usize,
    pub max_sub_calls_per_minute: usize,
    pub max_output_bytes: usize,
    pub max_message_bytes: usize,
    pub max_pending_calls: usize,
    pub stop_grace_ms: u64,
}

impl Default for PtcConfig {
    fn default() -> Self {
        Self {
            mode: PtcMode::Off,
            backend: PtcBackendKind::SubprocessPython,
            python_executable: None,
            timeout_secs: 120,
            max_timeout_secs: 3600,
            max_parallel_sub_calls: 8,
            max_sub_calls_per_minute: 512,
            max_output_bytes: 16 * 1024 * 1024,
            max_message_bytes: 16 * 1024 * 1024,
            max_pending_calls: 64,
            stop_grace_ms: 2_000,
        }
    }
}

impl PtcConfig {
    /// Create a `PtcConfig` with defaults and the given activation mode.
    pub fn new(mode: PtcMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Create a Hybrid (`PtcMode::Tool`) configuration with defaults.
    pub fn tool() -> Self {
        Self::new(PtcMode::Tool)
    }

    /// Create an `Exclusive` (`PtcMode::Exclusive`) configuration with defaults.
    pub fn exclusive() -> Self {
        Self::new(PtcMode::Exclusive)
    }

    pub fn with_mode(mut self, mode: PtcMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_backend(mut self, backend: PtcBackendKind) -> Self {
        self.backend = backend;
        self
    }

    pub fn with_python_executable(mut self, executable: impl Into<String>) -> Self {
        self.python_executable = Some(executable.into());
        self
    }

    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    pub fn with_max_timeout_secs(mut self, secs: u64) -> Self {
        self.max_timeout_secs = secs;
        self
    }

    pub fn with_max_parallel_sub_calls(mut self, max: usize) -> Self {
        self.max_parallel_sub_calls = max;
        self
    }

    pub fn with_max_sub_calls_per_minute(mut self, max: usize) -> Self {
        self.max_sub_calls_per_minute = max;
        self
    }

    pub fn with_max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    pub fn with_max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = bytes;
        self
    }

    pub fn with_max_pending_calls(mut self, max: usize) -> Self {
        self.max_pending_calls = max;
        self
    }

    pub fn with_stop_grace_ms(mut self, ms: u64) -> Self {
        self.stop_grace_ms = ms;
        self
    }

    /// Resolve timeout bounds and working directory for a custom [`PtcRuntime::resolve`] implementation.
    pub fn resolve_run_spec(&self, request: PtcRunRequest) -> Result<PtcRunSpec> {
        protocol::resolve_timeout_and_cwd(self, request)
    }

    /// Validate PTC configuration bounds and backend prerequisites before creating a session.
    pub fn validate(&self, sandbox_python: &SandboxPythonConfig) -> Result<()> {
        self.validate_for_runtime(sandbox_python, false)
    }

    /// Validate PTC configuration bounds, skipping built-in WASI prerequisites when a custom
    /// [`PtcRuntime`] is supplied by the host.
    pub fn validate_for_runtime(
        &self,
        sandbox_python: &SandboxPythonConfig,
        has_custom_runtime: bool,
    ) -> Result<()> {
        if self.timeout_secs < 5 {
            bail!(
                "ptc.timeout_secs must be at least 5 seconds; got {}",
                self.timeout_secs
            );
        }
        if self.max_timeout_secs < self.timeout_secs {
            bail!(
                "ptc.max_timeout_secs ({}) must be >= ptc.timeout_secs ({})",
                self.max_timeout_secs,
                self.timeout_secs
            );
        }
        if self.max_parallel_sub_calls == 0 {
            bail!("ptc.max_parallel_sub_calls must be at least 1");
        }
        if self.max_sub_calls_per_minute == 0 {
            bail!("ptc.max_sub_calls_per_minute must be at least 1");
        }
        if self.max_output_bytes < 1024 {
            bail!("ptc.max_output_bytes must be at least 1024");
        }
        if self.max_message_bytes < 4096 {
            bail!("ptc.max_message_bytes must be at least 4096");
        }
        if self.max_pending_calls == 0 {
            bail!("ptc.max_pending_calls must be at least 1");
        }
        if let Some(exe) = &self.python_executable
            && exe.trim().is_empty()
        {
            bail!("ptc.python_executable must not be empty when set");
        }
        if !has_custom_runtime
            && self.mode != PtcMode::Off
            && self.backend == PtcBackendKind::WasiPython
        {
            #[cfg(not(feature = "python-sandbox"))]
            bail!("ptc backend 'wasi_python' requires the 'python-sandbox' feature");
            #[cfg(feature = "python-sandbox")]
            if sandbox_python.wasm_path.trim().is_empty() {
                bail!("ptc backend 'wasi_python' requires sandbox_python.wasm_path");
            }
        }
        let _ = sandbox_python;
        Ok(())
    }

    /// Compute the total sub-call allowance for a resolved run duration.
    pub fn sub_call_limit(&self, timeout: std::time::Duration) -> usize {
        let secs = timeout.as_secs();
        let numerator = (secs as u128).saturating_mul(self.max_sub_calls_per_minute as u128);
        let limit = (numerator / 60).max(1);
        usize::try_from(limit).unwrap_or(usize::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ptc_mode_and_backend_parse_round_trip() {
        assert_eq!(PtcMode::parse("off").unwrap(), PtcMode::Off);
        assert_eq!(PtcMode::parse("TOOL").unwrap(), PtcMode::Tool);
        assert_eq!(PtcMode::parse("hybrid").unwrap(), PtcMode::Tool);
        assert_eq!(PtcMode::parse("exclusive").unwrap(), PtcMode::Exclusive);
        assert!(PtcMode::parse("bogus").is_err());

        assert_eq!(
            PtcBackendKind::parse("subprocess_python").unwrap(),
            PtcBackendKind::SubprocessPython
        );
        assert_eq!(
            PtcBackendKind::parse("wasi-python").unwrap(),
            PtcBackendKind::WasiPython
        );
        assert!(PtcBackendKind::parse("node").is_err());
    }

    #[test]
    fn ptc_config_validate_rejects_invalid_bounds() {
        let sp = SandboxPythonConfig::default();
        assert!(PtcConfig::default().validate(&sp).is_ok());

        let bad_timeout = PtcConfig {
            timeout_secs: 4,
            ..PtcConfig::default()
        };
        assert!(bad_timeout.validate(&sp).is_err());

        let bad_max = PtcConfig {
            timeout_secs: 200,
            max_timeout_secs: 100,
            ..PtcConfig::default()
        };
        assert!(bad_max.validate(&sp).is_err());

        let bad_parallel = PtcConfig {
            max_parallel_sub_calls: 0,
            ..PtcConfig::default()
        };
        assert!(bad_parallel.validate(&sp).is_err());
    }

    #[test]
    fn ptc_mode_deserializes_hybrid_alias_and_fluent_builders_round_trip() {
        let mode: PtcMode = serde_json::from_str("\"hybrid\"").unwrap();
        assert_eq!(mode, PtcMode::Tool);

        let cfg = PtcConfig::exclusive()
            .with_mode(PtcMode::Tool)
            .with_backend(PtcBackendKind::WasiPython)
            .with_python_executable("python3.12")
            .with_timeout_secs(45)
            .with_max_timeout_secs(600)
            .with_max_parallel_sub_calls(4)
            .with_max_sub_calls_per_minute(120)
            .with_max_output_bytes(8_192)
            .with_max_message_bytes(16_384)
            .with_max_pending_calls(12)
            .with_stop_grace_ms(750);
        assert_eq!(cfg.mode, PtcMode::Tool);
        assert_eq!(cfg.backend, PtcBackendKind::WasiPython);
        assert_eq!(cfg.python_executable.as_deref(), Some("python3.12"));
        assert_eq!(cfg.timeout_secs, 45);
        assert_eq!(cfg.max_timeout_secs, 600);
        assert_eq!(cfg.max_parallel_sub_calls, 4);
        assert_eq!(cfg.max_sub_calls_per_minute, 120);
        assert_eq!(cfg.max_output_bytes, 8_192);
        assert_eq!(cfg.max_message_bytes, 16_384);
        assert_eq!(cfg.max_pending_calls, 12);
        assert_eq!(cfg.stop_grace_ms, 750);

        // Built-in WASI validation rejects missing wasm_path (or missing feature), while
        // custom PtcRuntime skips built-in WASI backend prerequisite checks.
        let sp = SandboxPythonConfig {
            wasm_path: String::new(),
            ..SandboxPythonConfig::default()
        };
        assert!(cfg.validate(&sp).is_err());
        assert!(cfg.validate_for_runtime(&sp, true).is_ok());

        let spec = cfg
            .resolve_run_spec(PtcRunRequest {
                code: "inspect(1)".into(),
                cwd: std::path::PathBuf::from("."),
                tool_names: vec!["Read".into()],
                timeout_secs: None,
                cancel: crate::cancel::CancellationToken::new(),
                interrupt: crate::cancel::InterruptSignal::new(),
                sandbox: None,
            })
            .unwrap();
        assert_eq!(spec.timeout, std::time::Duration::from_secs(45));
        assert!(spec.cwd.is_absolute());
    }
}
