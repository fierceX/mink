use super::batch::{json_validity_note, resolve_summary_path};
use super::*;

struct DispatchTextInput {
    output: String,
    is_bash: bool,
    exit_code: Option<i32>,
    wall_ms: Option<u128>,
    no_mutation: bool,
    memo_candidate: Option<MemoCandidate>,
    diagnostics: Vec<String>,
    success: bool,
}

fn compose_dispatched_text(
    ctx: &ToolContext,
    call: &ToolCallEvent,
    input: DispatchTextInput,
) -> String {
    let DispatchTextInput {
        mut output,
        is_bash,
        exit_code,
        wall_ms,
        no_mutation,
        memo_candidate,
        diagnostics,
        success,
    } = input;
    if !success && exit_code.is_none() {
        output = format!("Error: {output}");
    }
    if !diagnostics.is_empty() {
        output.push_str("\nDiagnostics:");
        for diagnostic in diagnostics {
            output.push('\n');
            output.push_str(&diagnostic);
        }
    }
    if let Some(code) = exit_code {
        let mut header = format!("Exit code: {code}\n");
        if let Some(ms) = wall_ms {
            header.push_str(&format!("Wall time: {:.1}s\n", ms as f64 / 1000.0));
        }
        output = format!("{header}{output}");
    }
    let summary = if (call.name == "Read" || call.name == "Write") && !is_bash {
        let path_str = call.fields.get("path").map(|s| s.as_str()).unwrap_or("");
        let kind = call.name.as_str();
        let summary_path = resolve_summary_path(&ctx.cwd, path_str);
        Some(file_tool_result_summary_sync(
            kind,
            path_str,
            &summary_path.display().to_string(),
        ))
    } else {
        None
    };
    let json_note = if success && (call.name == "Edit" || call.name == "Write") {
        json_validity_note(ctx, call)
    } else {
        None
    };
    let mut composed = output;
    if let Some(summary) = summary {
        composed = format!("{summary}\n{composed}");
    }
    if let Some(note) = json_note {
        composed.push_str(&note);
    }
    if let Some(candidate) = memo_candidate
        && composed.len() <= ctx.tool_config.tool_result_max_bytes
    {
        ctx.memo_record(
            &candidate.path,
            candidate.raw,
            candidate.start_line,
            candidate.end_line,
        );
    }
    let mutating = crate::tools::runner::tool_registry()
        .iter()
        .find(|tool| tool.metadata().name == call.name)
        .is_some_and(|tool| tool.metadata().mutating);
    if mutating && success && !no_mutation {
        ctx.bump_mutation();
    }
    composed
}

pub(super) fn format_dispatched_result(
    ctx: &ToolContext,
    call: &ToolCallEvent,
    raw: ToolExecOutput,
) -> ToolExecution {
    let ToolExecOutput {
        content: output,
        is_bash,
        mut conv_content,
        exit_code,
        wall_ms,
        no_mutation,
        memo_candidate,
        spawns_sub_agent,
        status,
        diagnostics,
        plan_command,
        state_metadata,
        result_kind,
        presentation,
        structured_value: _,
        failure_source,
    } = raw;
    let success = status.is_success();
    let composed = compose_dispatched_text(
        ctx,
        call,
        DispatchTextInput {
            output,
            is_bash,
            exit_code,
            wall_ms,
            no_mutation,
            memo_candidate,
            diagnostics,
            success,
        },
    );
    let needs_finalization = plan_command.is_some() || spawns_sub_agent;
    let formatted = if needs_finalization {
        FormattedToolOutput {
            content: composed,
            artifacts: Vec::new(),
        }
    } else {
        format_tool_result_with_artifact(
            &call.name,
            &composed,
            ctx.tool_config.tool_result_max_bytes,
            ctx,
        )
    };

    let final_output = if is_bash {
        filter_bash_noise(&formatted.content)
    } else {
        formatted.content
    };

    // Edit diagnostics are continuation state, not a terminal-only summary:
    // the next model request needs new tags, changed lines, warnings and diffs.
    // Use the already size-protected/artifact-spilled output for both success
    // and failure so conversation and UI cannot diverge or bypass the limit.
    if call.name == "Edit" {
        conv_content = final_output.clone();
    }

    // Signals collected later by TurnExecutor (needs shared SignalCollector for EditLoop)
    let signals = Vec::new();

    let sub_agent_prompt = if spawns_sub_agent && success {
        call.fields.get("prompt").cloned()
    } else {
        None
    };
    let sub_agent_fork = spawns_sub_agent
        && success
        && call
            .fields
            .get("fork")
            .map(|s| s == "true" || s == "1")
            .unwrap_or(false);
    let sub_agent_label = if spawns_sub_agent && success {
        call.fields.get("description").cloned()
    } else {
        None
    };
    let sub_agent_model = if spawns_sub_agent && success {
        call.fields.get("model").cloned()
    } else {
        None
    };

    ToolExecution {
        tool_use_id: call.id.clone(),
        tool_name: call.name.clone(),
        tool_args: call.fields.clone(),
        content: final_output,
        conv_content,
        spawns_sub_agent,
        sub_agent_prompt,
        sub_agent_fork,
        sub_agent_label,
        sub_agent_model,
        exit_code,
        status,
        result_kind,
        presentation,
        artifacts: formatted.artifacts,
        signals,
        failure_source,

        plan_command,
        needs_finalization,
        state_metadata,
        image_attachment: None,
        extra_image_attachments: Vec::new(),
        todo_advanced: false,
        edited_paths: Vec::new(),
    }
}

pub(super) fn format_nested_dispatched_result(
    ctx: &ToolContext,
    call: &ToolCallEvent,
    raw: ToolExecOutput,
    is_mutating: bool,
) -> NestedToolOutcome {
    let ToolExecOutput {
        content: output,
        is_bash,
        conv_content: _,
        exit_code,
        wall_ms,
        no_mutation,
        memo_candidate,
        spawns_sub_agent: _,
        status,
        diagnostics,
        plan_command: _,
        state_metadata: _,
        result_kind,
        presentation: _,
        structured_value,
        failure_source: _,
    } = raw;
    let success = status.is_success();
    let composed = compose_dispatched_text(
        ctx,
        call,
        DispatchTextInput {
            output,
            is_bash,
            exit_code,
            wall_ms,
            no_mutation,
            memo_candidate,
            diagnostics,
            success,
        },
    );
    let rendered_text = if is_bash {
        filter_bash_noise(&composed)
    } else {
        composed
    };
    let unknown_effect = is_mutating
        && matches!(
            status,
            ToolStatus::Interrupted
                | ToolStatus::Failed(
                    ToolFailureKind::Timeout | ToolFailureKind::Aborted | ToolFailureKind::Unknown
                )
        );
    let todo_advanced = call.name == "TodoAdvance" && success;
    let edited_paths = if success && !no_mutation {
        crate::tools::runner::extract_edited_paths(&call.name, &call.fields)
    } else {
        Vec::new()
    };
    NestedToolOutcome {
        rendered_text,
        structured_value,
        status,
        exit_code,
        result_kind,
        unknown_effect,
        todo_advanced,
        edited_paths,
        image: None,
    }
}

fn file_tool_result_summary_sync(kind: &str, display_path: &str, read_path: &str) -> String {
    if display_path.is_empty() {
        return kind.to_string();
    }
    // Use metadata to check file size without reading content.
    let meta = match std::fs::metadata(read_path) {
        Ok(m) => m,
        Err(_) => return format!("{}({})", kind, display_path),
    };
    // For large files, show size only — avoids re-reading the entire file just for a summary.
    if meta.len() > 1_048_576 {
        return format!("{}({}) [{} bytes]", kind, display_path, meta.len());
    }
    match std::fs::read(read_path) {
        Ok(data) => format!(
            "{}({}) [{} lines, {} bytes]",
            kind,
            display_path,
            line_count(&data),
            data.len()
        ),
        Err(_) => format!("{}({})", kind, display_path),
    }
}

fn line_count(s: &[u8]) -> usize {
    if s.is_empty() {
        0
    } else {
        s.iter().filter(|&&c| c == b'\n').count() + 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncationPolicy {
    Bytes(usize),
    #[cfg(test)]
    Tokens(usize),
}

impl TruncationPolicy {
    /// Approximate byte budget. Tokens use a 4-bytes-per-token estimate so the
    /// policy composes with the rest of mink's explicit token budgets.
    fn byte_budget(&self) -> usize {
        match *self {
            Self::Bytes(bytes) => bytes,
            #[cfg(test)]
            Self::Tokens(tokens) => tokens.saturating_mul(4),
        }
    }
}

/// Approximate token count (bytes / 4). Used only for truncation markers.
pub fn approx_token_count(s: &str) -> usize {
    s.len().div_ceil(4)
}

pub fn format_tool_result(s: &str, max: usize) -> String {
    format_tool_result_policy(s, TruncationPolicy::Bytes(max))
}

pub fn format_tool_result_policy(s: &str, policy: TruncationPolicy) -> String {
    let budget = policy.byte_budget();
    if s.len() <= budget {
        return s.to_string();
    }
    let size = s.len();
    let tokens = approx_token_count(s);
    let marker = format!(
        "\n\n[... truncated: original token count: {tokens} ({} bytes); showing first/last portions ...]\n\n",
        size
    );
    // The tail is byte-budgeted as well: a few short lines or a char-safe
    // suffix of a single over-long line, never the whole original output.
    let tail_text = tail_within_budget(s, budget / 4);
    let marker_len = marker.len() + 20;
    let tail_len = tail_text.len();
    let head_len = budget.saturating_sub(marker_len + tail_len);
    let mut head_text = utf8_prefix_by_bytes(s, head_len);
    // Cut at a complete line boundary so the model never sees a half line.
    if let Some(boundary) = head_text.rfind('\n') {
        head_text = &head_text[..boundary + 1];
    }
    format!("{head_text}{marker}{tail_text}")
}

/// Last whole lines of `s` within `max_bytes`; a single over-long line
/// degrades to a char-boundary byte suffix so the tail never exceeds the
/// budget and never duplicates the whole output.
fn tail_within_budget(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len();
    let mut used = 0usize;
    for (idx, ch) in s.char_indices().rev() {
        if ch == '\n' {
            let segment_len = start - (idx + 1);
            if used + segment_len > max_bytes {
                break;
            }
            used += segment_len;
            start = idx + 1;
        }
    }
    // Nothing fit (single line longer than the budget, or no newline):
    // fall back to a char-boundary byte suffix of the output.
    if s.len() - start > max_bytes || start == s.len() {
        let mut cut = s.len();
        let mut len = 0usize;
        for (idx, ch) in s.char_indices().rev() {
            if len + ch.len_utf8() > max_bytes {
                break;
            }
            len += ch.len_utf8();
            cut = idx;
        }
        start = cut;
    }
    &s[start..]
}

pub(crate) struct FormattedToolOutput {
    pub(crate) content: String,
    pub(crate) artifacts: Vec<ArtifactDisplay>,
}

pub(crate) fn format_tool_result_with_artifact(
    tool_name: &str,
    output: &str,
    max: usize,
    ctx: &ToolContext,
) -> FormattedToolOutput {
    if output.len() <= max {
        return FormattedToolOutput {
            content: output.to_string(),
            artifacts: Vec::new(),
        };
    }
    // The marker is part of the unified size protection: reserve its bytes
    // before truncating the body so content never exceeds `max`.
    let body_and_suffix = |suffix: String| {
        if suffix.len() + 1 >= max {
            return format_tool_result(output, max);
        }
        let body = format_tool_result(output, max - suffix.len());
        format!("{body}{suffix}")
    };
    match ctx
        .artifacts
        .write_text(tool_name, "full tool output", output)
    {
        Ok(record) => FormattedToolOutput {
            content: body_and_suffix(format!("\n\n[Full output: artifact://{}]", record.id)),
            artifacts: vec![ArtifactDisplay {
                id: record.id,
                tool: record.tool,
                bytes: record.bytes,
                description: record.description,
            }],
        },
        Err(error) => FormattedToolOutput {
            // A failed spill must be visible to the model/UI: silent
            // truncation hides that the full output is not retrievable.
            content: body_and_suffix(format!(
                "\n\n[Output truncated; artifact storage failed: {error}]"
            )),
            artifacts: Vec::new(),
        },
    }
}

#[test]
fn format_tool_result_policy_token_mode_and_line_boundaries() {
    let s = "line0\n".repeat(500);
    let by_tokens = format_tool_result_policy(&s, TruncationPolicy::Tokens(25));
    assert!(by_tokens.len() <= 25 * 4 + 100);
    assert!(by_tokens.contains("original token count"));
    let head = by_tokens.split("[... truncated").next().unwrap();
    assert!(
        head.is_empty() || head.ends_with("\n\n"),
        "head must end at a line boundary: {head:?}"
    );
    let by_bytes = format_tool_result(&s, 100);
    assert!(by_bytes.contains("original token count"));
}

#[test]
fn truncation_single_overlong_line_stays_within_budget() {
    // 外部审查 #5：少于 5 个换行的输出此前会把整个原文当作 tail，
    // 单超长行导致结果超过预算。现在 head/tail 都受字节预算约束。
    let single_line = format!("{}\n", "x".repeat(10_000));
    let result = format_tool_result(&single_line, 100);
    assert!(
        result.len() <= 100 + 100,
        "truncated output {} bytes exceeds budget",
        result.len()
    );
    let no_newline = "y".repeat(8_000);
    let result = format_tool_result(&no_newline, 100);
    assert!(result.len() <= 100 + 100, "{} bytes", result.len());
}
fn utf8_prefix_by_bytes(s: &str, max_bytes: usize) -> &str {
    if max_bytes >= s.len() {
        return s;
    }
    let mut end = 0;
    for (i, ch) in s.char_indices() {
        let next = i + ch.len_utf8();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    &s[..end]
}

static RE_ANSI_ESCAPE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    // CSI (including private parameter bytes like `\x1b[?25l`), OSC strings
    // terminated by BEL or ST, and single-character escapes.
    regex::Regex::new(
        "\\x1b\\[[0-9;?]*[ -/]*[@-~]|\\x1b\\][^\\x07\\x1b]*(?:\\x07|\\x1b\\\\)|\\x1b[@-Z\\\\-_]",
    )
    .expect("valid ANSI regex")
});

pub(super) fn filter_bash_noise(s: &str) -> String {
    // Strip ANSI escape sequences
    let no_ansi = RE_ANSI_ESCAPE.replace_all(s, "");

    // Compress consecutive identical lines
    let lines: Vec<&str> = no_ansi.lines().collect();
    let mut out = Vec::new();
    let mut repeat_count: usize = 0;
    for i in 0..lines.len() {
        if i > 0 && lines[i] == lines[i - 1] {
            repeat_count += 1;
        } else {
            if repeat_count > 0 {
                out.push(format!("  [previous line repeated {} times]", repeat_count));
                repeat_count = 0;
            }
            out.push(lines[i].to_string());
        }
    }
    if repeat_count > 0 {
        out.push(format!("  [previous line repeated {} times]", repeat_count));
    }
    out.join("\n")
}
