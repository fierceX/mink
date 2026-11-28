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
    file_summary_metrics: Option<(usize, usize)>,
}

struct DispatchTextResult {
    composed: String,
    memo_candidate: Option<MemoCandidate>,
}

fn compose_dispatched_text(
    ctx: &ToolContext,
    call: &ToolCallEvent,
    input: DispatchTextInput,
) -> DispatchTextResult {
    let DispatchTextInput {
        mut output,
        is_bash,
        exit_code,
        wall_ms,
        no_mutation,
        memo_candidate,
        diagnostics,
        success,
        file_summary_metrics,
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
        if path_str.is_empty() {
            Some(kind.to_string())
        } else if let Some((lines, bytes)) = file_summary_metrics {
            Some(format!("{kind}({path_str}) [{lines} lines, {bytes} bytes]"))
        } else {
            let summary_path = resolve_summary_path(&ctx.cwd, path_str);
            Some(file_tool_result_summary_sync(
                kind,
                path_str,
                &summary_path.display().to_string(),
            ))
        }
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
    let mutating = crate::tools::runner::tool_registry()
        .iter()
        .find(|tool| tool.metadata().name == call.name)
        .is_some_and(|tool| tool.metadata().mutating);
    if mutating && success && !no_mutation {
        ctx.bump_mutation();
    }
    DispatchTextResult {
        composed,
        memo_candidate,
    }
}

pub(crate) fn apply_tool_output_filters(
    ctx: &ToolContext,
    filter_ctx: crate::tools::rim_hooks::ToolOutputFilterContext<'_>,
    content: String,
    conv_content: String,
    structured_value: Option<serde_json::Value>,
) -> (
    String,
    String,
    Option<serde_json::Value>,
    bool,
    bool,
    Vec<crate::events::EventLog>,
) {
    let filters = ctx.tool_hooks.snapshot_filters_for(&ctx.scope_path);
    if filters.is_empty() {
        return (
            content,
            conv_content,
            structured_value,
            false,
            false,
            Vec::new(),
        );
    }
    let call_id = filter_ctx.call_id;
    let tool_name = filter_ctx.tool_name;
    let is_ptc_nested = filter_ctx.is_ptc_nested;
    let mut mutation =
        crate::tools::rim_hooks::ToolOutputMutation::new(content, conv_content, structured_value);
    let mut any_mutated = false;
    let mut any_conv_mutated = false;
    let mut effects = Vec::new();
    for filter in filters {
        let prev_content = mutation.content.clone();
        let prev_conv = mutation.conv_content.clone();
        let prev_structured = mutation.structured_value.clone();
        mutation.clear_step_flag();
        match filter.filter(&filter_ctx, &mut mutation) {
            Ok(()) => {
                let content_changed = mutation.content != prev_content;
                let conv_changed = mutation.conv_content != prev_conv;
                let structured_changed = mutation.structured_value != prev_structured;
                let changed =
                    mutation.is_mutated() || content_changed || conv_changed || structured_changed;
                if content_changed && !conv_changed && !mutation.conv_content.is_empty() {
                    // Prevent unredacted `conv_content` from shadowing a
                    // redacted `content` when a filter only rewrites `content`.
                    mutation.conv_content.clear();
                    any_conv_mutated = true;
                }
                if conv_changed {
                    any_conv_mutated = true;
                }
                if changed {
                    any_mutated = true;
                    let code = mutation
                        .code
                        .take()
                        .unwrap_or_else(|| "POLICY_OUTPUT_REDACTED".to_string());
                    let reason = mutation.reason.take();
                    effects.push(crate::events::EventLog::PolicyEffect {
                        version: Some(1),
                        anchor: "after_tool_output".to_string(),
                        policy_id: filter.id().to_string(),
                        call_id: call_id.to_string(),
                        tool_name: tool_name.to_string(),
                        action: "redact".to_string(),
                        code,
                        reason,
                        is_sub_agent: ctx.is_sub_agent,
                        is_ptc_nested,
                    });
                }
            }
            Err(error) => {
                any_mutated = true;
                any_conv_mutated = true;
                mutation.content = format!(
                    "Error: tool output blocked by policy filter '{}' (fail-closed): {error:#}",
                    filter.id()
                );
                mutation.conv_content.clear();
                mutation.structured_value = None;
                effects.push(crate::events::EventLog::PolicyEffect {
                    version: Some(1),
                    anchor: "after_tool_output".to_string(),
                    policy_id: filter.id().to_string(),
                    call_id: call_id.to_string(),
                    tool_name: tool_name.to_string(),
                    action: "error_fail_closed".to_string(),
                    code: "POLICY_FILTER_ERROR".to_string(),
                    reason: Some(format!("{error:#}")),
                    is_sub_agent: ctx.is_sub_agent,
                    is_ptc_nested,
                });
                break;
            }
        }
    }
    (
        mutation.content,
        mutation.conv_content,
        mutation.structured_value,
        any_mutated,
        any_conv_mutated,
        effects,
    )
}

fn invalidate_read_snapshot_if_needed(
    ctx: &ToolContext,
    call: &ToolCallEvent,
    memo_path: Option<&Path>,
) {
    if call.name != "Read" {
        return;
    }
    let resolved = if let Some(path) = memo_path {
        Some(path.to_path_buf())
    } else if let Some(raw) = call.fields.get("path") {
        let base = crate::resources::selector::split_read_path_selection(raw)
            .map(|sel| sel.path)
            .unwrap_or_else(|_| raw.clone());
        (!base.contains("://")).then(|| resolve_summary_path(&ctx.cwd, &base))
    } else {
        None
    };
    if let Some(path) = resolved {
        ctx.snapshots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate(&path);
    }
}

pub(super) fn format_dispatched_result(
    ctx: &ToolContext,
    call: &ToolCallEvent,
    raw: ToolExecOutput,
) -> ToolExecution {
    let ToolExecOutput {
        content: output,
        is_bash,
        conv_content,
        exit_code,
        wall_ms,
        no_mutation,
        memo_candidate,
        spawns_sub_agent,
        status,
        diagnostics,
        plan_command,
        mut state_metadata,
        result_kind,
        presentation,
        structured_value: _,
        failure_source,
        file_summary_metrics,
    } = raw;
    let success = status.is_success();
    let DispatchTextResult {
        composed,
        memo_candidate,
    } = compose_dispatched_text(
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
            file_summary_metrics,
        },
    );
    let composed = if is_bash {
        strip_bash_ansi(&composed)
    } else {
        composed
    };
    let needs_finalization = plan_command.is_some() || spawns_sub_agent;
    let (filtered_composed, mut conv_content, conv_mutated, policy_effects) = if needs_finalization
    {
        // Deferred tools (`Plan*` / `SubAgent`) are filtered once in
        // `finalize_deferred_results` after state/sub-agent resolution.
        (composed, conv_content, false, Vec::new())
    } else {
        let (filtered_composed, conv_content, _, output_mutated, conv_mutated, policy_effects) =
            apply_tool_output_filters(
                ctx,
                crate::tools::rim_hooks::ToolOutputFilterContext {
                    session_id: &ctx.session_id,
                    call_id: &call.id,
                    tool_name: &call.name,
                    is_sub_agent: ctx.is_sub_agent,
                    is_ptc_nested: false,
                    succeeded: success,
                },
                composed,
                conv_content,
                None,
            );
        if output_mutated && call.name == "Read" {
            invalidate_read_snapshot_if_needed(
                ctx,
                call,
                memo_candidate.as_ref().map(|c| c.path.as_path()),
            );
        } else if let Some(candidate) = memo_candidate
            && filtered_composed.len() <= ctx.tool_config.tool_result_max_bytes
        {
            ctx.memo_record(
                &candidate.path,
                candidate.raw,
                candidate.start_line,
                candidate.end_line,
                candidate.content_hash,
            );
        }
        for effect in &policy_effects {
            crate::tools::runner::merge_policy_effect_metadata(&mut state_metadata, effect);
        }
        (
            filtered_composed,
            conv_content,
            conv_mutated,
            policy_effects,
        )
    };
    let formatted = if needs_finalization {
        FormattedToolOutput {
            content: filtered_composed,
            artifacts: Vec::new(),
        }
    } else {
        format_tool_result_with_artifact(
            &call.name,
            &filtered_composed,
            ctx.tool_config.tool_result_max_bytes,
            ctx,
        )
    };

    let final_output = if is_bash {
        compress_repeated_lines(&formatted.content)
    } else {
        formatted.content
    };

    // Edit diagnostics are continuation state, not a terminal-only summary:
    // the next model request needs new tags, changed lines, warnings and diffs.
    // Use the already size-protected/artifact-spilled output for both success
    // and failure so conversation and UI cannot diverge or bypass the limit,
    // unless an output filter explicitly customized `conv_content`.
    if call.name == "Edit" && !conv_mutated {
        conv_content = final_output.clone();
    } else if !conv_content.is_empty() && conv_content.len() > ctx.tool_config.tool_result_max_bytes
    {
        conv_content = format_tool_result(&conv_content, ctx.tool_config.tool_result_max_bytes);
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
        policy_effects,
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
        file_summary_metrics,
    } = raw;
    let success = status.is_success();
    let DispatchTextResult {
        composed,
        memo_candidate,
    } = compose_dispatched_text(
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
            file_summary_metrics,
        },
    );
    let composed = if is_bash {
        strip_bash_ansi(&composed)
    } else {
        composed
    };
    let (filtered_composed, _, structured_value, output_mutated, _, policy_effects) =
        apply_tool_output_filters(
            ctx,
            crate::tools::rim_hooks::ToolOutputFilterContext {
                session_id: &ctx.session_id,
                call_id: &call.id,
                tool_name: &call.name,
                is_sub_agent: ctx.is_sub_agent,
                is_ptc_nested: true,
                succeeded: success,
            },
            composed,
            String::new(),
            structured_value,
        );
    if output_mutated && call.name == "Read" {
        invalidate_read_snapshot_if_needed(
            ctx,
            call,
            memo_candidate.as_ref().map(|c| c.path.as_path()),
        );
    } else if let Some(candidate) = memo_candidate
        && filtered_composed.len() <= ctx.tool_config.tool_result_max_bytes
    {
        ctx.memo_record(
            &candidate.path,
            candidate.raw,
            candidate.start_line,
            candidate.end_line,
            candidate.content_hash,
        );
    }
    let rendered_text = if is_bash {
        compress_repeated_lines(&filtered_composed)
    } else {
        filtered_composed
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
        policy_effects,
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
        let newlines = s.iter().filter(|&&c| c == b'\n').count();
        if s.ends_with(b"\n") {
            newlines
        } else {
            newlines + 1
        }
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

pub(super) fn strip_bash_ansi(s: &str) -> String {
    RE_ANSI_ESCAPE.replace_all(s, "").into_owned()
}

pub(super) fn compress_repeated_lines(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
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

#[cfg(test)]
pub(super) fn filter_bash_noise(s: &str) -> String {
    let no_ansi = strip_bash_ansi(s);
    compress_repeated_lines(&no_ansi)
}

#[cfg(test)]
mod tests {
    use super::line_count;

    #[test]
    fn line_count_does_not_overcount_trailing_newline() {
        assert_eq!(line_count(b""), 0);
        assert_eq!(line_count(b"one"), 1);
        assert_eq!(line_count(b"one\n"), 1);
        assert_eq!(line_count(b"one\ntwo"), 2);
        assert_eq!(line_count(b"one\ntwo\n"), 2);
    }
}
