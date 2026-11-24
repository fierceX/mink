// Build-time version provenance: embed the git commit hash (and dirty marker)
// into the binaries so `--version` can identify the exact source revision.
//
// Override with MINK_GIT_HASH (e.g. release pipelines pinning a specific
// commit); empty when git is unavailable so builds still succeed.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    // `.git` lives at the workspace root (or as a `gitdir:` file in a worktree);
    // absolute paths keep rerun tracking reliable regardless of cwd.
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let dot_git = manifest.join("../../.git");
    let git_dir = resolve_git_dir(&dot_git);
    let head_path = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head_path.display());
    if let Ok(head) = std::fs::read_to_string(&head_path)
        && let Some(ref_rel) = head.trim().strip_prefix("ref: ")
    {
        let ref_path = git_dir.join(ref_rel.trim());
        println!("cargo:rerun-if-changed={}", ref_path.display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );
    println!("cargo:rerun-if-env-changed=MINK_GIT_HASH");

    let hash = match std::env::var("MINK_GIT_HASH") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => git_short_hash()
            .map(|hash| {
                if working_tree_dirty() {
                    format!("{hash}-dirty")
                } else {
                    hash
                }
            })
            .unwrap_or_default(),
    };

    println!("cargo:rustc-env=MINK_GIT_HASH={hash}");
}

fn resolve_git_dir(dot_git: &std::path::Path) -> PathBuf {
    if dot_git.is_file()
        && let Ok(content) = std::fs::read_to_string(dot_git)
        && let Some(raw) = content.trim().strip_prefix("gitdir:")
    {
        let path = PathBuf::from(raw.trim());
        return if path.is_absolute() {
            path
        } else {
            dot_git.parent().unwrap_or(dot_git).join(path)
        };
    }
    dot_git.to_path_buf()
}

fn git_short_hash() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!hash.is_empty()).then_some(hash)
}

/// Dirty = any tracked file changed (untracked build artifacts excluded, so
/// `target/`, `cpython-wasi/`, etc. do not mark a clean checkout as dirty).
fn working_tree_dirty() -> bool {
    Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .is_some_and(|output| output.status.success() && !output.stdout.is_empty())
}
