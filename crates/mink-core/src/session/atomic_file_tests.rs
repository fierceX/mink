use super::*;

#[test]
fn failure_before_replace_preserves_old_body() {
    let root = std::env::temp_dir().join(format!(
        "mink-atomic-fault-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let target = root.join("state.json");
    let temporary = root.join(".state.json.injected");
    std::fs::write(&target, b"old").unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .unwrap();

    let error = write_and_replace_with(
        &mut file,
        &temporary,
        &target,
        b"new",
        None,
        || bail!("injected pre-replace failure"),
        || Ok(()),
    )
    .unwrap_err();

    assert!(error.to_string().contains("injected"));
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    let _ = std::fs::remove_file(temporary);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn failure_after_replace_reports_published_but_unsynced() {
    let root = std::env::temp_dir().join(format!(
        "mink-atomic-post-fault-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let target = root.join("state.json");
    let temporary = root.join(".state.json.injected");
    std::fs::write(&target, b"old").unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .unwrap();

    let error = write_and_replace_with(
        &mut file,
        &temporary,
        &target,
        b"new",
        None,
        || Ok(()),
        || bail!("injected post-replace durability failure"),
    )
    .unwrap_err();

    match error {
        PublishError::PublishedButUnsynced(inner) => {
            assert!(inner.to_string().contains("injected"), "{inner}");
        }
        other => panic!("expected PublishedButUnsynced, got {other:?}"),
    }
    // The rename already happened: the new bytes are visible on disk.
    assert_eq!(std::fs::read(&target).unwrap(), b"new");
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn permissions_are_applied_before_publish() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!(
        "mink-atomic-perms-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let target = root.join("script.sh");
    let temporary = root.join(".script.sh.injected");
    std::fs::write(&target, b"old").unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .unwrap();
    let permissions = std::fs::Permissions::from_mode(0o600);

    write_and_replace_with(
        &mut file,
        &temporary,
        &target,
        b"new",
        Some(&permissions),
        || Ok(()),
        || Ok(()),
    )
    .unwrap();

    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "published inode must keep the requested mode");
    assert_eq!(std::fs::read(&target).unwrap(), b"new");
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn restrictive_temp_creation_never_exposes_content() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!(
        "mink-atomic-restrict-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let temporary = root.join("staging");

    let mut file = create_temp_file(&temporary, true).unwrap();
    let mode_at_creation = std::fs::metadata(&temporary).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode_at_creation, 0o600,
        "staging file must be restrictive at creation, before any content"
    );
    file.write_all(b"private payload").unwrap();
    let mode_with_content = std::fs::metadata(&temporary).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode_with_content, 0o600,
        "staging content must never sit in a wider-permission file"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn injected_permission_failure_is_not_published() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!(
        "mink-atomic-perm-inject-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let target = root.join("script.sh");
    std::fs::write(&target, b"old").unwrap();

    inject_permission_failure_once();
    let error = atomic_replace_status_with_permissions(
        &target,
        b"new",
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap_err();

    match error {
        PublishError::NotPublished(inner) => {
            assert!(
                inner.to_string().contains("injected permission failure"),
                "{inner}"
            );
        }
        other => panic!("expected NotPublished, got {other:?}"),
    }
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert!(
        std::fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }),
        "staging files must be cleaned up when permission application fails"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn rename_failure_before_publish_keeps_old_body() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!(
        "mink-atomic-rename-fail-{}-{}",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let target = root.join("script.sh");
    let temporary = root.join(".script.sh.injected");
    std::fs::write(&target, b"old").unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .unwrap();
    let permissions = std::fs::Permissions::from_mode(0o600);

    // Unlink the staging file in the pre-replace hook: the rename itself
    // fails, so nothing is published and the old body stays.
    let error = write_and_replace_with(
        &mut file,
        &temporary,
        &target,
        b"new",
        Some(&permissions),
        || {
            std::fs::remove_file(&temporary)?;
            Ok(())
        },
        || Ok(()),
    )
    .unwrap_err();

    assert!(
        matches!(error, PublishError::NotPublished(_)),
        "expected NotPublished, got {error:?}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    let _ = std::fs::remove_dir_all(root);
}
