use super::{
    atomic_write, changed_line_window, display_relative_path, inline_diff, resolve_tool_path,
    tool_outcome,
};
use crate::tools::snapshot::TextShape;
use anyhow::{Result, anyhow, bail, ensure};
use std::path::{Component, Path, PathBuf};

pub(crate) fn execute_replace_edit(
    input: &serde_json::Value,
    ctx: &crate::context::FileEditContext<'_>,
) -> Result<crate::tools::runner::ToolOutcome> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        path: String,
        edits: Vec<crate::tools::replace::ReplaceEntry>,
    }
    let args: Args = crate::tools::args::decode_args_context(
        input,
        "Replace Edit requires path and edits with old_text/new_text/all; legacy patch inputs are unsupported",
    )?;
    if args.edits.is_empty() {
        return Err(crate::tools::args::invalid_input(
            "Error: edits must contain at least one entry",
        ));
    }
    let path = resolve_replace_target(ctx.cwd, &args.path)?;
    let display_path = display_relative_path(ctx.cwd, &path);
    let mut output = Vec::new();
    let mut wrote = false;
    for (index, edit) in args.edits.iter().enumerate() {
        let original = std::fs::read_to_string(&path)
            .map_err(|error| anyhow!("Error: cannot read {}: {error}", path.display()))?;
        ensure!(
            original.len() <= ctx.file_write_max_bytes,
            "Error: file too large for Edit"
        );
        let (shape, normalized) = TextShape::decode(&original);
        let old_text = crate::tools::snapshot::normalize_snapshot_text(&edit.old_text);
        let new_text = crate::tools::snapshot::normalize_snapshot_text(&edit.new_text);
        let result = crate::tools::replace::replace_text(
            &normalized,
            &old_text,
            &new_text,
            edit.all,
            ctx.edit_fuzzy_match,
            ctx.edit_fuzzy_threshold,
            &display_path,
        )
        .map_err(|error| {
            anyhow!(
                "{}{}",
                error,
                if index > 0 {
                    format!("\n{index} earlier edit(s) in this call were already committed")
                } else {
                    String::new()
                }
            )
        })?;
        // Idempotent edits change nothing on disk: skip the write, the diff
        // and the mutation-epoch bump so mtime/inode stay stable and read
        // memos remain valid.
        if result.strategy == "idempotent" {
            output.push(format!(
                "Edit({}).{}: already applied (idempotent)\nThe requested final content is already present; no change was made.",
                display_path,
                index + 1
            ));
            continue;
        }
        let final_text = shape.restore(&result.content);
        ensure!(
            final_text.len() <= ctx.file_write_max_bytes,
            "Error: edited file would exceed file_write_max_bytes"
        );
        atomic_write(&path, &final_text)?;
        wrote = true;
        let (diff, added, removed) = inline_diff(&display_path, &normalized, &result.content)?;
        let first_changed = changed_line_window(&normalized, &result.content).0;
        output.push(format!(
            "Edit({}).{}: updated\nfirstChangedLine: {}\nmatchStrategy: {}\nmatchCount: {}\nlinesAdded: {}\nlinesRemoved: {}\nDiff:\n{}",
            display_path,
            index + 1,
            first_changed,
            result.strategy,
            result.count,
            added,
            removed,
            diff
        ));
    }
    let mut outcome = tool_outcome(output.join("\n\n"));
    outcome.no_mutation = !wrote;
    Ok(outcome)
}

pub(crate) fn resolve_replace_target(cwd: &Path, authored: &str) -> Result<PathBuf> {
    let direct = resolve_tool_path(cwd, authored)?;
    if direct.is_file() {
        return Ok(crate::tools::snapshot::canonical_snapshot_path(&direct));
    }
    let suffix = Path::new(authored)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_owned()),
            _ => None,
        })
        .collect::<PathBuf>();
    ensure!(
        !suffix.as_os_str().is_empty(),
        "Error: file not found: {authored}"
    );
    let mut candidates = ignore::WalkBuilder::new(cwd)
        .standard_filters(true)
        .parents(false)
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .map(|entry| entry.into_path())
        .filter(|path| path.ends_with(&suffix))
        .take(6)
        .collect::<Vec<_>>();
    candidates.sort();
    match candidates.as_slice() {
        [path] => Ok(crate::tools::snapshot::canonical_snapshot_path(path)),
        [] => bail!("Error: file not found: {authored}"),
        _ => bail!(
            "Error: path suffix {authored:?} is ambiguous: {}",
            candidates
                .iter()
                .map(|path| display_relative_path(cwd, path))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
