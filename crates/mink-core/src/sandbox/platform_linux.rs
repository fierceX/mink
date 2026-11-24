//! Linux sandbox backends: nsjail (preferred) and bubblewrap (fallback).

use crate::config::SandboxConfig;
use std::path::Path;
use std::process::Command;

/// Build a nsjail command line. Returns the full argv (including "nsjail").
pub fn try_nsjail(
    config: &SandboxConfig,
    exe: &Path,
    args: &[String],
) -> Result<Vec<String>, String> {
    // Check nsjail availability
    Command::new("nsjail")
        .arg("--version")
        .output()
        .map_err(|_| "nsjail binary not found in PATH".to_string())?;

    let cwd = std::env::current_dir().unwrap_or_default();
    let home = std::env::var("HOME").ok();
    let mink_home = std::env::var("MINK_HOME")
        .ok()
        .or_else(|| home.as_ref().map(|h| format!("{h}/.mink")))
        .unwrap_or_else(|| "/tmp/.mink".to_string());
    let mink_home = (!mink_home.is_empty() && mink_home != "/").then(|| {
        let _ = std::fs::create_dir_all(&mink_home);
        mink_home
    });
    let tmpdir = std::env::var("TMPDIR").ok();
    let has_lib64 = Path::new("/lib64").exists();

    Ok(super::nsjail_argv(
        config,
        exe,
        args,
        &cwd,
        home.as_deref(),
        mink_home.as_deref(),
        tmpdir.as_deref(),
        has_lib64,
    ))
}

/// Build a bubblewrap command line. Returns the full argv (including "bwrap").
pub fn try_bwrap(
    config: &SandboxConfig,
    exe: &Path,
    args: &[String],
) -> Result<Vec<String>, String> {
    Command::new("bwrap")
        .arg("--version")
        .output()
        .map_err(|_| "bwrap binary not found in PATH".to_string())?;

    let cwd = std::env::current_dir().unwrap_or_default();
    let home = std::env::var("HOME").ok();
    let mink_home = std::env::var("MINK_HOME")
        .ok()
        .or_else(|| home.as_ref().map(|h| format!("{h}/.mink")))
        .unwrap_or_else(|| "/tmp/.mink".to_string());
    let mink_home = (!mink_home.is_empty() && mink_home != "/").then(|| {
        let _ = std::fs::create_dir_all(&mink_home);
        mink_home
    });
    Ok(super::bwrap_argv(
        config,
        exe,
        args,
        &cwd,
        home.as_deref(),
        mink_home.as_deref(),
    ))
}
