//! Session-scoped paste staging (`<session_dir>/attachments/`).
//!
//! These files are **transport copies** for the path-based paste flow: the TUI
//! writes the clipboard image here and the user message carries the absolute
//! path, so the model's `Read` re-captures the bytes through the v7 image
//! pipeline (validation + home content-addressed cache + single consumption).
//! The store is content-addressed: the file name is the SHA-256 of the bytes,
//! so repeated pastes reuse one object and an existing object is never
//! overwritten with different content.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub struct AttachmentStore {
    dir: PathBuf,
}

impl AttachmentStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Publish `bytes` as `<dir>/<sha256>.png` and return the absolute path.
    /// Idempotent: identical bytes reuse the existing object after verifying
    /// its content; a mismatch fails closed instead of silently reusing a
    /// corrupted file.
    pub fn commit_png(&self, bytes: &[u8]) -> Result<PathBuf> {
        let (_, path) = self.commit(bytes, "png")?;
        Ok(path)
    }

    fn commit(&self, bytes: &[u8], extension: &str) -> Result<(String, PathBuf)> {
        std::fs::create_dir_all(&self.dir).with_context(|| {
            format!("cannot create attachment directory {}", self.dir.display())
        })?;
        if std::fs::symlink_metadata(&self.dir)?
            .file_type()
            .is_symlink()
        {
            bail!("attachment directory must not be a symlink");
        }
        restrict_private_dir(&self.dir)?;
        let id = format!("{}.{}", hex_digest(bytes), extension);
        let target = self.dir.join(&id);
        if std::fs::symlink_metadata(&target).is_ok() {
            verify_existing(&target, bytes)?;
            return Ok((id, target));
        }
        let mut staging = None;
        for _ in 0..8 {
            let candidate = self.dir.join(format!(
                ".staging-{}-{}.{extension}",
                std::process::id(),
                staging_tail()
            ));
            match write_staging(&candidate, bytes) {
                Ok(()) => {
                    staging = Some(candidate);
                    break;
                }
                Err(error) if is_already_exists(&error) => continue,
                Err(error) => return Err(error),
            }
        }
        let staging = staging.ok_or_else(|| {
            anyhow::anyhow!(
                "cannot allocate unique attachment staging file in {}",
                self.dir.display()
            )
        })?;
        let published = match std::fs::hard_link(&staging, &target) {
            Ok(()) => Ok(()),
            // Windows rejects renaming onto an existing file; the
            // content-addressed name guarantees identical bytes.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(anyhow::Error::from(error)),
        };
        let _ = std::fs::remove_file(&staging);
        published?;
        verify_existing(&target, bytes)?;
        crate::session::atomic_file::sync_parent(&target)?;
        Ok((id, target))
    }
}

fn is_already_exists(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
    })
}

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

/// The file name is the content hash: an object whose bytes differ from the
/// name would silently attach corrupt data, so reuse requires an exact match.
fn verify_existing(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Read as _;
    let file = open_object_readonly(path)
        .with_context(|| format!("cannot open attachment {}", path.display()))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("attachment {} must be a regular file", path.display());
    }
    let expected_len = bytes.len() as u64;
    if metadata.len() != expected_len {
        bail!(
            "attachment {} does not match its content-addressed name ({} bytes vs {}); refusing to reuse it",
            path.display(),
            metadata.len(),
            bytes.len()
        );
    }
    let mut existing = Vec::with_capacity(bytes.len());
    file.take(expected_len.saturating_add(1))
        .read_to_end(&mut existing)
        .with_context(|| format!("cannot read attachment {}", path.display()))?;
    if existing != bytes {
        bail!(
            "attachment {} does not match its content-addressed name ({} bytes vs {}); refusing to reuse it",
            path.display(),
            existing.len(),
            bytes.len()
        );
    }
    Ok(())
}

fn write_staging(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("cannot create attachment staging {}", path.display()))?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
pub fn restrict_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict attachment directory {}", dir.display()))
}

#[cfg(not(unix))]
pub fn restrict_private_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn staging_tail() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos}-{seq}")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AttachmentDescriptor {
    pub id: String,
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}
impl AttachmentStore {
    pub fn upload(
        &self,
        bytes: &[u8],
        limits: &crate::runtime::OpenAiChatImageUrlLimits,
    ) -> Result<AttachmentDescriptor> {
        let info = validate_image(bytes, limits)?;
        let extension = match info.format {
            crate::tools::image::ImageFormat::Png => "png",
            crate::tools::image::ImageFormat::Jpeg => "jpg",
            crate::tools::image::ImageFormat::Gif => "gif",
            crate::tools::image::ImageFormat::Webp => "webp",
        };
        let (id, path) = self.commit(bytes, extension)?;
        representable(&path)?;
        Ok(AttachmentDescriptor {
            id,
            mime: info.mime().into(),
            width: info.width,
            height: info.height,
            bytes: bytes.len() as u64,
        })
    }
    pub fn read(&self, id: &str, max_bytes: u64) -> Result<(PathBuf, Vec<u8>)> {
        let (hash, extension) = id
            .split_once('.')
            .ok_or_else(|| anyhow::anyhow!("invalid attachment ID"))?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            || !["png", "jpg", "gif", "webp"].contains(&extension)
        {
            bail!("invalid attachment ID");
        }
        if std::fs::symlink_metadata(&self.dir)?
            .file_type()
            .is_symlink()
        {
            bail!("attachment directory must not be a symlink");
        }
        let path = self.dir.join(id);
        let cap = max_bytes.min(16 * 1024 * 1024);
        let file = open_object_readonly(&path)
            .with_context(|| format!("cannot open attachment {}", path.display()))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > cap {
            bail!("invalid or oversized attachment");
        }
        use std::io::Read;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > cap || hex_digest(&bytes) != hash {
            bail!("attachment checksum mismatch or size limit exceeded");
        }
        let path = path.canonicalize()?;
        representable(&path)?;
        Ok((path, bytes))
    }
}
fn representable(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("attachment path is not UTF-8"))?;
    if text.contains('"') || text.chars().any(char::is_control) {
        bail!("attachment path cannot be represented in a message");
    }
    Ok(())
}
fn validate_image(
    bytes: &[u8],
    limits: &crate::runtime::OpenAiChatImageUrlLimits,
) -> Result<crate::tools::image::ImageInfo> {
    if bytes.len() as u64 > limits.max_image_bytes.min(16 * 1024 * 1024) {
        bail!("image too large");
    }
    let info = crate::tools::image::probe(bytes).ok_or_else(|| anyhow::anyhow!("invalid image"))?;
    if !limits.allowed_mime.contains(&info.format)
        || info.width > limits.max_dimension
        || info.height > limits.max_dimension
        || crate::tools::image::pixel_count(info.width, info.height) > limits.max_pixels
    {
        bail!("image format or dimensions exceed session limits");
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    // Bounded decoder checks the entire image; truncated header-only files fail.
    reader.decode().context("damaged image")?;
    Ok(info)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uploaded_bytes_are_validated_deduplicated_and_scoped() {
        let dir = std::env::temp_dir().join(format!(
            "mink-upload-{}-{}",
            std::process::id(),
            staging_tail()
        ));
        let store = AttachmentStore::new(dir.clone());
        let limits = crate::runtime::OpenAiChatImageUrlLimits::default();
        let mut bytes = Vec::new();
        image::RgbaImage::new(2, 2)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let first = store.upload(&bytes, &limits).unwrap();
        assert_eq!(first.id, store.upload(&bytes, &limits).unwrap().id);
        assert_eq!(
            store.read(&first.id, limits.max_image_bytes).unwrap().1,
            bytes
        );
        assert!(
            store
                .read("../../outside.png", limits.max_image_bytes)
                .is_err()
        );
        assert!(store.upload(&bytes[..33], &limits).is_err());
        let mut small = limits.clone();
        small.max_dimension = 1;
        assert!(store.upload(&bytes, &small).is_err());
        std::fs::write(dir.join(&first.id), b"damaged").unwrap();
        assert!(store.upload(&bytes, &limits).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_attachment_is_rejected_without_following() {
        let dir = std::env::temp_dir().join(format!(
            "mink-upload-symlink-{}-{}",
            std::process::id(),
            staging_tail()
        ));
        let store = AttachmentStore::new(dir.clone());
        let limits = crate::runtime::OpenAiChatImageUrlLimits::default();
        let mut bytes = Vec::new();
        image::RgbaImage::new(2, 2)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let id = format!("{}.png", hex_digest(&bytes));
        std::fs::create_dir_all(&dir).unwrap();
        let outside = dir.join("real.png");
        std::fs::write(&outside, &bytes).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(&id)).unwrap();

        assert!(
            store.read(&id, limits.max_image_bytes).is_err(),
            "read() must reject symlink objects via O_NOFOLLOW"
        );
        assert!(
            store.commit_png(&bytes).is_err(),
            "commit_png() must reject symlink dedup candidates"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
