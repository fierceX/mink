use super::{display_relative_path, resolve_tool_path};
use anyhow::Result;
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

// ── Image read protocol (v7 §6/§7) ───────────────────────────────────────
// The runner classifies Read calls before dispatch: image paths never reach
// `ReadTool::execute` (which keeps the text path byte-for-byte unchanged).

/// Classification of one Read call for the two-phase image path.
#[derive(Debug, Clone)]
pub(crate) enum ImageReadKind {
    /// Ordinary text read: existing dispatch path unchanged.
    NotImage,
    /// Local file whose magic prefix matches a supported raster format.
    ImageLocal {
        path: PathBuf,
        display_path: String,
        has_selector: bool,
    },
    /// `image://<sha256>` reference read: re-inject a cached object.
    ImageRef { id: String },
    /// Virtual filesystem candidate: the bytes are NOT read during
    /// classification — `prepare_vfs_candidate` reads one image at a time in
    /// call order, so many VFS images never share memory at once. A `None`
    /// image result falls back to the ordinary text path (with its selector
    /// still honored). `has_selector` is only rejected when the path IS an
    /// image.
    VfsCandidate {
        display_path: String,
        has_selector: bool,
    },
}

/// Phase one, compute-only: decode/verify one image into a prepared object
/// that has not touched storage yet (v7 §7.3 two-phase admission).
#[derive(Debug)]
pub(crate) struct PreparedImage {
    pub bytes: Vec<u8>,
    pub info: crate::tools::image::ImageInfo,
    pub name: String,
    pub display_path: String,
    /// Set for `image://` reference reads whose object already exists.
    pub existing_id: Option<String>,
}

/// Classify one Read call. Capability-disabled sessions never classify as
/// images: `image://` keeps the unknown-scheme fail-closed behavior and local
/// files stay on the text path (v7 §3.4).
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn classify_image_read(
    ctx: &crate::context::ToolContext,
    raw_path: &str,
    selection: &crate::resources::selector::ReadPathSelection,
) -> anyhow::Result<ImageReadKind> {
    if ctx.model_capabilities.image_input.limits().is_none() {
        return Ok(ImageReadKind::NotImage);
    }
    if let Some(id) = raw_path.strip_prefix("image://") {
        if !crate::session::image_cache::validate_image_id(id) {
            anyhow::bail!("Error: invalid image reference: {raw_path}");
        }
        return Ok(ImageReadKind::ImageRef { id: id.to_string() });
    }
    let has_selector = selection.offset.is_some() || selection.limit.is_some() || selection.raw;
    // Registered resources (artifact://, skill://, rule://, session://) and
    // unknown URL-like schemes never become image reads: they keep the
    // resource-router semantics (resolve with selector support, or fail
    // closed on the text path). Checks only what this classifier can see —
    // the async Read path already routed resources before VFS text.
    if ctx.resource_router.can_handle(raw_path) {
        return Ok(ImageReadKind::NotImage);
    }
    if ctx.resource_router.is_url_like(raw_path) {
        return Ok(ImageReadKind::NotImage);
    }
    if let Some(_vfs) = &ctx.read_only_fs {
        // The candidate keeps only its display path: the actual
        // `read_image` call is deferred to `prepare_vfs_candidate`, which
        // runs per image in call order — VFS bytes never accumulate across
        // a batch (bounded memory). A selector is honored by the text path
        // and rejected ONLY when the path turns out to be an image.
        return Ok(ImageReadKind::VfsCandidate {
            display_path: selection.path.clone(),
            has_selector,
        });
    }
    let path = match resolve_tool_path(&ctx.cwd, &selection.path) {
        Ok(path) => path,
        // Path resolution failures keep the existing text-path error surface.
        Err(_) => return Ok(ImageReadKind::NotImage),
    };
    let mut head = [0u8; 16];
    let matched = std::fs::File::open(&path).is_ok_and(|mut file| {
        use std::io::Read as _;
        file.read(&mut head)
            .is_ok_and(|n| crate::tools::image::magic_matches(&head[..n]))
    });
    if !matched {
        return Ok(ImageReadKind::NotImage);
    }
    let display_path = display_relative_path(&ctx.cwd, &path);
    Ok(ImageReadKind::ImageLocal {
        path,
        display_path,
        has_selector,
    })
}

/// Phase one compute: read bounded bytes (local) or reuse provided bytes
/// (VFS / reference), then run the full v7 §4 validation chain.
pub(crate) fn prepare_image_read(
    ctx: &crate::context::ToolContext,
    kind: ImageReadKind,
) -> anyhow::Result<PreparedImage> {
    let limits = image_limits(ctx)
        .ok_or_else(|| anyhow::anyhow!("image reading is not enabled in this session"))?;
    let (bytes, display_path, existing_id) = match kind {
        ImageReadKind::NotImage => anyhow::bail!("not an image read"),
        ImageReadKind::ImageLocal {
            path,
            display_path,
            has_selector,
        } => {
            if has_selector {
                anyhow::bail!(
                    "Error: image files do not support line selectors or raw mode: {display_path}"
                );
            }
            (
                read_bounded_image(&path, limits.max_image_bytes)?,
                display_path,
                None,
            )
        }
        ImageReadKind::ImageRef { id } => {
            let bytes = ctx
                .image_cache
                .read_bounded(&id, limits.max_image_bytes)?
                .ok_or_else(|| anyhow::anyhow!("Error: image://{id} not found in image cache"))?;
            (bytes, String::new(), Some(id))
        }
        ImageReadKind::VfsCandidate { .. } => {
            anyhow::bail!("vfs candidate must go through prepare_vfs_candidate")
        }
    };
    let info = crate::tools::image::probe(&bytes).ok_or_else(|| {
        anyhow::anyhow!("Error: invalid image: header or dimensions could not be parsed")
    })?;
    // Double-layer MIME check: Mink support set (probe) ∩ model allowed_mime.
    if !limits.allowed_mime.contains(&info.format) {
        anyhow::bail!(
            "Error: unsupported image mime {} for the current model",
            info.mime()
        );
    }
    if info.width > limits.max_dimension || info.height > limits.max_dimension {
        anyhow::bail!(
            "Error: image side {}x{} exceeds the {}px per-side limit; downscale the image and retry",
            info.width,
            info.height,
            limits.max_dimension
        );
    }
    let pixels = crate::tools::image::pixel_count(info.width, info.height);
    if pixels > limits.max_pixels {
        anyhow::bail!(
            "Error: image exceeds the {}px decoded-size limit; downscale the image and retry",
            limits.max_pixels
        );
    }
    let name = display_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_string();
    Ok(PreparedImage {
        bytes,
        info,
        name,
        display_path,
        existing_id,
    })
}

/// Prepared image for the VFS candidate path. `NotImage` when the backend
/// reported no image (falls back to the ordinary text read).
#[derive(Debug)]
pub(crate) enum VfsImageOutcome {
    Image(PreparedImage),
    NotImage,
}

/// Phase one compute for a `VfsCandidate`: read one image on demand (call
/// order, one at a time — VFS bytes never accumulate across a batch), then
/// run the same validation chain as local files.
pub(crate) fn prepare_vfs_candidate(
    ctx: &crate::context::ToolContext,
    display_path: String,
    has_selector: bool,
) -> anyhow::Result<VfsImageOutcome> {
    let limits = image_limits(ctx)
        .ok_or_else(|| anyhow::anyhow!("image reading is not enabled in this session"))?;
    let Some(vfs) = &ctx.read_only_fs else {
        anyhow::bail!("vfs candidate without a virtual filesystem");
    };
    let image = vfs
        .read_image(&ctx.vfs_scope, &display_path, limits.max_image_bytes)
        .map_err(|error| anyhow::anyhow!("image read failed for {display_path}: {error:#}"))?
        .map(Ok::<_, anyhow::Error>)
        .transpose()?;
    let Some(image) = image else {
        return Ok(VfsImageOutcome::NotImage);
    };
    // Only reject a selector once the path is CONFIRMED to be an image; a
    // text path with a selector keeps working (the ordinary text read honors
    // offset/limit/raw).
    if has_selector {
        anyhow::bail!(
            "Error: image files do not support line selectors or raw mode: {display_path}"
        );
    }
    // The backend may exceed its declared cap: re-check before any further
    // processing (review fix #6).
    if image.bytes.len() as u64 > limits.max_image_bytes {
        anyhow::bail!(
            "Error: image file exceeds the {} byte limit; downscale the image and retry",
            limits.max_image_bytes
        );
    }
    let info = crate::tools::image::probe(&image.bytes).ok_or_else(|| {
        anyhow::anyhow!("Error: invalid image: header or dimensions could not be parsed")
    })?;
    // The VFS-declared mime must match the decoded bytes (v7 §12).
    if info.mime() != image.mime {
        anyhow::bail!(
            "Error: virtual filesystem declared {} but the bytes are {}",
            image.mime,
            info.mime()
        );
    }
    if !limits.allowed_mime.contains(&info.format) {
        anyhow::bail!(
            "Error: unsupported image mime {} for the current model",
            info.mime()
        );
    }
    if info.width > limits.max_dimension || info.height > limits.max_dimension {
        anyhow::bail!(
            "Error: image side {}x{} exceeds the {}px per-side limit; downscale the image and retry",
            info.width,
            info.height,
            limits.max_dimension
        );
    }
    let pixels = crate::tools::image::pixel_count(info.width, info.height);
    if pixels > limits.max_pixels {
        anyhow::bail!(
            "Error: image exceeds the {}px decoded-size limit; downscale the image and retry",
            limits.max_pixels
        );
    }
    let name = display_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_string();
    Ok(VfsImageOutcome::Image(PreparedImage {
        bytes: image.bytes,
        info,
        name,
        display_path,
        existing_id: None,
    }))
}

pub(crate) fn image_limits(
    ctx: &crate::context::ToolContext,
) -> Option<crate::capabilities::model_capabilities::OpenAiChatImageUrlLimits> {
    ctx.model_capabilities.image_input.limits().cloned()
}

fn read_bounded_image(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity((max_bytes as usize).min(64 * 1024));
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!(
            "Error: image file exceeds the {} byte limit; downscale the image and retry",
            max_bytes
        );
    }
    Ok(bytes)
}
