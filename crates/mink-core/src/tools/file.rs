use crate::resources::selector::{select_text_lines, split_read_path_selection};
use crate::tools::surface::FilesystemBackend;
use anyhow::{Result, anyhow, bail, ensure};
use similar::{ChangeTag, TextDiff};
#[cfg(test)]
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

/// Threshold for switching to streaming read (bytes).
const STREAM_READ_THRESHOLD: u64 = 1_048_576; // 1MB

const FULL_READ_PREVIEW_LINES: usize = 200;

fn file_not_found_suggestion(path: &Path) -> String {
    let pattern = path
        .parent()
        .map(|parent| format!("{}/*", parent.display()))
        .unwrap_or_else(|| "*".to_string());
    format!(
        "Error: file not found or unreadable: {}. Use Glob(pattern='{pattern}') to discover existing files.",
        path.display()
    )
}

/// Read up to `max_lines` lines from the file head. Returns the lines and,
/// when the whole file was read, the exact line count (None otherwise).
fn read_head_lines(path: &Path, max_lines: usize) -> Result<(Vec<String>, Option<usize>)> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)
        .map_err(|error| anyhow!("Error: cannot read {}: {error}", path.display()))?;
    let mut lines = Vec::with_capacity(max_lines + 1);
    for line in std::io::BufReader::new(file).lines() {
        let line =
            line.map_err(|error| anyhow!("Error: cannot read {}: {error}", path.display()))?;
        lines.push(line);
        if lines.len() > max_lines {
            lines.pop();
            return Ok((lines, None));
        }
    }
    let total = lines.len();
    Ok((lines, Some(total)))
}

/// Read the last `max_lines` lines of a file (for the tail portion of an
/// oversized-read preview).
fn read_tail_lines(path: &Path, max_lines: usize) -> Result<Vec<String>> {
    use std::io::{Seek as _, SeekFrom};
    let meta = std::fs::metadata(path)?;
    let window = 8192usize.min(meta.len() as usize);
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::End(-(window as i64)))?;
    let mut bytes = Vec::new();
    std::io::BufReader::new(file).read_to_end(&mut bytes)?;
    // 窗口起点可能落在多字节 UTF-8 字符中间：丢弃残缺的首字符再解码，
    // 否则含 CJK 的超限文件整读失败。
    if window < meta.len() as usize {
        let mut start = 0;
        while start < bytes.len() && (bytes[start] >> 6) == 0b10 {
            start += 1;
        }
        bytes.drain(..start);
    }
    let text = String::from_utf8(bytes)?;
    let mut lines = text.lines().rev().take(max_lines).collect::<Vec<_>>();
    lines.reverse();
    Ok(lines.into_iter().map(str::to_string).collect())
}

/// Format the tail block of a preview: absolute line numbers when the exact
/// total is known, `…` prefixes otherwise.
fn format_preview_tail(tail: &[String], total: Option<usize>) -> String {
    if tail.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n...\n");
    let base = total.map(|total| total.saturating_sub(tail.len()));
    for (index, line) in tail.iter().enumerate() {
        match base {
            Some(base) => out.push_str(&format!("{}:{line}\n", base + index + 1)),
            None => out.push_str(&format!("…:{line}\n")),
        }
    }
    out
}
/// When a full read would exceed the byte limit, return a preview response
/// (size, line count, head lines, tail lines, selector example); Ok(None)
/// means the file fits within the limit and should be read normally.
fn full_read_preview(path: &Path, max_bytes: usize) -> Result<Option<String>> {
    let meta = std::fs::metadata(path).map_err(|_| anyhow!(file_not_found_suggestion(path)))?;
    if meta.len() as u128 <= max_bytes as u128 {
        return Ok(None);
    }
    let (head, exact_total) = read_head_lines(path, FULL_READ_PREVIEW_LINES)?;
    let total_desc = match exact_total {
        Some(total) => format!("{total} lines"),
        None => format!("more than {FULL_READ_PREVIEW_LINES} lines"),
    };
    let mut out = format!(
        "(file too large: {} bytes / {total_desc}; showing first {FULL_READ_PREVIEW_LINES} lines)\n",
        meta.len()
    );
    out.push_str(&format_numbered_read(1, &head.join("\n")));
    let tail = read_tail_lines(path, 5)?;
    out.push_str(&format_preview_tail(&tail, exact_total));
    out.push_str(&format!(
        "[Full file exceeds the read limit ({max_bytes} bytes). Use 'path:start-end' to read a line range, e.g. '{}:1-{FULL_READ_PREVIEW_LINES}'.]",
        path.display()
    ));
    Ok(Some(out))
}

pub fn read(path: &str, offset: Option<usize>, limit: Option<usize>) -> Result<String> {
    if path.is_empty() {
        bail!("Error: no path provided");
    }

    // Fast path: small file or full read — use read_to_string directly.
    let meta =
        std::fs::metadata(path).map_err(|_| anyhow!(file_not_found_suggestion(Path::new(path))))?;

    if meta.len() < STREAM_READ_THRESHOLD || (offset.is_none() && limit.is_none()) {
        // Small file: existing fast path
        let data = std::fs::read_to_string(path)
            .map_err(|_| anyhow!("Error: file not found or unreadable: {path}"))?;
        if offset.is_none() && limit.is_none() {
            return Ok(data);
        }
        // Range on a small file: same logic as before
        let mut lines: Vec<&str> = data.split('\n').collect();
        if !lines.is_empty() && lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        let range = selected_line_range(lines.len(), offset, limit, path)?;
        return Ok(lines[range].join("\n"));
    }

    // Large file + range: stream — scan line boundaries, then read exact byte range.
    let start_line = offset.unwrap_or(1).max(1);
    let count = limit.filter(|count| *count > 0);

    let mut reader = BufReader::new(std::fs::File::open(path)?);
    // line_offsets[i] = byte offset where line (i+1) starts in the file.
    let mut line_offsets: Vec<u64> = Vec::with_capacity(4096);

    let target_line_count = count.and_then(|count| start_line.checked_sub(1)?.checked_add(count));
    let mut buf = Vec::new();

    loop {
        let pos_before = reader.stream_position()?;
        buf.clear();
        let bytes_read = reader.read_until(b'\n', &mut buf)?;
        if bytes_read == 0 {
            break; // EOF
        }
        line_offsets.push(pos_before);
        if let Some(target) = target_line_count
            && line_offsets.len() > target
        {
            break; // we have the end offset for the requested range
        }
    }

    let total_lines = line_offsets.len();
    let range = selected_line_range(total_lines, Some(start_line), count, path)?;
    if range.is_empty() {
        return Ok(String::new());
    }
    let start_byte = line_offsets[range.start];

    // End byte: either the start of the next line after the range, or EOF.
    let end_byte = if range.end < line_offsets.len() {
        line_offsets[range.end]
    } else {
        // Scan to end of file to find the exact byte position
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf)?;
        reader.stream_position()?
    };

    if start_byte >= end_byte {
        return Ok(String::new());
    }

    // Seek and read the exact byte range.
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start_byte))?;
    let mut content = vec![0u8; (end_byte - start_byte) as usize];
    file.read_exact(&mut content)?;

    let mut selected = String::from_utf8(content)?;
    if selected.ends_with('\n') {
        selected.pop();
        if selected.ends_with('\r') {
            selected.pop();
        }
    }
    Ok(selected)
}

fn selected_line_range(
    total_lines: usize,
    offset: Option<usize>,
    limit: Option<usize>,
    path: &str,
) -> Result<Range<usize>> {
    let start_line = offset.unwrap_or(1).max(1);
    if start_line > total_lines {
        if total_lines == 0 && start_line == 1 {
            return Ok(0..0);
        }
        bail!(
            "Error: offset {} exceeds total lines {} in {}",
            start_line,
            total_lines,
            path
        );
    }
    let start = start_line - 1;
    let end = match limit {
        Some(count) if count > 0 => start.saturating_add(count).min(total_lines),
        _ => total_lines,
    };
    Ok(start..end)
}

pub fn write(path: &str, content: &str, max_bytes: usize) -> Result<String> {
    if path.is_empty() {
        bail!("Error: no path provided");
    }
    if content.len() > max_bytes {
        bail!(
            "Error: content too large for write_file ({} bytes > {} bytes)",
            content.len(),
            max_bytes
        );
    }
    if let Some(dir) = Path::new(path).parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Same atomic semantics as Edit: stage + fsync + rename, so a failed
    // write can never leave a truncated file behind.
    let existing = existing_target(Path::new(path))?;
    let (mut file, temporary) = stage_temp_file(
        Path::new(path),
        "mink-write",
        matches!(existing, ExistingTarget::Present(_)),
    )?;
    let result = (|| -> Result<()> {
        use std::io::Write as _;
        if let ExistingTarget::Present(permissions) = &existing {
            file.set_permissions(permissions.clone())?;
        }
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
        result?;
    }
    // Report from the written buffer, not a follow-up metadata call: a
    // successful write must never be reported as a failure.
    Ok(format!("OK: wrote {} bytes to {path}", content.len()))
}

/// Generate a unified diff using the `similar` crate (pure Rust, no subprocess).
pub(crate) fn inline_diff(path: &str, old: &str, new: &str) -> Result<(String, usize, usize)> {
    if old == new {
        return Ok((String::new(), 0, 0));
    }
    let diff = TextDiff::from_lines(old, new);
    let mut output = String::new();
    let mut added = 0usize;
    let mut removed = 0usize;

    let label = path.trim_start_matches('/');
    output.push_str(&format!("--- a/{label}\n"));
    output.push_str(&format!("+++ b/{label}\n"));

    for group in diff.grouped_ops(3).iter() {
        if group.is_empty() {
            continue;
        }
        if let Some(first) = group.first() {
            let old_range: Range<usize> = first.old_range();
            let mut total_old_len = 0usize;
            let mut total_new_len = 0usize;
            for op in group {
                total_old_len += op.old_range().len();
                total_new_len += op.new_range().len();
            }
            output.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                old_range.start + 1,
                total_old_len.max(1),
                first.new_range().start + 1,
                total_new_len.max(1)
            ));
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let (sign, ansi, reset) = match change.tag() {
                    ChangeTag::Equal => (" ", "", ""),
                    ChangeTag::Insert => {
                        added += 1;
                        ("+", "\x1b[32m", "\x1b[0m")
                    }
                    ChangeTag::Delete => {
                        removed += 1;
                        ("-", "\x1b[31m", "\x1b[0m")
                    }
                };
                let val = change.value();
                if let Some(trimmed) = val.strip_suffix('\n') {
                    output.push_str(&format!("{ansi}{sign}{reset}{ansi}{trimmed}{reset}\n"));
                } else {
                    output.push_str(&format!("{ansi}{sign}{reset}{ansi}{val}{reset}\n"));
                }
            }
        }
    }

    Ok((output, added, removed))
}

pub struct ReadTool;
pub struct WriteTool;
pub struct EditTool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadTargetClass {
    Filesystem {
        backend: FilesystemBackend,
        raw: bool,
    },
    RegisteredResource,
}

pub(crate) fn classify_read_selection(
    selection: &crate::resources::selector::ReadPathSelection,
    router: &crate::resources::ResourceRouter,
    filesystem_backend: FilesystemBackend,
) -> anyhow::Result<ReadTargetClass> {
    if router.can_handle(&selection.path) {
        return Ok(ReadTargetClass::RegisteredResource);
    }
    if router.is_url_like(&selection.path) {
        let scheme = selection
            .path
            .split_once("://")
            .map_or("", |(scheme, _)| scheme);
        anyhow::bail!("unknown resource scheme: {scheme}");
    }
    Ok(ReadTargetClass::Filesystem {
        backend: filesystem_backend,
        raw: selection.raw,
    })
}

pub(crate) fn classify_read_target(
    input: &serde_json::Value,
    router: &crate::resources::ResourceRouter,
    filesystem_backend: FilesystemBackend,
) -> anyhow::Result<ReadTargetClass> {
    let path = input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("tool input is missing a string path"))?;
    let selection = split_read_path_selection(path)?;
    classify_read_selection(&selection, router, filesystem_backend)
}

fn resolve_tool_path(cwd: &Path, raw: &str) -> Result<PathBuf> {
    if raw.is_empty() {
        bail!("Error: no path provided");
    }
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    Ok(normalize_lexically(&joined))
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            Component::RootDir | Component::Prefix(_) => out.push(component.as_os_str()),
        }
    }
    out
}

impl super::runner::ToolExec for ReadTool {
    fn metadata(&self) -> super::metadata::ToolMetadata {
        super::metadata::ToolMetadata::new(
            "Read",
            super::metadata::ApprovalTier::Read,
            super::metadata::ToolResultKind::FileRead,
        )
    }

    fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &crate::context::ToolContext,
    ) -> anyhow::Result<super::runner::ToolOutcome> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            path: String,
            // 兼容字段：外部 agent 框架常用这些读参名（limit/offset/range/
            // selector 等）。它们只被接受并忽略——行范围必须写在 `path`
            // 选择器里；真正未知的字段仍然 fail closed。
            #[serde(default)]
            limit: Option<serde_json::Value>,
            #[serde(default)]
            offset: Option<serde_json::Value>,
            #[serde(default)]
            range: Option<serde_json::Value>,
            #[serde(default)]
            path_range: Option<serde_json::Value>,
            #[serde(default)]
            path_selector: Option<serde_json::Value>,
            #[serde(default)]
            selector: Option<serde_json::Value>,
        }
        let args: Args = crate::tools::args::decode_args(input)?;
        // 兼容字段不携带行为：解析成功即丢弃，显式引用避免 lint 隐藏该事实。
        let _ = (
            &args.limit,
            &args.offset,
            &args.range,
            &args.path_range,
            &args.path_selector,
            &args.selector,
        );
        let selection = split_read_path_selection(&args.path)?;
        let filesystem_backend = if ctx.read_only_fs.is_some() {
            FilesystemBackend::ReadOnlyVfs
        } else {
            FilesystemBackend::Local
        };
        let target_class =
            classify_read_selection(&selection, &ctx.resource_router, filesystem_backend)?;
        if target_class == ReadTargetClass::RegisteredResource {
            let resource = ctx.resource_router.resolve(&selection, ctx)?;
            let text = select_text_lines(
                &resource.content,
                selection.offset,
                selection.limit,
                &selection.path,
            )?;
            return Ok(super::runner::ToolOutcome::text(text));
        }
        if let Some(vfs) = &ctx.read_only_fs {
            let virtual_path = crate::tools::vfs::normalize_virtual_file_path(&selection.path)?;
            let result = vfs.read(
                &ctx.vfs_scope,
                &crate::tools::vfs::VfsReadRequest {
                    path: virtual_path,
                    offset: selection.offset,
                    limit: selection.limit,
                    max_full_read_bytes: ctx.tool_config.tool_result_max_bytes,
                },
            )?;
            if selection.offset.is_none()
                && selection.limit.is_none()
                && result.total_bytes > ctx.tool_config.tool_result_max_bytes
            {
                let shown = result.content.lines().take(FULL_READ_PREVIEW_LINES).count();
                let total_desc = if result.total_lines > 0 {
                    format!("{} lines", result.total_lines)
                } else {
                    format!("more than {} lines", FULL_READ_PREVIEW_LINES)
                };
                let mut preview = format!(
                    "(file too large: {} bytes / {total_desc}; showing first {shown} lines)\n",
                    result.total_bytes
                );
                for (idx, line) in result
                    .content
                    .lines()
                    .take(FULL_READ_PREVIEW_LINES)
                    .enumerate()
                {
                    preview.push_str(&format!("{}:{}\n", idx + 1, line));
                }
                // Tail portion from the backend content (only when the content
                // actually exceeds the head window, so small results do not
                // duplicate their own head).
                let all_lines = result.content.lines().collect::<Vec<_>>();
                if all_lines.len() > FULL_READ_PREVIEW_LINES {
                    let tail = all_lines
                        .into_iter()
                        .rev()
                        .take(5)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>();
                    let tail_owned = tail.into_iter().map(str::to_string).collect::<Vec<_>>();
                    preview.push_str(&format_preview_tail(&tail_owned, None));
                }
                preview.push_str(&format!(
                    "[Full file exceeds the read limit ({} bytes). Use 'path:start-end' to read a line range, e.g. '{}:1-{}'.]",
                    ctx.tool_config.tool_result_max_bytes,
                    selection.path,
                    FULL_READ_PREVIEW_LINES
                ));
                return Ok(super::runner::ToolOutcome::text(preview));
            }
            if selection.raw {
                return Ok(super::runner::ToolOutcome::text(result.content));
            }
            return Ok(super::runner::ToolOutcome::text(format_read_only_virtual(
                &selection.path,
                selection.offset.unwrap_or(1),
                &result.content,
            )));
        }
        let path = resolve_tool_path(&ctx.cwd, &selection.path)?;
        if selection.offset.is_none()
            && selection.limit.is_none()
            && let Some(preview) = full_read_preview(&path, ctx.tool_config.tool_result_max_bytes)?
        {
            return Ok(super::runner::ToolOutcome::text(preview));
        }
        if let Some(cached) = ctx.memo_hit(
            &path,
            selection.raw,
            selection.offset,
            selection
                .offset
                .zip(selection.limit)
                .map(|(start, count)| start.saturating_add(count.saturating_sub(1))),
        ) {
            return Ok(super::runner::ToolOutcome::text(cached));
        }
        let start_line = selection.offset.unwrap_or(1);
        let editable_limit = 4 * 1024 * 1024usize;
        let full_text = if ctx.tool_config.edit_mode == crate::config::EditMode::Hashline
            && std::fs::metadata(&path)?.len() as u128
                <= editable_limit.min(ctx.tool_config.file_write_max_bytes) as u128
        {
            Some(
                std::fs::read_to_string(&path)
                    .map_err(|error| anyhow!("Error: cannot read {}: {error}", path.display()))?,
            )
        } else {
            None
        };
        let content = full_text.as_ref().map_or_else(
            || {
                read(
                    &path.display().to_string(),
                    selection.offset,
                    selection.limit,
                )
            },
            |text| select_text_lines(text, selection.offset, selection.limit, &selection.path),
        )?;
        let visible_count = crate::tools::snapshot::split_content_lines(&content).len();
        if selection.raw {
            // Size-guard raw output in every mode (Replace or beyond the
            // editable limit has no full_text snapshot path): a raw read whose
            // output would be truncated must not seed the memo.
            ensure!(
                content.len() <= ctx.tool_config.tool_result_max_bytes,
                "Error: selected Read output is too large ({} bytes > {} bytes); request a narrower line range",
                content.len(),
                ctx.tool_config.tool_result_max_bytes
            );
            // raw 读取不记录可编辑 snapshot（文档协议：raw 不生成锚点），
            // 也不满足 Hashline 的 seen-lines 守卫。
            // Offer the memo candidate; the runner records it only when the
            // final composed output (raw content plus its summary line) fits
            // the budget, so a later hit can never ask the model to "reuse"
            // truncated content. Raw reads stay under the raw flag and never
            // satisfy non-raw requests.
            let mut outcome = super::runner::ToolOutcome::text(content);
            outcome.memo_candidate = Some(super::runner::MemoCandidate {
                path: path.clone(),
                raw: true,
                start_line: selection.offset,
                end_line: memo_end_line(selection.offset, selection.limit, visible_count),
            });
            return Ok(outcome);
        }
        let rendered = match full_text {
            Some(full_text) => {
                let tag = crate::tools::snapshot::compute_file_tag(&full_text);
                let rendered = format_hashline_read(&selection.path, &tag, start_line, &content);
                ensure!(
                    rendered.len() <= ctx.tool_config.tool_result_max_bytes,
                    "Error: selected Hashline Read output is too large ({} bytes > {} bytes); request a narrower line range",
                    rendered.len(),
                    ctx.tool_config.tool_result_max_bytes
                );
                let snapshot = ctx
                    .snapshots
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .record(&path, &full_text, start_line..start_line + visible_count);
                debug_assert_eq!(snapshot.tag, tag);
                rendered
            }
            None => {
                // Replace 模式（或超限文件）无 hashline 快照语义，但仍记录回滚基线。
                if let Ok(meta) = std::fs::metadata(&path)
                    && meta.len() as u128
                        <= editable_limit.min(ctx.tool_config.file_write_max_bytes) as u128
                    && let Ok(full) = std::fs::read_to_string(&path)
                {
                    ctx.snapshots
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .record(&path, &full, start_line..start_line + visible_count);
                }
                format_numbered_read(start_line, &content)
            }
        };
        // Offer the memo candidate; the runner records it only when the final
        // composed output (rendered content plus its summary line) fits the
        // budget, so a later hit can never ask the model to "reuse" content
        // that was truncated or spilled at the runner layer.
        let mut outcome = super::runner::ToolOutcome::text(rendered);
        outcome.memo_candidate = Some(super::runner::MemoCandidate {
            path: path.clone(),
            raw: false,
            start_line: selection.offset,
            end_line: memo_end_line(selection.offset, selection.limit, visible_count),
        });
        Ok(outcome)
    }
}

/// memo 端行语义：开放式选择器（读到 EOF）记 `None`，与 `covers()` 的
/// `(Some(N), None)` 查询对齐，使重复的 `path:N` 开放式读取可以命中；
/// 有界选择器记实际末行。
fn memo_end_line(
    offset: Option<usize>,
    limit: Option<usize>,
    visible_count: usize,
) -> Option<usize> {
    limit?;
    offset.map(|start| start + visible_count.saturating_sub(1))
}

impl super::runner::ToolExec for WriteTool {
    fn metadata(&self) -> super::metadata::ToolMetadata {
        super::metadata::ToolMetadata::new(
            "Write",
            super::metadata::ApprovalTier::Write,
            super::metadata::ToolResultKind::FileWrite,
        )
        .mutating()
    }

    fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &crate::context::ToolContext,
    ) -> anyhow::Result<super::runner::ToolOutcome> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            path: String,
            content: String,
        }
        let args: Args = crate::tools::args::decode_args(input)?;
        let path = resolve_tool_path(&ctx.cwd, &args.path)?;
        let result = write(
            &path.display().to_string(),
            &args.content,
            ctx.tool_config.file_write_max_bytes,
        )?;
        if args.content.len() <= (4 * 1024 * 1024usize).min(ctx.tool_config.file_write_max_bytes) {
            let line_count = crate::tools::snapshot::split_content_lines(&args.content).len();
            let snapshot = ctx
                .snapshots
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(&path, &args.content, 1..=line_count);
            if ctx.tool_config.edit_mode == crate::config::EditMode::Hashline {
                return Ok(super::runner::ToolOutcome::text(format!(
                    "{result}\n[{}#{}]",
                    args.path, snapshot.tag
                )));
            }
        }
        Ok(super::runner::ToolOutcome::text(result))
    }
}

impl super::runner::ToolExec for EditTool {
    fn metadata(&self) -> super::metadata::ToolMetadata {
        super::metadata::ToolMetadata::new(
            "Edit",
            super::metadata::ApprovalTier::Write,
            super::metadata::ToolResultKind::Edit,
        )
        .mutating()
    }

    fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &crate::context::ToolContext,
    ) -> anyhow::Result<super::runner::ToolOutcome> {
        let edit_context = ctx.file_edit_context();
        match ctx.tool_config.edit_mode {
            crate::config::EditMode::Hashline => execute_hashline_edit(input, &edit_context),
            crate::config::EditMode::Replace => execute_replace_edit(input, &edit_context),
        }
    }
}

fn format_hashline_read(display_path: &str, tag: &str, start_line: usize, content: &str) -> String {
    let mut out = format!("[{display_path}#{tag}]");
    for (idx, line) in crate::tools::snapshot::split_content_lines(content)
        .iter()
        .enumerate()
    {
        out.push('\n');
        out.push_str(&format!("{}:{line}", start_line + idx));
    }
    out
}

fn format_numbered_read(start_line: usize, content: &str) -> String {
    let mut out = String::new();
    for (idx, line) in crate::tools::snapshot::split_content_lines(content)
        .iter()
        .enumerate()
    {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("{}:{line}", start_line + idx));
    }
    out
}

fn format_read_only_virtual(display_path: &str, start_line: usize, content: &str) -> String {
    let mut out = format!("[read-only virtual file: {display_path}]");
    for (idx, line) in crate::tools::snapshot::split_content_lines(content)
        .iter()
        .enumerate()
    {
        out.push('\n');
        out.push_str(&format!("{}:{line}", start_line + idx));
    }
    out
}

/// Existing-target permissions, distinguishing "absent" from "cannot read":
/// a swallowed metadata error would publish the file under wrong permissions.
enum ExistingTarget {
    Absent,
    Present(std::fs::Permissions),
}

fn existing_target(path: &Path) -> Result<ExistingTarget> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(ExistingTarget::Present(metadata.permissions())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ExistingTarget::Absent),
        Err(error) => Err(anyhow!(
            "failed to read metadata for {}: {error}",
            path.display()
        )),
    }
}

/// Create a hidden staging file next to `target`.
///
/// When the target already exists (final permissions known), the staging file
/// is created restrictively (0600 on Unix) so content is never stored with
/// wider permissions than the final target; new files keep the default umask
/// mode, matching previous user-file behavior.
fn stage_temp_file(
    target: &Path,
    kind: &str,
    restrict: bool,
) -> Result<(std::fs::File, std::path::PathBuf)> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temporary = parent.join(format!(".{name}.{kind}-{}-{stamp}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if restrict {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = restrict;
    let file = options.open(&temporary)?;
    Ok((file, temporary))
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    use std::io::Write as _;

    let existing = existing_target(path)?;
    let (mut file, temporary) = stage_temp_file(
        path,
        "mink-edit",
        matches!(existing, ExistingTarget::Present(_)),
    )?;
    let result = (|| -> Result<()> {
        if let ExistingTarget::Present(permissions) = &existing {
            file.set_permissions(permissions.clone())?;
        }
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[path = "file/hashline_edit.rs"]
mod hashline_edit;
pub(crate) use hashline_edit::execute_hashline_edit;
#[cfg(test)]
pub(crate) use hashline_edit::{MISMATCH_CONTEXT_MAX_LINES, format_mismatch_anchor_context};

fn changed_line_window(old: &str, new: &str) -> (usize, usize) {
    let diff = TextDiff::from_lines(old, new);
    let mut start = usize::MAX;
    let mut end = 1;
    for operation in diff
        .ops()
        .iter()
        .filter(|operation| operation.tag() != similar::DiffTag::Equal)
    {
        start = start.min(operation.new_range().start + 1);
        end = end.max(
            operation
                .new_range()
                .end
                .max(operation.new_range().start + 1),
        );
    }
    if start == usize::MAX {
        (1, 1)
    } else {
        (start, end)
    }
}

fn display_relative_path(cwd: &Path, path: &Path) -> String {
    // Canonicalized session cwd may differ lexically (e.g. /tmp symlink):
    // try both so canonical paths still display relative to the workspace.
    if let Ok(canonical_cwd) = std::fs::canonicalize(cwd)
        && let Ok(relative) = path.strip_prefix(&canonical_cwd)
    {
        return relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
    }
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

#[path = "file/replace_edit.rs"]
mod replace_edit;
pub(crate) use replace_edit::execute_replace_edit;
#[cfg(test)]
pub(crate) use replace_edit::resolve_replace_target;

fn tool_outcome(content: String) -> super::runner::ToolOutcome {
    super::runner::ToolOutcome {
        conversation_content: String::new(),
        content,
        is_bash: false,
        exit_code: None,
        success: true,
        no_mutation: false,
        memo_candidate: None,
        diagnostics: Vec::new(),
        plan_command: None,
        state_metadata: None,
        presentation: None,
        termination: None,
    }
}

#[path = "file/image_read.rs"]
mod image_read;
pub(crate) use image_read::{
    ImageReadKind, PreparedImage, VfsImageOutcome, classify_image_read, image_limits,
    prepare_image_read, prepare_vfs_candidate,
};

#[cfg(test)]
#[path = "file_tests.rs"]
mod tests;
