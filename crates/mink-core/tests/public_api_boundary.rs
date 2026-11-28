use std::fs;
use std::path::{Path, PathBuf};

const ALLOWED_ROOTS: &[&str] = &["config_file", "prelude", "runtime", "sdk_protocol"];

/// Collect every Rust file under `root`.
///
/// The boundary check must not silently shrink when a consumer directory is
/// missing or unreadable: both are test failures, not skips.
fn rust_files(root: &Path, files: &mut Vec<PathBuf>) {
    // Recursion only collects; the non-empty guarantee is checked at the
    // consumer roots, so grouped subdirectories without direct .rs files are
    // valid layouts.
    let entries = fs::read_dir(root)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", root.display()));
    for entry in entries {
        let entry = entry
            .unwrap_or_else(|error| panic!("cannot read an entry of {}: {error}", root.display()));
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

#[test]
fn workspace_consumers_only_use_supported_mink_modules() {
    let core = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = core.parent().and_then(Path::parent).unwrap();
    let roots = [
        workspace.join("crates/mink-cli/src"),
        workspace.join("crates/mink-server/src"),
        workspace.join("crates/mink-rim/src"),
        workspace.join("crates/mink-goal/src"),
        core.join("examples"),
    ];
    let mut files = Vec::new();
    for root in roots {
        assert!(
            root.is_dir(),
            "workspace consumer directory is missing: {}",
            root.display()
        );
        let before = files.len();
        rust_files(&root, &mut files);
        assert!(
            files.len() > before,
            "consumer root has no Rust source files: {}",
            root.display()
        );
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

#[test]
fn mink_core_sources_contain_no_goal_coupling() {
    let core = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = core.join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(!files.is_empty(), "mink-core src directory had no files");

    let forbidden = regex::Regex::new(
        r"GoalStore|GoalController|GoalDriver|GoalEngine|GoalPlugin|GoalReview|GoalDraft|GoalRead|GoalSnapshot|GoalDisplay|goal_updated|goal_finished|goal_adjudication|goal\.json|mink_goal",
    )
    .unwrap();
    let mut violations = Vec::new();
    for path in files {
        let text = fs::read_to_string(&path).unwrap();
        for (line_idx, line) in text.lines().enumerate() {
            if let Some(found) = forbidden.find(line) {
                violations.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    line_idx + 1,
                    found.as_str()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "goal coupling must not remain in mink-core:\n{}",
        violations.join("\n")
    );
}

#[test]
fn mink_core_sources_contain_no_rim_coupling() {
    let core = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = core.join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(!files.is_empty(), "mink-core src directory had no files");

    let forbidden =
        regex::Regex::new(r"mink_rim|RimProfile|RimBundle|SeatRegistry|RuntimeAssembler").unwrap();
    let mut violations = Vec::new();
    for path in files {
        let text = fs::read_to_string(&path).unwrap();
        for (line_idx, line) in text.lines().enumerate() {
            if let Some(found) = forbidden.find(line) {
                violations.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    line_idx + 1,
                    found.as_str()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "declarative rim crate concepts must not leak back into mink-core:\n{}",
        violations.join("\n")
    );
}

#[test]
fn only_orchestrators_import_runtime_ext() {
    let core = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = core.parent().and_then(Path::parent).unwrap();
    let non_orchestrator_roots = [
        workspace.join("crates/mink-cli/src"),
        workspace.join("crates/mink-server/src"),
        core.join("examples"),
    ];
    let mut files = Vec::new();
    for root in non_orchestrator_roots {
        assert!(
            root.is_dir(),
            "workspace consumer directory is missing: {}",
            root.display()
        );
        rust_files(&root, &mut files);
    }

    let direct_ext = regex::Regex::new(r"\bmink::runtime::ext\b").unwrap();
    let braced_ext = regex::Regex::new(r"\buse\s+mink::runtime::\{[^}]*\bext\b").unwrap();
    let ext_call = regex::Regex::new(r"\.ext\(\)").unwrap();
    let mut violations = Vec::new();
    for path in files {
        let text = fs::read_to_string(&path).unwrap();
        if braced_ext.is_match(&text) {
            violations.push(format!(
                "{}: braced use of mink::runtime::ext",
                path.display()
            ));
        }
        for (idx, line) in text.lines().enumerate() {
            if direct_ext.is_match(line) || ext_call.is_match(line) {
                violations.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "ordinary hosts/examples must use mink::runtime, not mink::runtime::ext:\n{}",
        violations.join("\n")
    );
}
