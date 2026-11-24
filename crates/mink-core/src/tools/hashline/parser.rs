use super::*;

#[derive(Debug, Clone)]
enum PendingKind {
    Put { start: RangeBound, end: RangeBound },
    Insert { cursor: Cursor },
}

#[derive(Debug, Clone)]
enum BodyRow {
    Explicit(String),
    Bare(String),
    Minus(String),
    Blank,
}

#[derive(Debug, Clone)]
struct Pending {
    kind: PendingKind,
    rows: Vec<BodyRow>,
    deferred_blanks: usize,
    line_num: usize,
}

pub fn parse(input: &str) -> Result<Patch> {
    ensure!(
        !input.trim().is_empty(),
        "Error: hashline input must not be empty"
    );
    let normalized = input
        .strip_prefix('\u{feff}')
        .unwrap_or(input)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let lines = normalized.split('\n').collect::<Vec<_>>();
    let mut sections = Vec::new();
    let mut current: Option<Section> = None;
    let mut pending: Option<Pending> = None;

    for (index, line) in lines.iter().enumerate() {
        let line_num = index + 1;
        let trimmed = line.trim();
        if trimmed == "*** Begin Patch" {
            continue;
        }
        if trimmed == "*** End Patch" || trimmed == "*** Abort" {
            break;
        }

        if let Some(parsed) = try_parse_header(trimmed, line_num)? {
            if let Some(pending) = pending.take() {
                flush_pending(current.as_mut(), pending)?;
            }
            if let Some(section) = current.take()
                && !section.operations.is_empty()
            {
                sections.push(section);
            }
            current = Some(parsed);
            continue;
        }

        if !is_operation_line(trimmed)
            && let Some(pending) = pending.as_mut()
        {
            if trimmed.is_empty() {
                pending.deferred_blanks += 1;
            } else {
                for _ in 0..pending.deferred_blanks {
                    pending.rows.push(BodyRow::Blank);
                }
                pending.deferred_blanks = 0;
                if let Some(payload) = line.strip_prefix('+') {
                    pending.rows.push(BodyRow::Explicit(payload.to_string()));
                } else if line.starts_with('-') {
                    pending.rows.push(BodyRow::Minus((*line).to_string()));
                } else {
                    pending.rows.push(BodyRow::Bare((*line).to_string()));
                }
            }
            continue;
        }

        if let Some(pending) = pending.take() {
            flush_pending(current.as_mut(), pending)?;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(section) = current.as_mut() else {
            if trimmed.starts_with('@') {
                bail!("Error: legacy @PATH#TAG headers are unsupported; use [PATH#TAG]");
            }
            bail!("Error: line {line_num}: hashline input must begin with [PATH#TAG]");
        };
        parse_operation(trimmed, line_num, section, &mut pending)?;
        ensure!(
            section.operations.len() <= MAX_OPERATIONS,
            "Error: too many hashline operations"
        );
    }

    if let Some(pending) = pending.take() {
        flush_pending(current.as_mut(), pending)?;
    }
    if let Some(section) = current.take()
        && !section.operations.is_empty()
    {
        sections.push(section);
    }
    ensure!(
        !sections.is_empty(),
        "Error: hashline input did not contain a [PATH#TAG] section"
    );
    for section in &sections {
        let file_ops = section
            .operations
            .iter()
            .filter(|operation| matches!(operation, Operation::Remove | Operation::Move { .. }))
            .count();
        ensure!(
            file_ops <= 1,
            "Error: section {} contains multiple file operations",
            section.path
        );
        if let Some(position) = section
            .operations
            .iter()
            .position(|operation| matches!(operation, Operation::Remove | Operation::Move { .. }))
        {
            ensure!(
                position + 1 == section.operations.len(),
                "Error: REM or MV must be the final operation in section {}",
                section.path
            );
        }
    }
    Ok(Patch { sections })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SectionHeader {
    pub(crate) path: String,
    pub(crate) tag: String,
}

/// Parse one `[PATH#TAG]` section header. This is the single authoritative
/// grammar for hash headers; callers that only need the path should use
/// [`section_header_path`] rather than reimplementing a weaker parser.
pub(crate) fn parse_section_header(line: &str, line_num: usize) -> Result<Option<SectionHeader>> {
    if !line.starts_with('[') {
        return Ok(None);
    }
    let body = line
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| anyhow!("Error: line {line_num}: malformed [PATH#TAG] header"))?;
    let body = strip_apply_patch_path_noise(body.trim());
    let (path, tag) = body.rsplit_once('#').ok_or_else(|| {
        anyhow!("Error: line {line_num}: header must include a four-hex snapshot tag")
    })?;
    let path = unquote_path(path.trim());
    ensure!(
        !path.is_empty(),
        "Error: line {line_num}: header path is empty"
    );
    ensure!(
        !path.contains('#'),
        "Error: line {line_num}: header path may not contain '#'"
    );
    ensure!(
        tag.len() == 4 && tag.chars().all(|ch| ch.is_ascii_hexdigit()),
        "Error: line {line_num}: snapshot tag must contain exactly four hexadecimal characters"
    );
    Ok(Some(SectionHeader {
        path,
        tag: tag.to_ascii_uppercase(),
    }))
}

/// Lenient projection for callers that only need the path from a header line.
/// Malformed headers are treated as non-headers, matching the historical
/// skip-and-continue behavior of these projections.
pub(crate) fn section_header_path(line: &str) -> Option<String> {
    parse_section_header(line.trim(), 0)
        .ok()
        .flatten()
        .map(|header| header.path)
}

fn try_parse_header(line: &str, line_num: usize) -> Result<Option<Section>> {
    Ok(parse_section_header(line, line_num)?.map(|header| Section {
        path: header.path,
        tag: header.tag,
        operations: Vec::new(),
        warnings: Vec::new(),
    }))
}

fn strip_apply_patch_path_noise(path: &str) -> &str {
    let mut value = path.trim_start_matches('*').trim_start();
    for prefix in [
        "Update File:",
        "Update:",
        "Add File:",
        "Delete File:",
        "Move to:",
    ] {
        if value
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            value = value[prefix.len()..].trim_start();
            break;
        }
    }
    value
}

fn unquote_path(path: &str) -> String {
    if path.len() >= 2 {
        let first = path.as_bytes()[0];
        let last = path.as_bytes()[path.len() - 1];
        if matches!(first, b'\'' | b'"') && first == last {
            return path[1..path.len() - 1].to_string();
        }
    }
    path.to_string()
}

fn is_operation_line(line: &str) -> bool {
    ["PUT", "CUT"]
        .iter()
        .any(|keyword| keyword_rest_loose(line, keyword).is_some())
        || ["REM", "MV"].iter().any(|keyword| {
            keyword_rest(line, keyword).is_some() || line.eq_ignore_ascii_case(keyword)
        })
        || line.starts_with('[')
        || line.starts_with("@@")
        || line.starts_with("***")
        || line.starts_with("diff --git ")
}

fn keyword_rest<'a>(line: &'a str, keyword: &'a str) -> Option<&'a str> {
    let head = line.get(..keyword.len())?;
    (head.eq_ignore_ascii_case(keyword)
        && line
            .as_bytes()
            .get(keyword.len())
            .is_some_and(u8::is_ascii_whitespace))
    .then(|| line[keyword.len()..].trim_start())
}

/// Like `keyword_rest`, but also accepts the missing-space shape models
/// habitually emit (`PUT18.=18:`, `PUT>40:`, `CUT5.=8`). Only locators that
/// start with a digit or `<`/`>`/`:` are normalized, so ordinary words that
/// merely begin with the keyword letters are never misread.
fn keyword_rest_loose<'a>(line: &'a str, keyword: &'a str) -> Option<&'a str> {
    if let Some(rest) = keyword_rest(line, keyword) {
        return Some(rest);
    }
    let head = line.get(..keyword.len())?;
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let b = line.as_bytes().get(keyword.len()).copied()?;
    (b.is_ascii_digit() || b == b'>' || b == b'<' || b == b':').then(|| &line[keyword.len()..])
}

fn parse_operation(
    line: &str,
    line_num: usize,
    section: &mut Section,
    pending: &mut Option<Pending>,
) -> Result<()> {
    if line.starts_with("@@") {
        bail!("Error: line {line_num}: unified-diff hunk header is not valid in hashline");
    }
    if line.starts_with("***") || line.starts_with("diff --git ") {
        bail!("Error: line {line_num}: apply_patch/unified-diff syntax is not valid in hashline");
    }
    if line.eq_ignore_ascii_case("REM") {
        section.operations.push(Operation::Remove);
        return Ok(());
    }
    if let Some(destination) = keyword_rest(line, "MV") {
        ensure!(
            !destination.is_empty(),
            "Error: line {line_num}: MV requires a destination path"
        );
        section.operations.push(Operation::Move {
            destination: unquote_path(destination),
        });
        return Ok(());
    }
    if let Some(rest) = keyword_rest_loose(line, "CUT") {
        if keyword_rest(line, "CUT").is_none() {
            section
                .warnings
                .push("Normalized missing space: `CUT5.` parsed as `CUT 5.`".to_string());
        }
        let (rest, had_colon) = strip_optional_colon(rest);
        if had_colon {
            section
                .warnings
                .push("Ignored trailing `:` on CUT; CUT takes no body rows.".to_string());
        }
        let (locator, register) = split_register(rest, line_num)?;
        let (start, end) = parse_locator_range(locator, line_num)?;
        section.operations.push(Operation::Cut {
            start,
            end,
            register,
        });
        return Ok(());
    }
    if let Some(rest) = keyword_rest_loose(line, "PUT") {
        if keyword_rest(line, "PUT").is_none() {
            section
                .warnings
                .push("Normalized missing space: `PUT2.` parsed as `PUT 2.`".to_string());
        }
        let (rest, had_colon) = strip_optional_colon(rest);
        let (locator, register) = split_register(rest, line_num)?;
        if locator == ">$" {
            return put_gap(
                Cursor::Tail,
                register,
                had_colon,
                line_num,
                section,
                pending,
            );
        }
        if let Some(raw) = locator.strip_prefix('<') {
            let line = parse_line_number(raw.trim(), line_num)?;
            let cursor = if line == 1 {
                Cursor::Head
            } else {
                Cursor::Before(line)
            };
            return put_gap(cursor, register, had_colon, line_num, section, pending);
        }
        if let Some(raw) = locator.strip_prefix('>') {
            let line = parse_line_number(raw.trim(), line_num)?;
            return put_gap(
                Cursor::After(line),
                register,
                had_colon,
                line_num,
                section,
                pending,
            );
        }
        let (start, end) = parse_locator_range(locator, line_num)?;
        if let Some(register) = register {
            ensure!(
                !had_colon,
                "Error: line {line_num}: register PUT takes no body rows"
            );
            section.operations.push(Operation::Paste {
                target: PasteTarget::Range { start, end },
                register: Some(register),
            });
        } else {
            ensure!(
                had_colon,
                "Error: line {line_num}: span PUT without a body must name a register"
            );
            *pending = Some(Pending {
                kind: PendingKind::Put { start, end },
                rows: Vec::new(),
                deferred_blanks: 0,
                line_num,
            });
        }
        return Ok(());
    }
    if line.starts_with('+') {
        bail!(
            "Error: line {line_num}: payload line has no preceding hunk header; CUT/register/file operations take no body rows"
        );
    }
    bail!(
        "Error: line {line_num}: unknown hashline operation {line:?}; expected PUT, CUT, REM, or MV"
    )
}

/// Parse an anchor locator: `'start line text'..'end line text'` (single or
/// double quotes). Anchors are matched trimmed and exact against file lines at
/// apply time, and must be unique. Uses a quote-aware scan so `..` inside a
/// quoted anchor (such as Rust ranges `0..10` or `..Default::default()`) does
/// not split the locator prematurely, and rejects trailing junk after the end
/// quote.
fn parse_quoted_anchor(input: &str, line_num: usize) -> Result<(String, &str)> {
    let input = input.trim_start();
    let Some(quote) = input.chars().next().filter(|c| *c == '\'' || *c == '"') else {
        bail!("Error: line {line_num}: anchor locator must start with ' or \" (got {input:?})");
    };
    let after_open = &input[quote.len_utf8()..];
    let Some((inner, rest)) = after_open.split_once(quote) else {
        bail!("Error: line {line_num}: unterminated anchor locator {input:?}");
    };
    ensure!(
        !inner.trim().is_empty(),
        "Error: line {line_num}: anchor locator must not be empty"
    );
    Ok((inner.to_string(), rest))
}

fn parse_anchor_range(raw: &str, line_num: usize) -> Result<(RangeBound, RangeBound)> {
    let (start_anchor, after_start) = parse_quoted_anchor(raw, line_num)?;
    let after_start = after_start.trim_start();
    let Some(after_sep) = after_start.strip_prefix("..") else {
        bail!("Error: line {line_num}: anchor locator must be 'start'..'end' (got {raw:?})");
    };
    let (end_anchor, trailing) = parse_quoted_anchor(after_sep, line_num)?;
    let trailing = trailing.trim();
    ensure!(
        trailing.is_empty(),
        "Error: line {line_num}: unexpected trailing text {trailing:?} in anchor locator"
    );
    Ok((
        RangeBound::Anchor(start_anchor),
        RangeBound::Anchor(end_anchor),
    ))
}

/// Line-number locator (`40.=60`) or anchor locator (`'start'..'end'`).
fn parse_locator_range(raw: &str, line_num: usize) -> Result<(RangeBound, RangeBound)> {
    if raw.trim_start().starts_with('\'') || raw.trim_start().starts_with('"') {
        parse_anchor_range(raw, line_num)
    } else {
        parse_range(raw, line_num)
    }
}

/// Resolve a range bound against the file's lines (1-based). Anchor text is
/// matched trimmed and exact; zero matches is an error with a re-Read hint,
/// multiple matches is an error asking for a longer, unique line.
fn strip_optional_colon(input: &str) -> (&str, bool) {
    let trimmed = input.trim_end();
    trimmed
        .strip_suffix(':')
        .map_or((trimmed, false), |value| (value.trim_end(), true))
}

fn split_register(input: &str, line_num: usize) -> Result<(&str, Option<String>)> {
    let Some(position) = input.rfind(" @") else {
        return Ok((input.trim(), None));
    };
    let name = input[position + 2..].trim();
    ensure!(
        !name.is_empty()
            && name.len() <= MAX_REGISTER_NAME
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')),
        "Error: line {line_num}: register names use 1-{MAX_REGISTER_NAME} ASCII letters, digits, `_`, or `-`"
    );
    Ok((input[..position].trim(), Some(name.to_string())))
}

fn put_gap(
    cursor: Cursor,
    register: Option<String>,
    had_colon: bool,
    line_num: usize,
    section: &mut Section,
    pending: &mut Option<Pending>,
) -> Result<()> {
    if had_colon {
        ensure!(
            register.is_none(),
            "Error: line {line_num}: register PUT takes no body rows"
        );
        *pending = Some(Pending {
            kind: PendingKind::Insert { cursor },
            rows: Vec::new(),
            deferred_blanks: 0,
            line_num,
        });
    } else {
        section.operations.push(Operation::Paste {
            target: PasteTarget::Gap(cursor),
            register,
        });
    }
    Ok(())
}

fn flush_pending(section: Option<&mut Section>, pending: Pending) -> Result<()> {
    let section =
        section.ok_or_else(|| anyhow!("Error: internal hashline parser lost its section"))?;
    let mut rows = pending.rows;
    let explicit = rows.iter().any(|row| matches!(row, BodyRow::Explicit(_)));
    let had_bare_rows = rows.iter().any(|row| matches!(row, BodyRow::Bare(_)));
    let mut bullet_auto_piped = false;
    let minus = rows
        .iter()
        .filter_map(|row| match row {
            BodyRow::Minus(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !minus.is_empty() {
        let all_bullets = minus.iter().all(|row| markdown_bullet(row));
        let explicit_bullet = rows
            .iter()
            .filter_map(|row| match row {
                BodyRow::Explicit(value) => Some(value),
                _ => None,
            })
            .any(|line| markdown_bullet(line));
        if all_bullets && (!explicit || explicit_bullet) {
            bullet_auto_piped = true;
            section
                .warnings
                .push(
                    "Auto-prefixed bare `- ` bullet row(s) as literal content. `-` rows never remove lines — the range does that; always prefix literal body rows with `+`: `+- item`."
                        .to_string(),
                );
            for row in &mut rows {
                if let BodyRow::Minus(value) = row {
                    *row = BodyRow::Bare(value.clone());
                }
            }
        } else if explicit && !all_bullets {
            rows.retain(|row| !matches!(row, BodyRow::Minus(_)));
            section.warnings.push(
                "Ignored unified-diff `-old` row(s); the range already removes old content, so only `+new` rows were kept."
                    .to_string(),
            );
        } else {
            bail!(
                "Error: line {}: `-` rows are not valid hashline body rows; use `+- item` for Markdown bullets",
                pending.line_num
            );
        }
    }

    let bare_indices = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            matches!(row, BodyRow::Bare(value) if !value.trim().is_empty()).then_some(index)
        })
        .collect::<Vec<_>>();
    if !bare_indices.is_empty() {
        let stripped = bare_indices
            .iter()
            .filter_map(|index| match &rows[*index] {
                BodyRow::Bare(value) => strip_read_prefix(value).map(ToString::to_string),
                _ => None,
            })
            .collect::<Vec<_>>();
        let all_prefixed = stripped.len() == bare_indices.len();
        let all_literal_values =
            all_prefixed && stripped.iter().all(|value| literal_mapping_value(value));
        if all_prefixed && !all_literal_values {
            for (index, value) in bare_indices.into_iter().zip(stripped) {
                rows[index] = BodyRow::Bare(value);
            }
        }
        if had_bare_rows && !bullet_auto_piped {
            section.warnings.push(
                "Auto-prefixed bare body row(s) with `+`. Body rows must be `+TEXT` literal lines."
                    .to_string(),
            );
        }
    }
    let body = rows
        .into_iter()
        .map(|row| match row {
            BodyRow::Explicit(value) | BodyRow::Bare(value) | BodyRow::Minus(value) => value,
            BodyRow::Blank => String::new(),
        })
        .collect::<Vec<_>>();
    match pending.kind {
        PendingKind::Put { start, end } => {
            if body.is_empty() {
                section.warnings.push(
                    "Interpreted an empty `PUT` body as deletion. Use `CUT N.=M` or `CUT N*` for bodyless deletes."
                        .to_string(),
                );
            }
            section.operations.push(Operation::Put { start, end, body })
        }
        PendingKind::Insert { cursor } => {
            ensure!(
                !body.is_empty(),
                "Error: line {}: PUT insert promises body rows",
                pending.line_num
            );
            section.operations.push(Operation::Insert { cursor, body });
        }
    }
    Ok(())
}

fn markdown_bullet(line: &str) -> bool {
    line.trim_start()
        .strip_prefix("- ")
        .is_some_and(|rest| !rest.is_empty())
}

fn strip_read_prefix(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let colon = trimmed.find(':')?;
    (colon > 0 && trimmed[..colon].chars().all(|ch| ch.is_ascii_digit()))
        .then_some(&trimmed[colon + 1..])
}

pub(super) fn literal_mapping_value(value: &str) -> bool {
    let value = value.trim().trim_end_matches(',').trim();
    (value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))))
        || value.parse::<f64>().is_ok()
}

fn parse_range(raw: &str, line_num: usize) -> Result<(RangeBound, RangeBound)> {
    let raw = raw.trim();
    let first_end = raw
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(raw.len());
    let start = parse_line_number(&raw[..first_end], line_num)?;
    let remainder = &raw[first_end..];
    let mut separator_end = 0;
    for (index, ch) in remainder.char_indices() {
        if ch.is_whitespace() || matches!(ch, '-' | '.' | '=' | '…') {
            separator_end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    if remainder.is_empty() {
        return Ok((RangeBound::Line(start), RangeBound::Line(start)));
    }
    ensure!(
        separator_end > 0,
        "Error: line {line_num}: invalid range {raw:?}"
    );
    let end = parse_line_number(remainder[separator_end..].trim(), line_num)?;
    ensure!(
        start <= end,
        "Error: line {line_num}: range start {start} is after end {end}"
    );
    ensure!(
        end - start < MAX_RANGE_LINES,
        "Error: line {line_num}: range expands beyond {MAX_RANGE_LINES} lines"
    );
    Ok((RangeBound::Line(start), RangeBound::Line(end)))
}

fn parse_line_number(raw: &str, line_num: usize) -> Result<usize> {
    let value = raw.parse::<usize>().map_err(|_| {
        anyhow!("Error: line {line_num}: expected a positive line number, got {raw:?}")
    })?;
    ensure!(
        value > 0,
        "Error: line {line_num}: line numbers are one-based"
    );
    Ok(value)
}
