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
fn permission_failure_before_publish_keeps_old_body() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!(
        "mink-atomic-perm-fail-{}-{}",
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

    // Remove the temporary file before the permission step so
    // `set_permissions` fails: the rollback must not be published.
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
