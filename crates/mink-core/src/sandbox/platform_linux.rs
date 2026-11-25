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
    let cwd = std::env::current_dir().unwrap_or_default();
    try_nsjail_in_dir(config, exe, args, &cwd, None)
}

/// Build a nsjail command line for a child process in `cwd` with optional inherited FD.
pub fn try_nsjail_in_dir(
    config: &SandboxConfig,
    exe: &Path,
    args: &[String],
    cwd: &Path,
    pass_fd: Option<i32>,
) -> Result<Vec<String>, String> {
    // Check nsjail availability
    Command::new("nsjail")
        .arg("--version")
        .output()
        .map_err(|_| "nsjail binary not found in PATH".to_string())?;

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

    let mut argv = super::nsjail_argv(
        config,
        exe,
        args,
        cwd,
        home.as_deref(),
        mink_home.as_deref(),
        tmpdir.as_deref(),
        has_lib64,
    );
    if let Some(fd) = pass_fd
        && let Some(sep) = argv.iter().position(|a| a == "--")
    {
        argv.splice(sep..sep, ["--pass_fd".into(), fd.to_string()]);
    }
    Ok(argv)
}

/// Build a bubblewrap command line. Returns the full argv (including "bwrap").
pub fn try_bwrap(
    config: &SandboxConfig,
    exe: &Path,
    args: &[String],
) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    try_bwrap_in_dir(config, exe, args, &cwd)
}

/// Build a bubblewrap command line for a child process in `cwd`.
pub fn try_bwrap_in_dir(
    config: &SandboxConfig,
    exe: &Path,
    args: &[String],
    cwd: &Path,
) -> Result<Vec<String>, String> {
    Command::new("bwrap")
        .arg("--version")
        .output()
        .map_err(|_| "bwrap binary not found in PATH".to_string())?;

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
        cwd,
        home.as_deref(),
        mink_home.as_deref(),
    ))
}
