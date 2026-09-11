//! Publish-phase fault handling for session state files.
//!
//! [`crate::session::atomic_file::atomic_replace_status`] can fail *after*
//! the rename: the new content is visible but the directory sync that would
//! confirm durability failed. Treating that as "nothing happened" and
//! retrying from the old in-memory state can diverge disk and memory across
//! restarts.
//!
//! [`publish_state`] is the shared decision point: verify the full intended
//! snapshot on disk, redo the durability step, and either resume normally or
//! latch the session so every later state mutation fails closed until the
//! session is restarted.

use crate::session::atomic_file::{PublishError, atomic_replace_status, sync_parent};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PersistenceFaultInfo {
    pub path: PathBuf,
    pub detail: String,
}

/// Session-level latch for unresolved publish/durability failures.
///
/// Cloning shares the latch; the runtime keeps one handle and state stores
/// (todo/plan) hold clones so a fault raised by any store is visible to the
/// execution layer.
#[derive(Clone, Default)]
pub(crate) struct PersistenceFault {
    inner: Arc<Mutex<Option<PersistenceFaultInfo>>>,
}

impl PersistenceFault {
    pub(crate) fn info(&self) -> Option<PersistenceFaultInfo> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Fail closed if the session is already latched.
    pub(crate) fn check(&self) -> anyhow::Result<()> {
        match self.info() {
            Some(info) => Err(fault_error(&info.path, &info.detail)),
            None => Ok(()),
        }
    }

    /// Latch the session and return the fatal error to propagate.
    pub(crate) fn raise(&self, path: &Path, detail: impl Into<String>) -> anyhow::Error {
        let detail = detail.into();
        let mut slot = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(existing) = slot.as_ref() {
            // Keep the first fault: it is the root cause.
            return fault_error(&existing.path, &existing.detail);
        }
        let info = PersistenceFaultInfo {
            path: path.to_path_buf(),
            detail,
        };
        let error = fault_error(&info.path, &info.detail);
        *slot = Some(info);
        error
    }
}

fn fault_error(path: &Path, detail: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "session state persistence fault at {}: {detail}; restart the session before continuing",
        path.display()
    )
}

/// Publish `content` at `path`, recovering from a published-but-unsynced
/// failure or latching the session when recovery is impossible.
pub(crate) fn publish_state(
    path: &Path,
    content: &[u8],
    fault: &PersistenceFault,
) -> anyhow::Result<()> {
    publish_state_with(
        path,
        content,
        fault,
        || atomic_replace_status(path, content),
        sync_parent,
    )
}

/// Publish `content` while preserving `permissions` on the published inode
/// (set on the temporary file before the rename), with the same
/// published-but-unsynced recovery/latch semantics as [`publish_state`].
pub(crate) fn publish_state_with_permissions(
    path: &Path,
    content: &[u8],
    permissions: std::fs::Permissions,
    fault: &PersistenceFault,
) -> anyhow::Result<()> {
    publish_state_with(
        path,
        content,
        fault,
        || {
            crate::session::atomic_file::atomic_replace_status_with_permissions(
                path,
                content,
                permissions,
            )
        },
        sync_parent,
    )
}

fn publish_state_with(
    path: &Path,
    content: &[u8],
    fault: &PersistenceFault,
    publish: impl FnOnce() -> Result<(), PublishError>,
    sync: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // Authoritative publish entry: a latched session must not write new
    // state. Explicit recovery paths use the low-level `atomic_replace`
    // helpers instead of this entry point.
    fault.check()?;
    match publish() {
        Ok(()) => Ok(()),
        // Nothing was published: the previous state is still authoritative
        // and the caller may report a normal failure.
        Err(PublishError::NotPublished(error)) => Err(error),
        Err(PublishError::PublishedButUnsynced(error)) => {
            // The bytes may be live: compare the full intended snapshot
            // (not just a revision) before deciding what state the session
            // is in.
            match std::fs::read(path) {
                Ok(on_disk) if on_disk == content => {}
                Ok(_) => {
                    return Err(fault.raise(
                        path,
                        format!(
                            "published content does not match the expected snapshot: {error:#}"
                        ),
                    ));
                }
                Err(read_error) => {
                    return Err(fault.raise(
                        path,
                        format!(
                            "cannot verify published content: {read_error}; original failure: {error:#}"
                        ),
                    ));
                }
            }
            // Content verified: redo the durability step. Only a confirmed
            // sync resumes normal execution; otherwise the session stays
            // faulted even though the bytes were correct.
            if let Err(sync_error) = sync(path) {
                return Err(fault.raise(
                    path,
                    format!(
                        "published content verified but re-sync failed: {sync_error:#}; original failure: {error:#}"
                    ),
                ));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "persistence_tests.rs"]
mod tests;
