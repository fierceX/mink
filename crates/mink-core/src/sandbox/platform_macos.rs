//! macOS sandbox backend: sandbox-exec (built-in).
//!
//! File-system write restrictions and optional network isolation work via
//! `sandbox-exec` (deny-all + explicit allow holes).
//! Note: macOS `sandbox-exec` does NOT enforce `read_dirs` at the kernel level
//! because a blanket `(deny file-read* (subpath "/"))` blocks critical system
//! paths required by dyld/TUI initialization that cannot be portably enumerated.
//! Callers requiring strict read-path isolation should run on Linux (`nsjail`/`bwrap`).

use crate::config::SandboxConfig;
use std::path::{Component, Path, PathBuf};

/// Build a sandbox-exec command line. Returns the full argv.
pub fn try_sandbox_exec(
    config: &SandboxConfig,
    exe: &Path,
    args: &[String],
) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    try_sandbox_exec_in_dir(config, exe, args, &cwd, true)
}

/// Build a sandbox-exec command line rooted at an explicit `cwd`.
pub fn try_sandbox_exec_in_dir(
    config: &SandboxConfig,
    exe: &Path,
    args: &[String],
    cwd: &Path,
    allow_session_storage_writes: bool,
) -> Result<Vec<String>, String> {
    let sb_profile = build_sb_profile(config, exe, cwd, allow_session_storage_writes);

    let mut cmd: Vec<String> = vec![
        "sandbox-exec".into(),
        "-p".into(),
        sb_profile,
        exe.display().to_string(),
    ];
    if args.len() > 1 {
        cmd.extend(args[1..].iter().cloned());
    }

    Ok(cmd)
}

/// Build a sandbox-exec profile (Scheme-like DSL).
///
/// Rule evaluation in sandbox-exec:
///   - ``(allow ...)`` rules add capabilities
///   - ``(deny ...)`` rules subtract capabilities
///   - Deny overrides allow regardless of position
///
/// Strategy:
///   1. ``(allow default)`` — let everything start normally (TUI, Mach, IOKit etc.)
///   2. ``(deny file-write* (subpath "/"))`` — deny all writes
///   3. ``(allow file-write* ...)`` — allow writes only to specified dirs
///   4. No blanket ``(deny file-read* ...)`` — reads blocked at app-level
fn build_sb_profile(
    config: &SandboxConfig,
    _exe: &Path,
    cwd: &Path,
    allow_session_storage_writes: bool,
) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mink_home = std::env::var("MINK_HOME").ok();

    build_sb_profile_with_env(
        config,
        cwd,
        &home,
        mink_home.as_deref(),
        allow_session_storage_writes,
    )
}

fn build_sb_profile_with_env(
    config: &SandboxConfig,
    cwd: &Path,
    home: &str,
    mink_home: Option<&str>,
    allow_session_storage_writes: bool,
) -> String {
    let mut lines: Vec<String> = vec!["(version 1)".into()];

    // ═══ Step 1: Allow default — let the process initialize ═══
    lines.push("(allow default)".into());

    // ═══ Step 2: Write restrictions (always enforced when sandboxed) ═══
    // Deny all writes unconditionally so an empty `write_dirs` produces a
    // read-only workspace rather than failing open to `(allow default)`.
    lines.push("(deny file-write* (subpath \"/\"))".into());

    // Punch holes for user-specified write dirs
    for d in &config.write_dirs {
        let resolved = resolve_dir(d, cwd);
        for variant in sandbox_path_variants(Path::new(&resolved)) {
            let escaped = escape_sbpl_string(&variant);
            lines.push(format!("(allow file-write* (subpath \"{escaped}\"))"));
        }
    }

    // Always allow system temp directory (TMPDIR for Edit's unified_diff_color).
    // Emit both the lexical and the resolved path: on macOS TMPDIR is often
    // `/var/folders/...`, a symlink to `/private/var/folders/...`, and
    // sandbox-exec matches the canonical path only.
    for variant in sandbox_path_variants(&std::env::temp_dir()) {
        let escaped = escape_sbpl_string(&variant);
        lines.push(format!("(allow file-write* (subpath \"{escaped}\"))"));
    }
    // Also allow /tmp and /private/tmp (common temp locations)
    lines.push("(allow file-write* (subpath \"/tmp\"))".into());
    lines.push("(allow file-write* (subpath \"/private/tmp\"))".into());

    // Always allow mink session storage when running the mink host process itself.
    // Child subprocesses (e.g., PTC Python workers) must not receive extra write
    // holes into session storage files unless explicitly allowed by `write_dirs`.
    if allow_session_storage_writes {
        for dir in session_storage_write_dirs(home, mink_home, cwd) {
            for variant in sandbox_path_variants(Path::new(&dir)) {
                let escaped = escape_sbpl_string(&variant);
                lines.push(format!("(allow file-write* (subpath \"{escaped}\"))"));
            }
        }
    }

    // ═══ Step 3: Network restrictions ════════════════════════════
    if !config.allow_network {
        lines.push("(deny network*)".into());
    }

    // ═══ Read restrictions are NOT enforced by sandbox-exec ══════
    // A blanket (deny file-read* (subpath "/")) breaks dyld/TUI initialization
    // on macOS because required system paths cannot be portably enumerated.
    // `read_dirs` is therefore not enforced at the OS level on macOS.

    lines.join("\n")
}

fn escape_sbpl_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

fn session_storage_write_dirs(home: &str, mink_home: Option<&str>, cwd: &Path) -> Vec<String> {
    let mut dirs = Vec::new();
    let home = home.trim();
    let mink_home = mink_home.map(str::trim).filter(|value| !value.is_empty());

    match mink_home {
        Some(root) if root != "/" && root != home => {
            dirs.push(resolve_dir(root, cwd));
        }
        _ if !home.is_empty() && home != "/" => {
            dirs.push(
                normalize_sandbox_path(&Path::new(home).join(".mink"))
                    .display()
                    .to_string(),
            );
        }
        _ => {}
    }

    dirs.push(
        normalize_sandbox_path(&cwd.join(".mink"))
            .display()
            .to_string(),
    );
    dirs.sort();
    dirs.dedup();
    dirs
}

fn resolve_dir(dir: &str, cwd: &Path) -> String {
    let p = Path::new(dir);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    normalize_sandbox_path(&abs).display().to_string()
}

/// Lexical and resolved variants of a path for `sandbox-exec` subpath rules.
///
/// Deny/allow rules are evaluated against the kernel-canonical path, while
/// configuration usually carries symlinked forms (`/var/folders/...`,
/// `/tmp/...`, a symlinked HOME). Emitting both forms keeps the rules working
/// regardless of which side of the symlink the caller used.
fn sandbox_path_variants(path: &Path) -> Vec<String> {
    let mut variants = vec![normalize_sandbox_path(path).display().to_string()];
    let canonical = std::fs::canonicalize(path).ok().or_else(|| {
        let parent = path.parent()?;
        let name = path.file_name()?;
        std::fs::canonicalize(parent)
            .ok()
            .map(|resolved| resolved.join(name))
    });
    if let Some(resolved) = canonical {
        let resolved = resolved.display().to_string();
        if !variants.contains(&resolved) {
            variants.push(resolved);
        }
    }
    variants
}

fn normalize_sandbox_path(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect()
}

#[cfg(test)]
#[path = "platform_macos_tests.rs"]
mod tests;
