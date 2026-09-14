use super::*;
use crate::tools::runner::ToolExec;

#[test]
fn numbered_hashline_format_uses_bracket_header() {
    assert_eq!(
        format_hashline_read("src/a.rs", "A1B2", 4, "a\nb"),
        "[src/a.rs#A1B2]\n4:a\n5:b"
    );
}

#[test]
fn mismatch_context_marks_anchors_separates_runs_and_truncates_utf8_safely() {
    let mut lines = (1..=100)
        .map(|line| format!("line-{line}"))
        .collect::<Vec<_>>();
    lines[49] = "界".repeat(300);
    let content = lines.join("\n") + "\n";
    let anchors = (10..=100).step_by(10).collect::<BTreeSet<_>>();
    let rendered = format_mismatch_anchor_context(&content, &anchors);

    assert!(rendered.contains("* 10:line-10"));
    assert!(rendered.contains("\n  …\n"));
    assert!(rendered.contains("[line truncated]"));
    assert!(rendered.contains("Anchor context truncated to safety limits"));
    assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());
    let displayed = rendered
        .lines()
        .filter(|line| line.starts_with("* ") || line.starts_with("  ") && line.contains(':'))
        .count();
    assert!(displayed <= MISMATCH_CONTEXT_MAX_LINES);
}

#[test]
fn replace_suffix_recovery_rejects_ambiguity() {
    let root = std::env::temp_dir().join(format!("mink-replace-suffix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::write(root.join("a/x.rs"), "a").unwrap();
    std::fs::write(root.join("b/x.rs"), "b").unwrap();
    assert!(
        resolve_replace_target(&root, "x.rs")
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn memo_end_line_open_ended_reads_cover_eof() {
    // 开放式选择器：end_line=None 表示覆盖 start..EOF，
    // 重复的 `path:N` 请求可以命中 memo。
    assert_eq!(memo_end_line(Some(5), None, 10), None);
    // 有界选择器：记实际末行。
    assert_eq!(memo_end_line(Some(5), Some(3), 3), Some(7));
    assert_eq!(memo_end_line(Some(5), Some(30), 10), Some(14));
}

#[tokio::test]
async fn replace_mode_write_records_rollback_baseline() {
    let shared =
        crate::regression::test_context_for_agent_with_config("replace-write-baseline", |cfg| {
            cfg.edit_mode = crate::config::EditMode::Replace
        })
        .await
        .unwrap();
    let ctx = crate::context::ToolContext::from(shared.as_ref());
    let path = ctx.cwd.join("a.txt");
    let result = WriteTool
        .execute(
            &serde_json::json!({ "path": "a.txt", "content": "new content\n" }),
            &ctx,
        )
        .unwrap();
    assert!(result.content.contains("wrote"));
    let baseline = ctx
        .snapshots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .latest_read_snapshot(&path)
        .expect("Replace-mode Write must update the rollback baseline");
    assert_eq!(baseline.text, "new content\n");
}

#[cfg(unix)]
#[test]
fn atomic_write_reports_target_metadata_error() {
    use std::os::unix::fs::symlink;

    let dir = std::env::temp_dir().join(format!("mink-atomic-meta-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("loop.txt");
    // Self-referential symlink: metadata() fails with ELOOP, which is NOT
    // "target absent" and must not be swallowed.
    symlink("loop.txt", &path).unwrap();

    let error = atomic_write(&path, "content").expect_err("metadata error must be reported");
    assert!(error.to_string().contains("metadata"), "{error}");
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the target must be untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn atomic_write_preserves_existing_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("mink-atomic-perm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("a.rs");
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

    atomic_write(&path, "new").unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "existing permissions must survive the write");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    let _ = std::fs::remove_dir_all(&dir);
}
