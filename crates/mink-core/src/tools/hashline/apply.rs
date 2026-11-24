use super::balance::{insertion_boundary, repair_replacement, section_delimiter_delta};
use super::*;

/// True when applying `operations` to `text` would not change it because the
/// requested final content is already present at the exact target positions.
/// Used to turn an explainable no-op into an idempotent success; anything
/// ambiguous returns false so callers keep failing closed.
pub fn already_applied(text: &str, operations: &[Operation]) -> bool {
    let lines = crate::tools::snapshot::split_content_lines(text);
    let matches = |start0: usize, body: &[String]| -> bool {
        if body.len() > lines.len().saturating_sub(start0) {
            return false;
        }
        body.iter()
            .zip(lines[start0..start0 + body.len()].iter())
            .all(|(wanted, actual)| wanted == actual)
    };
    operations.iter().all(|operation| match operation {
        Operation::Put { start, end, body } => {
            let (Ok(start), Ok(end)) = (
                resolve_bound(&lines, start, ""),
                resolve_bound(&lines, end, ""),
            ) else {
                return false;
            };
            if start < 1 || end < start || end > lines.len() {
                return false;
            }
            matches(start - 1, body)
        }
        Operation::Insert { cursor, body } => match cursor {
            Cursor::Head => matches(0, body),
            Cursor::Tail => {
                let start0 = lines.len().saturating_sub(body.len());
                matches(start0, body)
            }
            Cursor::Before(n) => matches(n.saturating_sub(1), body),
            Cursor::After(n) => matches(*n, body),
        },
        _ => false,
    })
}

pub(super) fn resolve_bound(lines: &[String], bound: &RangeBound, path: &str) -> Result<usize> {
    match bound {
        RangeBound::Line(line) => Ok(*line),
        RangeBound::Anchor(text) => {
            let matches: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.trim() == text.trim())
                .map(|(index, _)| index + 1)
                .collect();
            match matches.len() {
                0 => bail!(
                    "Error: anchor line not found in {path}: {text:?}; re-Read the file and copy the exact line text"
                ),
                1 => Ok(matches[0]),
                n => bail!(
                    "Error: anchor line is not unique in {path} ({n} matches): {text:?}; use a longer line that appears exactly once"
                ),
            }
        }
    }
}

pub fn anchor_lines(section: &Section) -> BTreeSet<usize> {
    anchor_lines_in_text(section, None)
}

/// Collect all 1-based line numbers referenced by `section`, resolving
/// `RangeBound::Anchor` bounds against `file_text` when provided so
/// `edit_enforce_seen_lines` cannot be bypassed via text anchors.
pub fn anchor_lines_in_text(section: &Section, file_text: Option<&str>) -> BTreeSet<usize> {
    let lines: Option<Vec<String>> =
        file_text.map(|text| text.split('\n').map(str::to_string).collect());
    let mut anchors = BTreeSet::new();
    for operation in &section.operations {
        match operation {
            Operation::Put { start, end, .. }
            | Operation::Cut { start, end, .. }
            | Operation::Paste {
                target: PasteTarget::Range { start, end },
                ..
            } => match (start, end) {
                (RangeBound::Line(start), RangeBound::Line(end)) => {
                    anchors.extend(*start..=*end);
                }
                (start_bound, end_bound) => {
                    if let Some(file_lines) = lines.as_deref()
                        && let (Ok(s), Ok(e)) = (
                            resolve_bound(file_lines, start_bound, &section.path),
                            resolve_bound(file_lines, end_bound, &section.path),
                        )
                        && s <= e
                    {
                        anchors.extend(s..=e);
                    }
                }
            },
            Operation::Insert { cursor, .. }
            | Operation::Paste {
                target: PasteTarget::Gap(cursor),
                ..
            } => match cursor {
                Cursor::Before(line) | Cursor::After(line) => {
                    anchors.insert(*line);
                }
                Cursor::Head | Cursor::Tail => {}
            },
            Operation::Remove | Operation::Move { .. } => {}
        }
    }
    anchors
}

pub fn is_head_tail_only(section: &Section) -> bool {
    section.operations.iter().all(|operation| {
        matches!(
            operation,
            Operation::Insert {
                cursor: Cursor::Head | Cursor::Tail,
                ..
            } | Operation::Paste {
                target: PasteTarget::Gap(Cursor::Head | Cursor::Tail),
                ..
            }
        )
    })
}

pub fn remap_anchors(section: &Section, offset: isize) -> Result<Section> {
    let shift = |line: usize| -> Result<usize> {
        line.checked_add_signed(offset)
            .filter(|line| *line > 0)
            .ok_or_else(|| anyhow!("Error: stale recovery mapped an anchor outside the file"))
    };
    let mut section = section.clone();
    for operation in &mut section.operations {
        match operation {
            Operation::Put { start, end, .. }
            | Operation::Cut { start, end, .. }
            | Operation::Paste {
                target: PasteTarget::Range { start, end },
                ..
            } => {
                if let RangeBound::Line(line) = start {
                    *start = RangeBound::Line(shift(*line)?);
                }
                if let RangeBound::Line(line) = end {
                    *end = RangeBound::Line(shift(*line)?);
                }
            }
            Operation::Insert { cursor, .. }
            | Operation::Paste {
                target: PasteTarget::Gap(cursor),
                ..
            } => match cursor {
                Cursor::Before(line) | Cursor::After(line) => *line = shift(*line)?,
                Cursor::Head | Cursor::Tail => {}
            },
            Operation::Remove | Operation::Move { .. } => {}
        }
    }
    Ok(section)
}

pub fn apply(text: &str, section: &Section, clipboard: &mut Clipboard) -> Result<ApplyResult> {
    let had_trailing_newline = text.ends_with('\n');
    let lines = crate::tools::snapshot::split_content_lines(text);
    let mut deleted = BTreeSet::new();
    let mut insertions: BTreeMap<usize, Vec<Vec<String>>> = BTreeMap::new();
    let mut warnings = section.warnings.clone();

    for operation in &section.operations {
        match operation {
            Operation::Put { start, end, .. }
            | Operation::Cut { start, end, .. }
            | Operation::Paste {
                target: PasteTarget::Range { start, end },
                ..
            } => {
                let start = resolve_bound(&lines, start, &section.path)?;
                let end = resolve_bound(&lines, end, &section.path)?;
                ensure!(
                    start <= end,
                    "Error: anchor range start line {start} is after end line {end}"
                );
                validate_range(start, end, lines.len(), &section.path)?;
                ensure_target_free(&mut deleted, start, end)?;
            }
            _ => {}
        }
    }
    let section_delta = section_delimiter_delta(&lines, section);

    for operation in &section.operations {
        match operation {
            Operation::Put { start, end, body } => {
                let start = resolve_bound(&lines, start, &section.path)?;
                let end = resolve_bound(&lines, end, &section.path)?;
                let repair = repair_replacement(&lines, start, end, body, section_delta)?;
                for line in repair.keep_lines {
                    deleted.remove(&line);
                }
                warnings.extend(repair.warnings);
                if !repair.body.is_empty() {
                    insertions.entry(start - 1).or_default().push(repair.body);
                }
            }
            Operation::Cut {
                start,
                end,
                register,
            } => {
                let start = resolve_bound(&lines, start, &section.path)?;
                let end = resolve_bound(&lines, end, &section.path)?;
                let captured = lines[start - 1..end].to_vec();
                if let Some(register) = register {
                    clipboard.named.insert(register.clone(), captured);
                } else {
                    clipboard.anonymous = Some(captured);
                    clipboard
                        .pending_anonymous_cuts
                        .push(format!("CUT {start}.={end}"));
                }
            }
            Operation::Insert { cursor, body } => {
                let (boundary, shifted) =
                    insertion_boundary(cursor, body, &lines, &deleted, &section.path)?;
                if let Some((from, to)) = shifted {
                    warnings.push(format!(
                        "shifted PUT > landing from line {from} to structural closer line {to}"
                    ));
                }
                insertions.entry(boundary).or_default().push(body.clone());
            }
            Operation::Paste { target, register } => {
                let (start, _end) = match target {
                    PasteTarget::Range { start, end } => (
                        resolve_bound(&lines, start, &section.path)?,
                        resolve_bound(&lines, end, &section.path)?,
                    ),
                    PasteTarget::Gap(_) => (0, 0),
                };
                let body = if let Some(register) = register {
                    clipboard
                        .named
                        .get(register)
                        .cloned()
                        .ok_or_else(|| anyhow!("Error: unknown Hashline register @{register}"))?
                } else {
                    ensure!(
                        clipboard.pending_anonymous_cuts.len() <= 1,
                        "Error: anonymous PUT is ambiguous after multiple CUT operations; name the register"
                    );
                    let body = clipboard.anonymous.clone().ok_or_else(|| {
                        anyhow!(
                            "Error: anonymous PUT requires a prior unlabeled CUT in this Edit call"
                        )
                    })?;
                    clipboard.pending_anonymous_cuts.clear();
                    body
                };
                match target {
                    PasteTarget::Gap(cursor) => {
                        let (boundary, shifted) =
                            insertion_boundary(cursor, &body, &lines, &deleted, &section.path)?;
                        if let Some((from, to)) = shifted {
                            warnings.push(format!(
                                "shifted register PUT landing from line {from} to {to}"
                            ));
                        }
                        insertions.entry(boundary).or_default().push(body);
                    }
                    PasteTarget::Range { .. } => {
                        insertions.entry(start - 1).or_default().push(body);
                    }
                }
            }
            Operation::Remove | Operation::Move { .. } => {}
        }
    }

    let mut output = Vec::new();
    for boundary in 0..=lines.len() {
        if let Some(groups) = insertions.get(&boundary) {
            for group in groups {
                output.extend(group.iter().cloned());
            }
        }
        if boundary < lines.len() && !deleted.contains(&(boundary + 1)) {
            output.push(lines[boundary].clone());
        }
    }
    let mut result = output.join("\n");
    if had_trailing_newline && !result.is_empty() {
        result.push('\n');
    }
    Ok(ApplyResult {
        text: result,
        warnings,
    })
}

fn validate_range(start: usize, end: usize, line_count: usize, path: &str) -> Result<()> {
    ensure!(
        start > 0 && start <= end && end <= line_count,
        "Error: range {start}.={end} is outside {path} ({line_count} lines)"
    );
    Ok(())
}

fn ensure_target_free(targeted: &mut BTreeSet<usize>, start: usize, end: usize) -> Result<()> {
    for line in start..=end {
        ensure!(
            targeted.insert(line),
            "Error: anchor line {line} is already targeted by another hunk"
        );
    }
    Ok(())
}

pub(super) fn cursor_boundary(cursor: &Cursor, line_count: usize, path: &str) -> Result<usize> {
    match cursor {
        Cursor::Head => Ok(0),
        Cursor::Tail => Ok(line_count),
        Cursor::Before(line) => {
            ensure!(
                *line > 0 && *line <= line_count,
                "Error: anchor line {line} is outside {path} ({line_count} lines)"
            );
            Ok(line - 1)
        }
        Cursor::After(line) => {
            ensure!(
                *line > 0 && *line <= line_count,
                "Error: anchor line {line} is outside {path} ({line_count} lines)"
            );
            Ok(*line)
        }
    }
}
