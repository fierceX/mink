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

/// Activation mode for Programmatic Tool Calling (`RunCode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PtcMode {
    /// Disabled: byte-for-byte native tool surface, prompts, and schemas.
    #[default]
    Off,
    /// Additive: `RunCode` is exposed alongside normal tools plus `<ptc-sdk>`.
    Tool,
    /// Exclusive: wire tools expose `RunCode` (+ direct-only control tools) while
    /// programmatic tools are declared in `<ptc-sdk>` and invoked inside `RunCode`.
    Exclusive,
}

impl PtcMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" => Ok(Self::Off),
            "tool" | "additive" => Ok(Self::Tool),
            "exclusive" | "on" | "true" | "1" => Ok(Self::Exclusive),
            other => bail!("invalid ptc mode {other:?}; expected 'off', 'tool', or 'exclusive'"),
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
            max_timeout_secs: 600,
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
    /// Validate PTC configuration bounds and backend prerequisites before creating a session.
    pub fn validate(&self, sandbox_python: &SandboxPythonConfig) -> Result<()> {
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
        if self.mode != PtcMode::Off && self.backend == PtcBackendKind::WasiPython {
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
}
