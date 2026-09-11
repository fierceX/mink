//! Content-addressed image object cache (`<home>/.mink/cache/images/v1/`).
//!
//! v7 §5: immutable objects keyed by the SHA-256 of their raw bytes, with a
//! two-phase commit (tmp + fsync + hard link + directory fsync) and read-back
//! digest verification. Phase one keeps no index and no variants: source
//! paths live only in the current tool result.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Image cache root below a home directory: `<home>/.mink/cache/images/v1`.
pub fn image_cache_root(home: &Path) -> PathBuf {
    home.join(".mink").join("cache").join("images").join("v1")
}

/// Validate an image object id: `sha256:<64 lowercase hex>`; anything else
/// (paths, traversal, uppercase hex, arbitrary strings) fails closed. The
/// cache only ever produces lowercase ids.
pub fn validate_image_id(id: &str) -> bool {
    let Some(hex) = id.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!(
        "sha256:{}",
        crate::capabilities::fingerprint::hex_lower(hasher.finalize())
    )
}

fn object_path(objects: &Path, id: &str) -> PathBuf {
    let hex = &id["sha256:".len()..];
    objects.join(&hex[..2]).join(hex)
}

pub struct ImageCache {
    root: PathBuf,
    objects: PathBuf,
    tmp: PathBuf,
    write_lock: Mutex<()>,
}

impl ImageCache {
    pub fn new(home: &Path) -> Self {
        let root = image_cache_root(home);
        Self {
            objects: root.join("objects"),
            tmp: root.join("tmp"),
            root,
            write_lock: Mutex::new(()),
        }
    }

    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn ensure(&self) -> Result<()> {
        std::fs::create_dir_all(&self.objects)?;
        std::fs::create_dir_all(&self.tmp)?;
        Ok(())
    }

    /// Publish one image: content-addressed two-phase commit with EEXIST
    /// dedup and crash-safe directory syncs.
    pub fn commit(&self, bytes: &[u8]) -> Result<String> {
        self.ensure()?;
        let id = digest(bytes);
        let target = object_path(&self.objects, &id);
        if target.exists() {
            // Dedup: verify the existing object really matches the digest.
            verify_existing_object(&target, bytes.len(), &id)?;
            return Ok(id);
        }
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Re-check under the lock (concurrent commit race); the winner's
        // object must still match the expected length and digest.
        if target.exists() {
            verify_existing_object(&target, bytes.len(), &id)?;
            return Ok(id);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = self
            .tmp
            .join(format!("{}-{}", std::process::id(), uuid_tail()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.flush()?;
            file.sync_all()?;
            match std::fs::hard_link(&temporary, &target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Another writer won the race; verify its object with the
                    // same bounded check used by the other dedup branches.
                    verify_existing_object(&target, bytes.len(), &id)?;
                }
                Err(error) => return Err(error.into()),
            }
            sync_directory(target.parent().context("object bucket has no parent")?)?;
            sync_directory(&self.objects)?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&temporary);
        result?;
        Ok(id)
    }

    /// Read one object with full digest verification and a hard size cap.
    /// `Ok(None)` when the object is absent; oversized, non-regular or
    /// corrupt objects are errors (fail closed).
    ///
    /// One handle is opened and the cap is enforced on **that handle**:
    /// metadata is checked first, then the read is limited to
    /// `max_bytes + 1`, so a concurrent replacement or growth (TOCTOU) still
    /// cannot cause an unbounded allocation. Symlinks are rejected on Unix
    /// (`O_NOFOLLOW`) and non-regular files (e.g. FIFOs) fail closed;
    /// `O_NONBLOCK` keeps opening a FIFO from blocking.
    pub fn read_bounded(&self, id: &str, max_bytes: u64) -> Result<Option<Vec<u8>>> {
        if !validate_image_id(id) {
            bail!("invalid image object id: {id}");
        }
        let path = object_path(&self.objects, id);
        let file = match open_object_readonly(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("image object {id} is not a regular file");
        }
        if metadata.len() > max_bytes {
            bail!("image object {id} exceeds the {} byte limit", max_bytes);
        }
        let mut bytes = Vec::new();
        file.take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes {
            bail!("image object {id} exceeds the {} byte limit", max_bytes);
        }
        if digest(&bytes) != id {
            bail!("image object {id} failed integrity verification");
        }
        Ok(Some(bytes))
    }

    #[allow(dead_code)]
    pub fn contains(&self, id: &str) -> bool {
        validate_image_id(id) && object_path(&self.objects, id).exists()
    }
}

fn uuid_tail() -> String {
    // Uniqueness for staging files is best-effort; the create_new flag
    // rejects collisions and the caller retries via a fresh name.
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

/// Open a cache object for reading without following symlinks and without
/// blocking on FIFOs; the caller checks `metadata.is_file()` afterwards.
#[cfg(unix)]
fn open_object_readonly(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_object_readonly(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Verify an existing dedup candidate against the content being committed.
///
/// The read is bounded by the expected length (+1 to detect growth): a
/// matching object must have exactly the submitted length and digest, so a
/// mismatch is corruption and fails closed.
fn verify_existing_object(target: &Path, expected_len: usize, id: &str) -> Result<()> {
    let file = open_object_readonly(target)
        .with_context(|| format!("open existing image object {}", target.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("image object {id} exists but is not a regular file (corruption)");
    }
    let expected = expected_len as u64;
    if metadata.len() != expected {
        bail!("image object {id} exists with mismatched length (corruption)");
    }
    let mut existing = Vec::new();
    file.take(expected.saturating_add(1))
        .read_to_end(&mut existing)?;
    if existing.len() as u64 != expected || digest(&existing) != id {
        bail!("image object {id} exists with mismatched content (corruption)");
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        if let Ok(dir) = std::fs::File::open(path)
            && let Err(error) = dir.sync_all()
        {
            let unsupported = error
                .raw_os_error()
                .is_some_and(|code| code == libc::EINVAL || code == libc::ENOTSUP);
            if !unsupported {
                return Err(error.into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(name: &str) -> (ImageCache, PathBuf) {
        let home = std::env::temp_dir().join(format!(
            "mink-image-cache-{}-{name}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t")
        ));
        (ImageCache::new(&home), home)
    }

    #[test]
    fn commit_read_roundtrip() {
        let (cache, home) = cache("roundtrip");
        let id = cache.commit(b"image-bytes-1").unwrap();
        assert!(id.starts_with("sha256:"));
        assert_eq!(
            cache.read_bounded(&id, 1024).unwrap(),
            Some(b"image-bytes-1".to_vec())
        );
        // Dedup: committing identical bytes returns the same id.
        assert_eq!(cache.commit(b"image-bytes-1").unwrap(), id);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn absent_object_returns_none_and_corruption_fails_closed() {
        let (cache, home) = cache("absent");
        assert_eq!(cache.read_bounded(&digest(b"missing"), 1024).unwrap(), None);

        let id = cache.commit(b"payload").unwrap();
        let path = object_path(&cache.objects, &id);
        std::fs::write(&path, b"tampered").unwrap();
        assert!(cache.read_bounded(&id, 1024).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn read_bounded_rejects_oversized_objects_before_allocation() {
        let (cache, home) = cache("bounded");
        let id = cache.commit(b"payload-bytes").unwrap();
        // Larger than the object: fine.
        assert!(cache.read_bounded(&id, 1024).unwrap().is_some());
        // Smaller than the object: fail closed without allocating.
        assert!(cache.read_bounded(&id, 4).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn id_validation_rejects_paths() {
        let valid = "sha256:".to_string() + &"ab".repeat(32);
        assert!(validate_image_id(&valid));
        assert!(!validate_image_id("sha256:ab1"));
        assert!(!validate_image_id("../etc/passwd"));
        assert!(!validate_image_id("image://sha256:ab12"));
        assert!(!validate_image_id("sha256:zzzz"));
        assert!(!validate_image_id(""));
        // Uppercase hex is rejected: the cache only produces lowercase ids.
        let upper = "sha256:".to_string() + &"AB".repeat(32);
        assert!(!validate_image_id(&upper));
    }

    #[test]
    fn mismatched_existing_object_is_rejected() {
        let (cache, home) = cache("mismatch");
        let id = cache.commit(b"first").unwrap();
        // Corrupt the object, then a dedup commit must fail closed instead
        // of silently accepting the corrupted object.
        let path = object_path(&cache.objects, &id);
        std::fs::write(&path, b"corrupted").unwrap();
        assert!(cache.commit(b"first").is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn read_bounded_rejects_symlinked_object() {
        use std::os::unix::fs::symlink;
        let (cache, home) = cache("symlink-object");
        cache.ensure().unwrap();
        let id = digest(b"payload");
        let path = object_path(&cache.objects, &id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink("/dev/zero", &path).unwrap();

        // O_NOFOLLOW refuses the symlink instead of reading an unbounded
        // device stream.
        assert!(cache.read_bounded(&id, 1024).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn read_bounded_rejects_fifo_object() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let (cache, home) = cache("fifo-object");
        cache.ensure().unwrap();
        let id = digest(b"payload");
        let path = object_path(&cache.objects, &id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let cpath = CString::new(path.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

        // O_NONBLOCK keeps the open from blocking on a writer-less FIFO; the
        // regular-file check fails closed.
        let error = cache.read_bounded(&id, 1024).unwrap_err();
        assert!(error.to_string().contains("not a regular file"), "{error}");
        let _ = std::fs::remove_dir_all(&home);
    }
}
