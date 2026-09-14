use std::fs;
use std::path::{Path, PathBuf};

const ALLOWED_ROOTS: &[&str] = &["prelude", "runtime", "sdk_protocol"];

/// Collect every Rust file under `root`.
///
/// The boundary check must not silently shrink when a consumer directory is
/// missing or unreadable: both are test failures, not skips.
fn rust_files(root: &Path, files: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(root)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", root.display()));
    let mut count = 0;
    for entry in entries {
        let entry = entry
            .unwrap_or_else(|error| panic!("cannot read an entry of {}: {error}", root.display()));
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
            files.push(path);
            count += 1;
        }
    }
    assert!(
        count > 0,
        "{} must contain at least one Rust source file",
        root.display()
    );
}

#[test]
fn workspace_consumers_only_use_supported_mink_modules() {
    let core = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = core.parent().and_then(Path::parent).unwrap();
    let roots = [
        workspace.join("crates/mink-cli/src"),
        workspace.join("crates/mink-server/src"),
        workspace.join("crates/mink-router/src"),
        workspace.join("crates/mink-prefab/src"),
        core.join("examples"),
    ];
    let mut files = Vec::new();
    for root in roots {
        assert!(
            root.is_dir(),
            "workspace consumer directory is missing: {}",
            root.display()
        );
        rust_files(&root, &mut files);
    }

    let direct = regex::Regex::new(r"\bmink::([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    let braced = regex::Regex::new(r"\buse\s+mink::\{([^}]*)\}").unwrap();
    let mut violations = Vec::new();
    for path in files {
        let text = fs::read_to_string(&path).unwrap();
        for captures in direct.captures_iter(&text) {
            let root = &captures[1];
            if !ALLOWED_ROOTS.contains(&root) {
                violations.push(format!("{}: mink::{root}", path.display()));
            }
        }
        for captures in braced.captures_iter(&text) {
            for item in captures[1].split(',') {
                let root = item.split_whitespace().next().unwrap_or_default();
                if !root.is_empty() && !ALLOWED_ROOTS.contains(&root) {
                    violations.push(format!("{}: use mink::{{{root}}}", path.display()));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "unsupported mink module imports:\n{}",
        violations.join("\n")
    );
}
