//! Publish-phase fault handling tests: recovery on verified content,
//! latching when recovery is impossible, and fail-closed checks.

use super::*;

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "mink-persistence-{name}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn unsynced_error() -> PublishError {
    PublishError::PublishedButUnsynced(anyhow::anyhow!("injected sync failure"))
}

#[test]
fn not_published_does_not_latch() {
    let dir = temp_dir("not-published");
    let path = dir.join("state.json");
    let fault = PersistenceFault::default();

    let result = publish_state_with(
        &path,
        b"new",
        &fault,
        || {
            Err(PublishError::NotPublished(anyhow::anyhow!(
                "injected write failure"
            )))
        },
        |_| Ok(()),
    );

    assert!(result.is_err());
    assert!(fault.info().is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn published_unsynced_with_matching_snapshot_and_resync_recovers() {
    let dir = temp_dir("recover");
    let path = dir.join("state.json");
    std::fs::write(&path, b"new").unwrap();
    let fault = PersistenceFault::default();

    publish_state_with(&path, b"new", &fault, || Err(unsynced_error()), |_| Ok(()))
        .expect("verified content plus successful re-sync must recover");

    assert!(fault.info().is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn published_unsynced_with_mismatched_snapshot_latches() {
    let dir = temp_dir("mismatch");
    let path = dir.join("state.json");
    std::fs::write(&path, b"old").unwrap();
    let fault = PersistenceFault::default();

    let error = publish_state_with(&path, b"new", &fault, || Err(unsynced_error()), |_| Ok(()))
        .unwrap_err();

    assert!(error.to_string().contains("does not match"), "{error}");
    let info = fault.info().expect("session must be latched");
    assert_eq!(info.path, path);
    let check = fault.check().unwrap_err().to_string();
    assert!(check.contains("restart the session"), "{check}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn published_unsynced_with_failed_resync_latches_even_when_content_matches() {
    let dir = temp_dir("resync-failure");
    let path = dir.join("state.json");
    std::fs::write(&path, b"new").unwrap();
    let fault = PersistenceFault::default();

    let error = publish_state_with(
        &path,
        b"new",
        &fault,
        || Err(unsynced_error()),
        |_| Err(anyhow::anyhow!("injected re-sync failure")),
    )
    .unwrap_err();

    assert!(error.to_string().contains("re-sync failed"), "{error}");
    assert!(
        fault.info().is_some(),
        "a verified snapshot without confirmed durability must stay faulted"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn first_fault_is_kept_as_root_cause() {
    let fault = PersistenceFault::default();
    let first = fault.raise(Path::new("/first"), "first failure");
    let second = fault.raise(Path::new("/second"), "second failure");

    assert!(first.to_string().contains("first failure"), "{first}");
    assert!(
        second.to_string().contains("first failure"),
        "later faults must not replace the root cause: {second}"
    );
    assert_eq!(fault.info().unwrap().path, PathBuf::from("/first"));
}

#[test]
fn publish_entry_refuses_after_latch() {
    let dir = temp_dir("entry-refuses");
    let path = dir.join("state.json");
    std::fs::write(&path, b"old").unwrap();
    let fault = PersistenceFault::default();
    let _ = fault.raise(&path, "injected fault");

    let error = publish_state(&path, b"new", &fault).unwrap_err();
    assert!(error.to_string().contains("persistence fault"), "{error}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"old",
        "a latched session must not publish new state"
    );

    // Explicit recovery paths may still use the low-level helper by design.
    crate::session::atomic_file::atomic_replace(&path, b"recovered").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"recovered");
    let _ = std::fs::remove_dir_all(dir);
}
