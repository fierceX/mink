use super::{
    atomic_write, changed_line_window, display_relative_path, format_hashline_read, inline_diff,
    resolve_tool_path, stage_temp_file, tool_outcome,
};
use crate::tools::snapshot::TextShape;
use anyhow::{Result, anyhow, bail, ensure};
use similar::TextDiff;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn move_file_noclobber(source: &Path, destination: &Path, replacement: Option<&str>) -> Result<()> {
    use std::io::Write as _;

    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    ensure!(
        !destination.exists(),
        "MV destination already exists: {}",
        destination.display()
    );

    if let Some(content) = replacement {
        let source_permissions = std::fs::metadata(source)
            .map(|metadata| metadata.permissions())
            .ok();
        let (mut file, temporary) =
            stage_temp_file(destination, "mink-move", source_permissions.is_some())?;
        let operation = (|| -> Result<()> {
            if let Some(permissions) = source_permissions {
                file.set_permissions(permissions)?;
            }
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            // hard_link is the portable no-clobber publication primitive used
            // here: it fails if destination appeared after preflight.
            std::fs::hard_link(&temporary, destination)?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&temporary);
        operation?;
    } else {
        std::fs::hard_link(source, destination)?;
    }

    if let Err(error) = std::fs::remove_file(source) {
        let _ = std::fs::remove_file(destination);
        return Err(error.into());
    }
    Ok(())
}

#[derive(Debug)]
enum HashlineAction {
    Write {
        updated: String,
        shape: TextShape,
    },
    Remove,
    Move {
        destination: PathBuf,
        updated: String,
        shape: TextShape,
    },
}

#[derive(Debug)]
struct PreparedHashline {
    path: PathBuf,
    display_path: String,
    normalized_original: String,
    action: HashlineAction,
    warnings: Vec<String>,
    clipboard_after: crate::tools::hashline::Clipboard,
}

pub(crate) fn execute_hashline_edit(
    input: &serde_json::Value,
    ctx: &crate::context::FileEditContext<'_>,
) -> Result<crate::tools::runner::ToolOutcome> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        input: String,
    }
    let args: Args = crate::tools::args::decode_args_context(
        input,
        "Hashline Edit accepts only {\"input\": \"[PATH#TAG]...\"}; legacy path/patch and old_string/new_string inputs are unsupported",
    )?;
    let patch = crate::tools::hashline::parse(&args.input)
        .map_err(|error| crate::tools::args::invalid_input(format!("{error:#}")))?;
    let mut store = ctx
        .snapshots
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut clipboard =
        crate::tools::hashline::Clipboard::with_named(store.named_clipboard().clone());
    let mut prepared = Vec::new();
    let mut canonical_targets = BTreeSet::new();
    let mut move_destinations = BTreeSet::new();

    for authored in &patch.sections {
        let authored_path = resolve_tool_path(ctx.cwd, &authored.path)?;
        let (path, recovered_path) = if authored_path.exists() {
            (authored_path, None)
        } else {
            let file_name = authored_path
                .file_name()
                .ok_or_else(|| anyhow!("Error: {} has no recoverable filename", authored.path))?;
            let recovered = store
                .unique_path_for_tag_and_name(&authored.tag, file_name, &authored_path)
                .map_err(|message| {
                    anyhow!("Error: {} does not exist and {message}", authored.path)
                })?;
            (recovered, Some(authored.path.clone()))
        };
        let canonical = crate::tools::snapshot::canonical_snapshot_path(&path);
        ensure!(
            canonical_targets.insert(canonical.clone()),
            "Error: multiple hashline sections resolve to the same canonical path {}",
            canonical.display()
        );
        store.begin_noop_attempt(&canonical, &args.input);
        let original = std::fs::read_to_string(&canonical)
            .map_err(|error| anyhow!("Error: cannot read {}: {error}", canonical.display()))?;
        ensure!(
            original.len() <= ctx.file_write_max_bytes,
            "Error: file too large for Edit ({} bytes > {} bytes): {}",
            original.len(),
            ctx.file_write_max_bytes,
            canonical.display()
        );
        let (shape, normalized) = TextShape::decode(&original);
        let current_tag = crate::tools::snapshot::compute_file_tag(&normalized);
        let authored_anchors = crate::tools::hashline::anchor_lines(authored);
        let versions = store.versions(&canonical, &authored.tag);
        if versions.is_empty() {
            let context = format_mismatch_anchor_context(&normalized, &authored_anchors);
            return Err(crate::tools::args::invalid_input(format!(
                "Error: snapshot tag #{} for {} is unknown and does not belong to this session. Do not invent tags or reuse a tag from another session.\nCurrent content hash (diagnostic only): #{}. This hash is not an authorized snapshot tag and must not be used to retry.\nUse Read or Grep to obtain a verifiable new [PATH#TAG] header before editing again.{}",
                authored.tag, authored.path, current_tag, context
            )));
        }

        let mut warnings = Vec::new();
        if let Some(authored_path) = recovered_path {
            warnings.push(format!(
                "path {authored_path:?} does not exist; matched its filename and snapshot tag #{} to {}",
                authored.tag,
                display_relative_path(ctx.cwd, &canonical)
            ));
        }
        let (section, snapshot) = if current_tag.eq_ignore_ascii_case(&authored.tag) {
            let snapshot = versions
                .iter()
                .find(|snapshot| {
                    snapshot.text == normalized
                        || hash_equivalent_snapshot_text(&snapshot.text, &normalized)
                })
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "Error: tag #{} collides with unobserved content for {}; re-read the file",
                        authored.tag,
                        authored.path
                    )
                })?;
            (authored.clone(), snapshot)
        } else if crate::tools::hashline::is_head_tail_only(authored) {
            warnings.push(format!(
                "stale snapshot #{}: HEAD/TAIL-only operations were applied to the current file",
                authored.tag
            ));
            (authored.clone(), versions[0].clone())
        } else {
            let mut recovered = Vec::new();
            for snapshot in versions {
                if let Ok(offset) =
                    recover_uniform_offset(&snapshot.text, &normalized, &authored_anchors)
                    && let Ok(section) = crate::tools::hashline::remap_anchors(authored, offset)
                {
                    recovered.push((section, snapshot, offset));
                }
            }
            if recovered.len() != 1 {
                let context = format_mismatch_anchor_context(&normalized, &authored_anchors);
                let guidance = if store.is_edit_result_tag(&canonical, &current_tag) {
                    format!(
                        "The current content matches [{}#{}], a header returned by an earlier successful Edit in this session; that exact response header may be reused directly.",
                        authored.path, current_tag
                    )
                } else {
                    let current_hint = store
                        .latest_tag(&canonical)
                        .filter(|tag| !tag.eq_ignore_ascii_case(&current_tag))
                        .map(|tag| {
                            format!(
                                "\nThe last known snapshot [{}#{tag}] predates the current content (the file drifted after that snapshot) and cannot be reused; editing against it would fail again. Re-Read the file and use the newly returned header.",
                                authored.path
                            )
                        })
                        .unwrap_or_default();
                    format!(
                        "The file has drifted outside a successful Edit response. Re-run Read or Grep and use the newly returned header; do not retry with the diagnostic hash below.{current_hint}"
                    )
                };
                bail!(
                    "Error: stale snapshot #{} for {} could not be recovered unambiguously. An anchor changed, was deleted/split, repeated, or mapped with an inconsistent offset.\nCurrent content hash (diagnostic): #{}. {}{}",
                    authored.tag,
                    authored.path,
                    current_tag,
                    guidance,
                    context
                );
            }
            let (section, snapshot, offset) = recovered.remove(0);
            warnings.push(format!(
                "recovered stale snapshot #{} with a uniform {offset:+} line offset",
                authored.tag
            ));
            (section, snapshot)
        };

        if ctx.edit_enforce_seen_lines {
            let missing = authored_anchors
                .difference(&snapshot.seen_lines)
                .copied()
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                let lines = crate::tools::snapshot::split_content_lines(&snapshot.text);
                let preview = missing
                    .iter()
                    .take(200)
                    .filter_map(|line| lines.get(line - 1).map(|text| format!("{line}:{text}")))
                    .collect::<Vec<_>>()
                    .join("\n");
                let message = format!(
                    "Error: hashline anchors were not shown by Read/Grep for {}: {}{}{}",
                    authored.path,
                    missing
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                    if preview.is_empty() { "" } else { "\n" },
                    preview
                );
                let fully_echoed = missing.len() <= 200
                    && missing.iter().all(|line| {
                        lines
                            .get(line.saturating_sub(1))
                            .is_some_and(|text| text.len() <= 500)
                    })
                    && message.len() <= ctx.tool_result_max_bytes;
                if fully_echoed {
                    store.add_seen_lines(
                        &canonical,
                        &snapshot.tag,
                        &snapshot.text,
                        missing.iter().copied(),
                    );
                }
                bail!(message);
            }
        }

        let applied = crate::tools::hashline::apply(&normalized, &section, &mut clipboard)?;
        warnings.extend(applied.warnings);
        let updated = applied.text;
        let file_operation = section
            .operations
            .last()
            .and_then(|operation| match operation {
                crate::tools::hashline::Operation::Remove => Some((None, true)),
                crate::tools::hashline::Operation::Move { destination } => {
                    Some((Some(destination.as_str()), false))
                }
                _ => None,
            });
        let action = match file_operation {
            Some((None, true)) => {
                ensure!(
                    current_tag.eq_ignore_ascii_case(&authored.tag),
                    "Error: REM refuses a stale file tag; re-read {}",
                    authored.path
                );
                HashlineAction::Remove
            }
            Some((Some(destination), false)) => {
                let destination = resolve_tool_path(ctx.cwd, destination)?;
                let destination = crate::tools::snapshot::canonical_snapshot_path(&destination);
                ensure!(
                    !destination.exists(),
                    "Error: MV destination already exists: {}",
                    destination.display()
                );
                ensure!(
                    move_destinations.insert(destination.clone()),
                    "Error: multiple Hashline sections move to the same destination {}",
                    destination.display()
                );
                ensure!(
                    !canonical_targets.contains(&destination),
                    "Error: MV destination is also a source path in this Hashline batch: {}",
                    destination.display()
                );
                let final_text = shape.restore(&updated);
                ensure!(
                    final_text.len() <= ctx.file_write_max_bytes,
                    "Error: moved file would exceed file_write_max_bytes"
                );
                HashlineAction::Move {
                    destination,
                    updated,
                    shape,
                }
            }
            _ => {
                if updated == normalized {
                    if patch.sections.len() != 1 {
                        bail!(
                            "Error: Hashline batch preflight found a no-op section for {}; no files were committed. Remove or change the no-op section before retrying.",
                            authored.path
                        );
                    }
                    // The requested final content is already present at the exact target positions.
                    if crate::tools::hashline::already_applied(
                        &normalized,
                        &patch.sections[0].operations,
                    ) {
                        // Idempotent success writes nothing: keep the
                        // mutation epoch untouched so read memos stay valid
                        // (mirrors the Replace-mode idempotent path).
                        let mut outcome = tool_outcome(format!(
                            "Edit({}): already applied (idempotent)\nThe requested final content is already present; no change was made.",
                            authored.path
                        ));
                        outcome.no_mutation = true;
                        return Ok(outcome);
                    }
                    let count = store.note_noop(&canonical, &args.input);
                    if count >= 3 {
                        bail!(
                            "Error: identical Hashline payload produced no changes three consecutive times for {}. The same payload will continue to fail; change the operation or re-check the file and its anchors before retrying.",
                            authored.path
                        );
                    }
                    // Soft no-op writes nothing either: only the no-op
                    // counter changes, never the mutation epoch.
                    let mut outcome = tool_outcome(format!(
                        "Edit({}): no changes (soft no-op {count}/2)\nNo file or clipboard state was changed. Do not expand the edit range. Confirm whether the intended change is already present and re-check the target anchors before retrying.",
                        authored.path
                    ));
                    outcome.no_mutation = true;
                    return Ok(outcome);
                }
                let final_text = shape.restore(&updated);
                ensure!(
                    final_text.len() <= ctx.file_write_max_bytes,
                    "Error: edited file would exceed file_write_max_bytes"
                );
                HashlineAction::Write { updated, shape }
            }
        };
        prepared.push(PreparedHashline {
            path: canonical,
            display_path: authored.path.clone(),
            normalized_original: normalized,
            action,
            warnings,
            clipboard_after: clipboard.clone(),
        });
    }

    let section_names = prepared
        .iter()
        .map(|item| item.display_path.clone())
        .collect::<Vec<_>>();
    let mut rendered = Vec::new();
    let mut committed = Vec::new();
    for (index, item) in prepared.into_iter().enumerate() {
        let result: Result<String> = (|| match item.action {
            HashlineAction::Write { updated, shape } => {
                let final_text = shape.restore(&updated);
                atomic_write(&item.path, &final_text)?;
                let (diff, added, removed) =
                    inline_diff(&item.display_path, &item.normalized_original, &updated)?;
                let (first_changed, last_changed) =
                    changed_line_window(&item.normalized_original, &updated);
                let lines = crate::tools::snapshot::split_content_lines(&updated);
                let start = first_changed.saturating_sub(12).max(1);
                let end = (last_changed + 12)
                    .min(lines.len())
                    .max(start.min(lines.len()));
                let visible = if lines.is_empty() {
                    1..1
                } else {
                    start..end.saturating_add(1)
                };
                let snapshot = store.record_edit(&item.path, &updated, visible.clone());
                store.reset_noop(&item.path);
                let body = if lines.is_empty() || start > end {
                    format!("[{}#{}]", item.display_path, snapshot.tag)
                } else {
                    format_hashline_read(
                        &item.display_path,
                        &snapshot.tag,
                        start,
                        &lines[start - 1..end].join("\n"),
                    )
                };
                Ok(format!(
                    "Edit({}): updated\n{}\nfirstChangedLine: {}\nlinesAdded: {}\nlinesRemoved: {}{}\nDiff:\n{}",
                    item.display_path,
                    body,
                    first_changed,
                    added,
                    removed,
                    format_warnings(&item.warnings),
                    diff
                ))
            }
            HashlineAction::Remove => {
                let (diff, added, removed) =
                    inline_diff(&item.display_path, &item.normalized_original, "")?;
                std::fs::remove_file(&item.path)?;
                store.reset_noop(&item.path);
                Ok(format!(
                    "Edit({}): removed\nfirstChangedLine: 1\nlinesAdded: {}\nlinesRemoved: {}{}\nDiff:\n{}",
                    item.display_path,
                    added,
                    removed,
                    format_warnings(&item.warnings),
                    diff
                ))
            }
            HashlineAction::Move {
                destination,
                updated,
                shape,
            } => {
                let final_text = shape.restore(&updated);
                let replacement =
                    (updated != item.normalized_original).then_some(final_text.as_str());
                move_file_noclobber(&item.path, &destination, replacement)?;
                store.relocate(&item.path, &destination);
                let display = display_relative_path(ctx.cwd, &destination);
                let (diff, added, removed) =
                    inline_diff(&item.display_path, &item.normalized_original, &updated)?;
                let changed = (updated != item.normalized_original)
                    .then(|| changed_line_window(&item.normalized_original, &updated).0);
                let lines = crate::tools::snapshot::split_content_lines(&updated);
                let (start, end) = changed.map_or_else(
                    || (1, lines.len().min(25)),
                    |line| (line.saturating_sub(12).max(1), (line + 12).min(lines.len())),
                );
                let visible = if lines.is_empty() || start > end {
                    1..1
                } else {
                    start..end.saturating_add(1)
                };
                let snapshot = store.record_edit(&destination, &updated, visible);
                store.reset_noop(&destination);
                let body = if lines.is_empty() || start > end {
                    format!("[{display}#{}]", snapshot.tag)
                } else {
                    format_hashline_read(
                        &display,
                        &snapshot.tag,
                        start,
                        &lines[start - 1..end].join("\n"),
                    )
                };
                Ok(format!(
                    "Edit({}): moved -> {}\n{}\nfirstChangedLine: {}\nlinesAdded: {}\nlinesRemoved: {}{}\nDiff:\n{}",
                    item.display_path,
                    display,
                    body,
                    changed.map_or_else(|| "none".to_string(), |line| line.to_string()),
                    added,
                    removed,
                    format_warnings(&item.warnings),
                    if diff.is_empty() {
                        "(no content changes; path moved)"
                    } else {
                        &diff
                    }
                ))
            }
        })();
        match result {
            Ok(text) => {
                committed.push(item.display_path.clone());
                // A committed file already changed on disk even when a later
                // section fails: invalidate memos before continuing.
                ctx.bump_mutation();
                store.set_named_clipboard(item.clipboard_after.named().clone());
                rendered.push(text);
            }
            Err(error) => {
                let uncommitted = section_names[index + 1..].join(", ");
                bail!(
                    "Error: multi-file Hashline commit stopped; committed [{}]; failed {}: {error}; not committed [{}]",
                    committed.join(", "),
                    item.display_path,
                    uncommitted
                );
            }
        }
    }
    let content = rendered.join("\n\n");
    Ok(tool_outcome(content))
}

fn hash_equivalent_snapshot_text(left: &str, right: &str) -> bool {
    left.split('\n')
        .map(|line| line.trim_end_matches([' ', '\t', '\r']))
        .eq(right
            .split('\n')
            .map(|line| line.trim_end_matches([' ', '\t', '\r'])))
}

fn recover_uniform_offset(old: &str, current: &str, anchors: &BTreeSet<usize>) -> Result<isize> {
    ensure!(
        !anchors.is_empty(),
        "no content anchors are available for stale recovery"
    );
    let diff = TextDiff::from_lines(old, current);
    let old_lines = old.split('\n').collect::<Vec<_>>();
    let current_lines = current.split('\n').collect::<Vec<_>>();
    let mut line_map = vec![None; old_lines.len()];
    for operation in diff
        .ops()
        .iter()
        .filter(|operation| operation.tag() == similar::DiffTag::Equal)
    {
        for offset in 0..operation.old_range().len() {
            line_map[operation.old_range().start + offset] =
                Some(operation.new_range().start + offset);
        }
    }

    let duplicated_old = duplicated_line_values(&old_lines);
    let duplicated_current = duplicated_line_values(&current_lines);
    let sorted = anchors.iter().copied().collect::<Vec<_>>();
    let mut offsets = BTreeSet::new();
    let mut run_start = 0usize;
    while run_start < sorted.len() {
        let mut run_end = run_start;
        while run_end + 1 < sorted.len() && sorted[run_end + 1] == sorted[run_end] + 1 {
            run_end += 1;
        }
        let first = sorted[run_start];
        let last = sorted[run_end];
        let before = first.checked_sub(1).filter(|line| *line >= 1);
        let after = (last < old_lines.len()).then_some(last + 1);
        for anchor in &sorted[run_start..=run_end] {
            let old_index = anchor - 1;
            let mapped = line_map
                .get(old_index)
                .and_then(|mapped| *mapped)
                .ok_or_else(|| anyhow!("anchor line {anchor} changed or was deleted"))?;
            let offset = mapped as isize - old_index as isize;
            offsets.insert(offset);

            let context_matches = |line: usize| {
                let index = line - 1;
                line_map.get(index).and_then(|mapped| *mapped) == index.checked_add_signed(offset)
            };
            let duplicate = duplicated_old.contains(old_lines[old_index])
                || duplicated_current.contains(current_lines[mapped]);
            let context_valid = if duplicate {
                let mut checked = false;
                let mut valid = true;
                if let Some(line) = before {
                    checked = true;
                    valid &= context_matches(line);
                }
                if let Some(line) = after {
                    checked = true;
                    valid &= context_matches(line);
                }
                checked && valid
            } else {
                after.is_some_and(context_matches) || before.is_some_and(context_matches)
            };
            ensure!(
                context_valid,
                "anchor context at line {anchor} is changed or ambiguous"
            );
        }
        run_start = run_end + 1;
    }
    ensure!(offsets.len() == 1, "anchors do not share one line offset");
    Ok(*offsets.first().expect("one offset"))
}

fn duplicated_line_values<'a>(lines: &'a [&'a str]) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut duplicated = BTreeSet::new();
    for line in lines {
        if !seen.insert(*line) {
            duplicated.insert(*line);
        }
    }
    duplicated
}

fn format_warnings(warnings: &[String]) -> String {
    if warnings.is_empty() {
        String::new()
    } else {
        format!("\nWarnings:\n{}", warnings.join("\n"))
    }
}

const MISMATCH_CONTEXT_RADIUS: usize = 2;
pub(crate) const MISMATCH_CONTEXT_MAX_LINES: usize = 40;
const MISMATCH_CONTEXT_MAX_LINE_BYTES: usize = 500;

pub(crate) fn format_mismatch_anchor_context(content: &str, anchors: &BTreeSet<usize>) -> String {
    if anchors.is_empty() {
        return "\nAnchor context: unavailable (the section has no numbered anchors).".to_string();
    }

    let lines = crate::tools::snapshot::split_content_lines(content);
    if lines.is_empty() {
        return "\nAnchor context from current content: (file is empty).".to_string();
    }

    let mut requested = BTreeSet::new();
    let mut out_of_range = false;
    for anchor in anchors {
        if *anchor == 0 || *anchor > lines.len() {
            out_of_range = true;
            continue;
        }
        let start = anchor.saturating_sub(MISMATCH_CONTEXT_RADIUS).max(1);
        let end = anchor
            .saturating_add(MISMATCH_CONTEXT_RADIUS)
            .min(lines.len());
        requested.extend(start..=end);
    }

    if requested.is_empty() {
        return format!(
            "\nAnchor context from current content: unavailable (requested anchors are outside 1..={}).",
            lines.len()
        );
    }

    let total_requested = requested.len();
    let selected = requested
        .into_iter()
        .take(MISMATCH_CONTEXT_MAX_LINES)
        .collect::<Vec<_>>();
    let mut truncated = total_requested > selected.len() || out_of_range;
    let mut rendered = String::from(
        "\nAnchor context from current content (diagnostic only; this does not mark lines as seen or authorize a tag):",
    );
    let mut previous = None;
    for line_number in selected {
        if previous.is_some_and(|line| line + 1 != line_number) {
            rendered.push_str("\n  …");
        }
        let source = &lines[line_number - 1];
        let (text, line_truncated) = utf8_bounded_line(source, MISMATCH_CONTEXT_MAX_LINE_BYTES);
        truncated |= line_truncated;
        rendered.push('\n');
        rendered.push_str(if anchors.contains(&line_number) {
            "* "
        } else {
            "  "
        });
        rendered.push_str(&line_number.to_string());
        rendered.push(':');
        rendered.push_str(text);
        if line_truncated {
            rendered.push_str(" … [line truncated]");
        }
        previous = Some(line_number);
    }
    if truncated {
        rendered.push_str(
            "\n[Anchor context truncated to safety limits; use Read/Grep for complete current context.]",
        );
    }
    rendered
}

fn utf8_bounded_line(line: &str, max_bytes: usize) -> (&str, bool) {
    if line.len() <= max_bytes {
        return (line, false);
    }
    let mut end = max_bytes;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    (&line[..end], true)
}
