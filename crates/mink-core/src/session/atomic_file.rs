use anyhow::{Result, bail};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Failure phase of an atomic replace.
///
/// `NotPublished` keeps the destination untouched (the previous state is
/// still authoritative). `PublishedButUnsynced` means the rename already
/// happened: the new content is visible but the directory sync that would
/// confirm durability failed, so callers must not retry from the old
/// in-memory state as if nothing had been committed.
#[derive(Debug)]
pub(crate) enum PublishError {
    NotPublished(anyhow::Error),
    PublishedButUnsynced(anyhow::Error),
}

impl PublishError {
    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::NotPublished(error) => error,
            Self::PublishedButUnsynced(error) => {
                error.context("content was published but the directory sync failed; do not retry from the previous state")
            }
        }
    }
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPublished(error) => write!(f, "{error}"),
            Self::PublishedButUnsynced(error) => write!(
                f,
                "content was published but the directory sync failed: {error}"
            ),
        }
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotPublished(error) | Self::PublishedButUnsynced(error) => error.source(),
        }
    }
}

pub fn atomic_replace(path: &Path, content: &[u8]) -> Result<()> {
    atomic_replace_status(path, content).map_err(PublishError::into_anyhow)
}

/// Publish `content` at `path`, reporting which phase failed.
///
/// Callers that have to distinguish "nothing happened" from "the bytes are
/// live but durability is unconfirmed" (session state stores) use this
/// instead of [`atomic_replace`]; the public wrapper keeps its signature.
pub(crate) fn atomic_replace_status(path: &Path, content: &[u8]) -> Result<(), PublishError> {
    atomic_replace_impl(path, content, None)
}

/// Publish `content` after setting `permissions` on the temporary file, so
/// the published inode never carries the default creation mode.
pub(crate) fn atomic_replace_status_with_permissions(
    path: &Path,
    content: &[u8],
    permissions: std::fs::Permissions,
) -> Result<(), PublishError> {
    atomic_replace_impl(path, content, Some(permissions))
}

fn atomic_replace_impl(
    path: &Path,
    content: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> Result<(), PublishError> {
    ensure_parent(path).map_err(PublishError::NotPublished)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("state path has no parent: {}", path.display()))
        .map_err(PublishError::NotPublished)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid state path: {}", path.display()))
        .map_err(PublishError::NotPublished)?;
    let mut last_collision = None;
    for _ in 0..16 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{file_name}.tmp-{}-{sequence}",
            std::process::id()
        ));
        match create_temp_file(&temporary, permissions.is_some()) {
            Ok(mut file) => {
                let result = write_and_replace_with(
                    &mut file,
                    &temporary,
                    path,
                    content,
                    permissions.as_ref(),
                    || Ok(()),
                    || Ok(()),
                );
                if result.is_err() {
                    let _ = std::fs::remove_file(&temporary);
                }
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
            }
            Err(error) => return Err(PublishError::NotPublished(error.into())),
        }
    }
    Err(PublishError::NotPublished(
        last_collision
            .map(anyhow::Error::from)
            .unwrap_or_else(|| anyhow::anyhow!("cannot allocate temporary state file")),
    ))
}

/// Create the staging file. When the caller supplies final permissions, the
/// staging file is created restrictively (0600 on Unix) so the private
/// content is never stored under the default umask mode, even briefly.
fn create_temp_file(path: &Path, restrict_permissions: bool) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if restrict_permissions {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = restrict_permissions;
    options.open(path)
}

fn write_and_replace_with(
    file: &mut File,
    temporary: &Path,
    path: &Path,
    content: &[u8],
    permissions: Option<&std::fs::Permissions>,
    before_replace: impl FnOnce() -> Result<()>,
    after_replace: impl FnOnce() -> Result<()>,
) -> Result<(), PublishError> {
    file.write_all(content)
        .map_err(|error| PublishError::NotPublished(error.into()))?;
    file.flush()
        .map_err(|error| PublishError::NotPublished(error.into()))?;
    // Apply the final permissions BEFORE the file sync so the mode is part
    // of the same durability barrier as the content (audit R4).
    if let Some(permissions) = permissions {
        #[cfg(test)]
        if INJECT_PERMISSION_FAILURE.with(|flag| flag.replace(false)) {
            return Err(PublishError::NotPublished(anyhow::anyhow!(
                "injected permission failure"
            )));
        }
        file.set_permissions(permissions.clone())
            .map_err(|error| PublishError::NotPublished(error.into()))?;
    }
    file.sync_all()
        .map_err(|error| PublishError::NotPublished(error.into()))?;
    before_replace().map_err(PublishError::NotPublished)?;
    replace_existing(temporary, path).map_err(PublishError::NotPublished)?;
    // From here on the new bytes are visible: failures are durability
    // failures, never "nothing happened".
    after_replace().map_err(PublishError::PublishedButUnsynced)?;
    sync_parent(path).map_err(PublishError::PublishedButUnsynced)?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test-only injection: make the next permission application fail, so
    /// tests can pin the "no publish on permission failure" contract.
    static INJECT_PERMISSION_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn inject_permission_failure_once() {
    INJECT_PERMISSION_FAILURE.with(|flag| flag.set(true));
}

#[cfg(not(windows))]
fn replace_existing(temporary: &Path, path: &Path) -> Result<()> {
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(windows)]
fn replace_existing(temporary: &Path, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

pub(crate) fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("state path has no parent: {}", path.display()))?;
        if let Err(error) = File::open(parent)?.sync_all() {
            let explicitly_unsupported = error
                .raw_os_error()
                .is_some_and(|code| code == libc::EINVAL || code == libc::ENOTSUP);
            if !explicitly_unsupported {
                return Err(error.into());
            }
        }
    }
    Ok(())
}

fn ensure_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("state path has no parent: {}", path.display()))?;
    if parent.as_os_str().is_empty() {
        bail!("state path has an empty parent: {}", path.display());
    }
    std::fs::create_dir_all(parent)?;
    Ok(())
}

#[cfg(test)]
#[path = "atomic_file_tests.rs"]
mod tests;
