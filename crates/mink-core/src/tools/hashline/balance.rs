use super::apply::{cursor_boundary, resolve_bound};
use super::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct DelimiterBalance {
    pub(super) paren: isize,
    pub(super) bracket: isize,
    pub(super) brace: isize,
}

impl DelimiterBalance {
    fn add(self, other: Self) -> Self {
        Self {
            paren: self.paren + other.paren,
            bracket: self.bracket + other.bracket,
            brace: self.brace + other.brace,
        }
    }

    fn subtract(self, other: Self) -> Self {
        Self {
            paren: self.paren - other.paren,
            bracket: self.bracket - other.bracket,
            brace: self.brace - other.brace,
        }
    }

    fn is_zero(self) -> bool {
        self == Self::default()
    }
}

pub(super) fn section_delimiter_delta(
    lines: &[String],
    section: &Section,
) -> Option<DelimiterBalance> {
    let mut delta = DelimiterBalance::default();
    for operation in &section.operations {
        let contribution = match operation {
            Operation::Put { start, end, body } => {
                let (Ok(start), Ok(end)) = (
                    resolve_bound(lines, start, &section.path),
                    resolve_bound(lines, end, &section.path),
                ) else {
                    return None;
                };
                balance(body).subtract(balance(&lines[start - 1..end]))
            }
            Operation::Cut { start, end, .. } => {
                let (Ok(start), Ok(end)) = (
                    resolve_bound(lines, start, &section.path),
                    resolve_bound(lines, end, &section.path),
                ) else {
                    return None;
                };
                DelimiterBalance::default().subtract(balance(&lines[start - 1..end]))
            }
            Operation::Insert { body, .. } => balance(body),
            Operation::Paste { .. } => return None,
            Operation::Remove | Operation::Move { .. } => DelimiterBalance::default(),
        };
        delta = delta.add(contribution);
    }
    Some(delta)
}

#[derive(Debug)]
pub(super) struct ReplacementRepair {
    pub(super) body: Vec<String>,
    pub(super) keep_lines: Vec<usize>,
    pub(super) warnings: Vec<String>,
}

pub(super) fn repair_replacement(
    lines: &[String],
    start: usize,
    end: usize,
    body: &[String],
    section_delta: Option<DelimiterBalance>,
) -> Result<ReplacementRepair> {
    let mut body = body.to_vec();
    let mut warnings = Vec::new();
    repair_replacement_indentation(lines, start, end, &mut body, &mut warnings);

    let leading_limit = body.len().min(start.saturating_sub(1));
    let mut leading = 0;
    for count in 1..=leading_limit {
        if body[..count] == lines[start - 1 - count..start - 1] {
            leading = count;
        }
    }
    let trailing_limit = body.len().min(lines.len().saturating_sub(end));
    let mut trailing = 0;
    for count in 1..=trailing_limit {
        if body[body.len() - count..] == lines[end..end + count] {
            trailing = count;
        }
    }
    if leading > 0 && trailing > 0 && leading + trailing < body.len() {
        let dropped = balance(&body[..leading]).add(balance(&body[body.len() - trailing..]));
        let delta = balance(&body).subtract(balance(&lines[start - 1..end]));
        if dropped.is_zero() || dropped == delta {
            body = body[leading..body.len() - trailing].to_vec();
            warnings.push(format!(
                "Auto-repaired a replacement boundary echo at line {start}: dropped {leading} leading and {trailing} trailing payload line(s) already present outside the range. Issue the payload as the final desired content for the selected range only — never restate unchanged lines bordering the range."
            ));
            return Ok(ReplacementRepair {
                body,
                keep_lines: Vec::new(),
                warnings,
            });
        }
    }

    let source = &lines[start - 1..end];
    let delta = balance(&body).subtract(balance(source));
    if !delta.is_zero() {
        if trailing > 0 && balance(&body[body.len() - trailing..]) == delta {
            body.truncate(body.len() - trailing);
            warnings.push(format!(
                "Auto-repaired a delimiter-balance mismatch in the replacement at line {start}: dropped {trailing} duplicated trailing payload line(s) already present below the range. Issue the payload as the final desired content only — never restate or omit a closing bracket bordering the range."
            ));
            return Ok(ReplacementRepair {
                body,
                keep_lines: Vec::new(),
                warnings,
            });
        }
        if leading > 0 && balance(&body[..leading]) == delta {
            body.drain(..leading);
            warnings.push(format!(
                "Auto-repaired a delimiter-balance mismatch in the replacement at line {start}: dropped {leading} duplicated leading payload line(s) already present above the range. Issue the payload as the final desired content only — never restate or omit a closing bracket bordering the range."
            ));
            return Ok(ReplacementRepair {
                body,
                keep_lines: Vec::new(),
                warnings,
            });
        }
    }

    // A neutral one-sided echo on a multi-line rewrite is the common
    // off-by-one keeper mistake. Reject the short/ambiguous form instead of
    // silently deleting source lines not represented by the payload.
    if (leading > 0) ^ (trailing > 0) {
        let (side, count, echo) = if leading > 0 {
            ("leading", leading, &body[..leading])
        } else {
            ("trailing", trailing, &body[body.len() - trailing..])
        };
        let range_len = end - start + 1;
        if range_len > 1 && balance(echo).is_zero() && count < body.len() {
            if body.len() < range_len + count {
                let where_text = if side == "leading" {
                    "opens by restating"
                } else {
                    "ends by restating"
                };
                bail!(
                    "`PUT {start}.={end}:` rejected: the body {where_text} the {count} line(s) just {} the range, but is too short to be the full final content of the widened range — applying it as-is or auto-repairing would delete range line(s) the body never restates. Re-issue with the range covering exactly the lines that change and the body as their complete final content: drop the restated keeper from the body, or widen the range to consume it.",
                    if side == "leading" { "above" } else { "below" }
                );
            }
            if side == "leading" {
                body.drain(..count);
            } else {
                body.truncate(body.len() - count);
            }
            warnings.push(format!(
                "Auto-repaired a replacement boundary echo at line {start}: dropped {count} {side} payload line(s) identical to the surviving line(s) just {} the range. The range was one line short of the content you retyped — issue the payload as the final content for the selected range only, and widen the range to consume any keeper you restate.",
                if side == "leading" { "above" } else { "below" }
            ));
            return Ok(ReplacementRepair {
                body,
                keep_lines: Vec::new(),
                warnings,
            });
        }
    }

    // When the payload leaves unmatched openers and the selected range ends in
    // bare structural closers, retain only the suffix needed to rebalance it.
    let delta = balance(&body).subtract(balance(source));
    let mut keep_lines = Vec::new();
    let mut remaining = delta;
    for line in (start..=end).rev() {
        let text = &lines[line - 1];
        if !is_structural_closer(text) {
            break;
        }
        let closer = balance(std::slice::from_ref(text));
        let next = remaining.subtract(DelimiterBalance {
            paren: -closer.paren,
            bracket: -closer.bracket,
            brace: -closer.brace,
        });
        if next.paren.abs() <= remaining.paren.abs()
            && next.bracket.abs() <= remaining.bracket.abs()
            && next.brace.abs() <= remaining.brace.abs()
        {
            keep_lines.push(line);
            remaining = next;
        }
        if remaining.is_zero() {
            break;
        }
    }
    let section_can_cover = section_delta.is_some_and(|total| balance_covers(total, delta));
    if !keep_lines.is_empty() && remaining.is_zero() && section_can_cover {
        let closer_indent = leading_indent(&lines[keep_lines[0] - 1]);
        let claims_inside = body
            .iter()
            .filter(|line| !line.trim().is_empty())
            .any(|line| leading_indent(line).len() > closer_indent.len());
        if !claims_inside {
            bail!(
                "`PUT {start}.={end}:` rejected: replacing a structural closer with content at the closer's own indentation is unsafe because whether it belongs before or after the closer is ambiguous"
            );
        }
        keep_lines.sort_unstable();
        warnings.push(format!(
            "Auto-repaired a delimiter-balance mismatch in the replacement at line {start}: kept {} structural closing line(s) the range deleted without restating. Issue the payload as the final desired content only — never restate or omit a closing bracket bordering the range.",
            keep_lines.len()
        ));
    } else {
        keep_lines.clear();
    }
    Ok(ReplacementRepair {
        body,
        keep_lines,
        warnings,
    })
}

pub(super) fn balance_covers(available: DelimiterBalance, needed: DelimiterBalance) -> bool {
    fn covers(available: isize, needed: isize) -> bool {
        needed == 0 || available.signum() == needed.signum() && available.abs() >= needed.abs()
    }
    covers(available.paren, needed.paren)
        && covers(available.bracket, needed.bracket)
        && covers(available.brace, needed.brace)
}

pub(super) fn repair_replacement_indentation(
    lines: &[String],
    start: usize,
    end: usize,
    body: &mut [String],
    warnings: &mut Vec<String>,
) {
    if body.len() != end - start + 1 || start <= 1 {
        return;
    }
    let preceding = &lines[start - 2];
    let source_first = &lines[start - 1];
    let payload_first = &body[0];
    let preceding_indent = leading_indent(preceding);
    if !preceding.trim_end().ends_with('{')
        || source_first.len() - source_first.trim_start().len() <= preceding_indent.len()
        || payload_first.len() - payload_first.trim_start().len() > preceding_indent.len()
    {
        return;
    }
    let mut shift: Option<&str> = None;
    let mut matches = 0usize;
    for (source, payload) in lines[start - 1..end].iter().zip(body.iter()) {
        if source.trim().is_empty() || source.trim_start() != payload.trim_start() {
            continue;
        }
        let source_indent = leading_indent(source);
        let payload_indent = leading_indent(payload);
        let Some(candidate) = source_indent.strip_suffix(payload_indent) else {
            return;
        };
        if shift.is_some_and(|current| current != candidate) {
            return;
        }
        shift = Some(candidate);
        matches += 1;
    }
    let Some(shift) = shift else {
        return;
    };
    if shift.is_empty() || matches < 2 || matches * 2 <= body.len() {
        return;
    }
    for line in body.iter_mut().filter(|line| !line.trim().is_empty()) {
        line.insert_str(0, shift);
    }
    warnings.push(
        "Auto-indented a replacement body to match unchanged structural rows in its source range."
            .to_string(),
    );
}

pub(super) fn balance(lines: &[String]) -> DelimiterBalance {
    let mut result = DelimiterBalance::default();
    let mut block_comment = false;
    let mut quote = None;
    for line in lines {
        let chars = line.chars().collect::<Vec<_>>();
        let mut index = 0usize;
        while index < chars.len() {
            let ch = chars[index];
            let next = chars.get(index + 1).copied();
            if block_comment {
                if ch == '*' && next == Some('/') {
                    block_comment = false;
                    index += 2;
                    continue;
                }
                index += 1;
                continue;
            }
            if let Some(active) = quote {
                if ch == '\\' {
                    index += 2;
                    continue;
                }
                if ch == active {
                    quote = None;
                }
                index += 1;
                continue;
            }
            if ch == '/' && next == Some('/') {
                break;
            }
            if ch == '/' && next == Some('*') {
                block_comment = true;
                index += 2;
                continue;
            }
            if matches!(ch, '"' | '`') {
                quote = Some(ch);
                index += 1;
                continue;
            }
            if ch == '\'' {
                // Distinguish Rust lifetimes/loop labels (`'a`, `'static`, `'outer:`)
                // from single-quoted string/char literals so apostrophes never swallow
                // following delimiters on the same line.
                let is_rust_lifetime = next.is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && {
                        let prev = index.checked_sub(1).map(|i| chars[i]);
                        let mut end = index + 2;
                        while end < chars.len()
                            && (chars[end].is_ascii_alphanumeric() || chars[end] == '_')
                        {
                            end += 1;
                        }
                        let after = chars.get(end).copied();
                        let ident = &chars[index + 1..end];
                        let is_static_or_anon =
                            ident == ['_'] || ident == ['s', 't', 'a', 't', 'i', 'c'];
                        after != Some('\'')
                            && (matches!(prev, Some('<' | '&'))
                                || matches!(after, Some('>' | '+' | ',' | ')' | ':'))
                                || is_static_or_anon)
                    };
                let has_closing_single_quote = !is_rust_lifetime && {
                    let mut scan = index + 1;
                    let mut found = false;
                    while scan < chars.len() {
                        if chars[scan] == '\\' {
                            scan += 2;
                            continue;
                        }
                        if chars[scan] == '\'' {
                            found = true;
                            break;
                        }
                        scan += 1;
                    }
                    found
                };
                if has_closing_single_quote {
                    quote = Some('\'');
                }
                index += 1;
                continue;
            }
            match ch {
                '(' => result.paren += 1,
                ')' => result.paren -= 1,
                '[' => result.bracket += 1,
                ']' => result.bracket -= 1,
                '{' => result.brace += 1,
                '}' => result.brace -= 1,
                _ => {}
            }
            index += 1;
        }
        if quote != Some('`') {
            quote = None;
        }
    }
    result
}

pub(super) fn insertion_boundary(
    cursor: &Cursor,
    body: &[String],
    lines: &[String],
    targeted: &BTreeSet<usize>,
    path: &str,
) -> Result<(usize, Option<(usize, usize)>)> {
    let literal = cursor_boundary(cursor, lines.len(), path)?;
    let Cursor::After(anchor) = cursor else {
        return Ok((literal, None));
    };
    let Some(target_indent) = body_target_indent(body) else {
        return Ok((literal, None));
    };
    let anchor_indent = leading_indent(&lines[anchor - 1]);
    if target_indent.len() >= anchor_indent.len() || !anchor_indent.starts_with(target_indent) {
        return Ok((literal, None));
    }
    let mut landing = *anchor;
    for line in anchor + 1..=lines.len() {
        let text = &lines[line - 1];
        if text.trim().is_empty() {
            continue;
        }
        if !is_structural_closer(text) {
            break;
        }
        let indent = leading_indent(text);
        if !indent.starts_with(target_indent) || targeted.contains(&line) {
            return Ok((literal, None));
        }
        landing = line;
        if indent.len() == target_indent.len() {
            break;
        }
    }
    if landing == *anchor {
        Ok((literal, None))
    } else {
        Ok((landing, Some((*anchor, landing))))
    }
}

pub(super) fn leading_indent(line: &str) -> &str {
    let length = line.len() - line.trim_start_matches([' ', '\t']).len();
    &line[..length]
}

fn body_target_indent(body: &[String]) -> Option<&str> {
    let mut rows = body
        .iter()
        .filter(|line| !line.trim().is_empty() && !is_structural_closer(line));
    let mut target = leading_indent(rows.next()?);
    for row in rows {
        let indent = leading_indent(row);
        if indent.starts_with(target) {
            continue;
        }
        if target.starts_with(indent) {
            target = indent;
        } else {
            return None;
        }
    }
    Some(target)
}

fn is_structural_closer(line: &str) -> bool {
    let trimmed = line.trim();
    let core = trimmed
        .strip_suffix(';')
        .or_else(|| trimmed.strip_suffix(','))
        .unwrap_or(trimmed);
    !core.is_empty() && core.chars().all(|ch| matches!(ch, ')' | ']' | '}'))
}
