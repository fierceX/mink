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

#[test]
fn write_uses_atomic_staging_and_reports_written_bytes() {
    let dir = std::env::temp_dir().join(format!("mink-write-atomic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nested/a.txt");
    let content = "hello\nworld\n";

    let message = write(path.to_str().unwrap(), content, usize::MAX).unwrap();

    assert_eq!(
        message,
        format!("OK: wrote {} bytes to {}", content.len(), path.display())
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    // No staging leftovers next to the target.
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("mink-write"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "staging files must be cleaned: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[tokio::test]
async fn hashline_partial_commit_bumps_mutation_for_committed_files() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;

    let shared =
        crate::regression::test_context_for_agent_with_config("hashline-partial-mutation", |_| {})
            .await
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
    let tool_ctx = crate::context::ToolContext::from(shared.as_ref());
    let cwd = shared.cwd.clone();

    let readonly = cwd.join("readonly");
    std::fs::create_dir_all(&readonly)?;
    std::fs::write(cwd.join("a.txt"), "old\n")?;
    std::fs::write(readonly.join("b.txt"), "old\n")?;
    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o555))?;

    let (tag_a, tag_b) = {
        let mut store = tool_ctx.snapshots.lock().unwrap();
        (
            store.record(&cwd.join("a.txt"), "old\n", [1]).tag,
            store.record(&readonly.join("b.txt"), "old\n", [1]).tag,
        )
    };
    let before = tool_ctx.memo_mutation.load(Ordering::SeqCst);
    let input =
        format!("[a.txt#{tag_a}]\nPUT 1.=1:\n+new\n[readonly/b.txt#{tag_b}]\nPUT 1.=1:\n+newer");

    let error = super::execute_hashline_edit(&serde_json::json!({"input": input}), &tool_ctx)
        .expect_err("the read-only second file must fail the batch");
    let message = format!("{error:#}");
    assert!(message.contains("committed [a.txt]"), "{message}");
    let after = tool_ctx.memo_mutation.load(Ordering::SeqCst);
    assert!(
        after > before,
        "a committed file must invalidate memos even when a later section fails"
    );
    assert_eq!(std::fs::read_to_string(cwd.join("a.txt"))?, "new\n");
    assert_eq!(std::fs::read_to_string(readonly.join("b.txt"))?, "old\n");

    std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}
