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
    let cwd = std::env::current_dir()
        .map_err(|e| format!("cannot determine current working directory for nsjail: {e}"))?;
    try_nsjail_in_dir(config, exe, args, &cwd, None, true)
}

fn resolve_validated_mink_home(home: Option<&str>) -> Option<String> {
    let val = std::env::var("MINK_HOME")
        .ok()
        .or_else(|| home.map(|h| format!("{h}/.mink")))
        .unwrap_or_else(|| "/tmp/.mink".to_string());
    let trimmed = val.trim().trim_end_matches('/');
    let home_trimmed = home.map(|h| h.trim().trim_end_matches('/'));
    (!trimmed.is_empty() && home_trimmed != Some(trimmed)).then(|| {
        let _ = std::fs::create_dir_all(&val);
        val
    })
}

/// Build a nsjail command line for a child process in `cwd` with optional inherited FD.
pub fn try_nsjail_in_dir(
    config: &SandboxConfig,
    exe: &Path,
    args: &[String],
    cwd: &Path,
    pass_fd: Option<i32>,
    allow_session_storage_writes: bool,
) -> Result<Vec<String>, String> {
    // Check nsjail availability
    Command::new("nsjail")
        .arg("--version")
        .output()
        .map_err(|_| "nsjail binary not found in PATH".to_string())?;

    let env_home = std::env::var("HOME").ok();
    let mount_home = if allow_session_storage_writes {
        env_home.as_deref()
    } else {
        None
    };
    let mink_home = if allow_session_storage_writes {
        resolve_validated_mink_home(env_home.as_deref())
    } else {
        None
    };
    let tmpdir = std::env::var("TMPDIR").ok();
    let has_lib64 = Path::new("/lib64").exists();
    let has_systemd_resolve = Path::new("/run/systemd/resolve").exists();

    let mut argv = super::nsjail_argv(
        config,
        exe,
        args,
        cwd,
        mount_home,
        mink_home.as_deref(),
        tmpdir.as_deref(),
        has_lib64,
        has_systemd_resolve,
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
    let cwd = std::env::current_dir()
        .map_err(|e| format!("cannot determine current working directory for bwrap: {e}"))?;
    try_bwrap_in_dir(config, exe, args, &cwd, true)
}

/// Build a bubblewrap command line for a child process in `cwd`.
pub fn try_bwrap_in_dir(
    config: &SandboxConfig,
    exe: &Path,
    args: &[String],
    cwd: &Path,
    allow_session_storage_writes: bool,
) -> Result<Vec<String>, String> {
    Command::new("bwrap")
        .arg("--version")
        .output()
        .map_err(|_| "bwrap binary not found in PATH".to_string())?;

    let env_home = std::env::var("HOME").ok();
    let mount_home = if allow_session_storage_writes {
        env_home.as_deref()
    } else {
        None
    };
    let mink_home = if allow_session_storage_writes {
        resolve_validated_mink_home(env_home.as_deref())
    } else {
        None
    };
    Ok(super::bwrap_argv(
        config,
        exe,
        args,
        cwd,
        mount_home,
        mink_home.as_deref(),
    ))
}
