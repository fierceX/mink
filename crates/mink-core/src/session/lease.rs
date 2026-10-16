//! Advisory session ownership shared by every writable runtime and deletion.
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub struct SessionLease {
    _file: File,
    _parent: Option<std::sync::Arc<SessionLease>>,
}
impl SessionLease {
    pub fn acquire_directory(directory: &Path) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        Self::acquire(directory.join("session.lock"))
    }
    pub(crate) fn with_parent(mut self, parent: Option<std::sync::Arc<Self>>) -> Self {
        self._parent = parent;
        self
    }
    /// The stable lock inode must never be unlinked while a writer owns it.
    pub fn acquire(path: PathBuf) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open session lock {}", path.display()))?;
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|error| anyhow::anyhow!("session is locked by another process: {error}"))?;
        Ok(Self {
            _file: file,
            _parent: None,
        })
    }
}
