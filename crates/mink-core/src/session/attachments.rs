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
        self.commit(bytes, "png")
    }

    fn commit(&self, bytes: &[u8], extension: &str) -> Result<PathBuf> {
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
        let target = self
            .dir
            .join(format!("{}.{}", hex_digest(bytes), extension));
        if std::fs::symlink_metadata(&target).is_ok() {
            verify_existing(&target, bytes)?;
            return Ok(target);
        }
        let staging = self.dir.join(format!(
            ".staging-{}-{}.png",
            std::process::id(),
            staging_tail()
        ));
        let published = write_staging(&staging, bytes).and_then(|()| {
            match std::fs::hard_link(&staging, &target) {
                Ok(()) => Ok(()),
                // Windows rejects renaming onto an existing file; the
                // content-addressed name guarantees identical bytes.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(error) => Err(error.into()),
            }
        });
        let _ = std::fs::remove_file(&staging);
        published?;
        verify_existing(&target, bytes)?;
        Ok(target)
    }
}

/// The file name is the content hash: an object whose bytes differ from the
/// name would silently attach corrupt data, so reuse requires an exact match.
fn verify_existing(path: &Path, bytes: &[u8]) -> Result<()> {
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("attachment must not be a symlink");
    }
    let existing = std::fs::read(path)
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
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("cannot create attachment staging {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
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
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
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
        let path = self.commit(bytes, extension)?;
        representable(&path)?;
        crate::session::atomic_file::sync_parent(&path)?;
        Ok(AttachmentDescriptor {
            id: path.file_name().unwrap().to_str().unwrap().into(),
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
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() > max_bytes.min(16 * 1024 * 1024) {
            bail!("invalid or oversized attachment");
        }
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(&path)?
            .take(max_bytes.min(16 * 1024 * 1024) + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_bytes || hex_digest(&bytes) != hash {
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
}
