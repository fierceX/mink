#!/usr/bin/env python3
"""Build a sanitized excerpt with status reconstructed from recorded events.

Completed usage is correlated with accepted replies by tool IDs or exact text.
Missing observations remain unknown; the export is not a frame recording.
"""
import argparse
import html
import json
import re
from pathlib import Path


def load_messages(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def shorten(text, limit=160):
    text = re.sub(r"\s+", " ", text).strip()
    return text if len(text) <= limit else text[:limit - 1].rstrip() + "…"


def sanitize_text(text, repo_name, recording_root=None):
    # Refuse credentials rather than silently publishing a partly redacted key.
    if re.search(r"(?:sk-[\w-]{16,}|Bearer\s+[\w.-]{16,}|api_key[\"']?\s*[=:]\s*[\"']?[^\s\"']{12,})", text, re.I):
        raise ValueError("Replay source contains credential-like content")
    if recording_root:
        text = text.replace(str(recording_root), f"/workspace/{repo_name}")
    text = text.replace(str(Path.cwd()), f"/workspace/{repo_name}")
    text = text.replace(str(Path.home()), "/home/user")
    text = re.sub(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))", "", text)
    return re.sub(r"[\x00-\x08\x0b-\x1f\x7f]", "", text).strip()


def tool_summary(name, payload):
    if name == "Read": return payload.get("path", "")
    if name == "Bash": return shorten(payload.get("command", ""), 100)
    if name == "Grep": return f"{payload.get('pattern', '')} · {payload.get('path', '.')}"
    if name == "Glob": return payload.get("pattern", "*")
    if name == "Edit":
        edits = payload.get("edits", [])
        return ", ".join(dict.fromkeys(str(edit.get("path", "")) for edit in edits)) or payload.get("path", "")
    if name == "Write": return payload.get("path", "")
    return ""


def tool_label(name):
    color = {"Read": "read-tool", "Edit": "edit-tool", "Write": "edit-tool", "Bash": "command-tool", "Python": "command-tool"}.get(name, "info")
    return f'<span class="{color}">{html.escape(name)}</span>'


def result_excerpt(text):
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    # Test output deserves its actual summary, including a red-test run.
    test_lines = [line for line in lines if line.startswith("test result:") or re.match(r"^test [\w:]+ \.\.\. (ok|FAILED)$", line)]
    if len([line for line in test_lines if line.startswith("test result:")]) > 1:
        test_lines = [line for line in test_lines if not line.startswith("test result: ok. 0 passed; 0 failed;")]
    if test_lines: return "\n".join(test_lines[-8:])
    return "\n".join(lines[:2])


def build_steps(messages, repo_name, recording_root=None):
    steps, calls = [], {}
    turn_id = None
    def clean(value): return sanitize_text(value, repo_name, recording_root)
    def add(index, kind, **content):
        steps.append({"sourceMessage": index, "kind": kind, **content})
    for index, message in enumerate(messages):
        if message.get("internal"): continue
        role = message.get("role")
        blocks = message.get("content", [])
        if isinstance(blocks, str): blocks = [{"type": "text", "text": blocks}]
        for block in blocks:
            kind = block.get("type")
            if role == "user" and kind == "text":
                text = clean(block.get("text", ""))
                if not text: continue
                metadata = message.get("_mink") or {}
                if metadata.get("guidance"):
                    if not turn_id or metadata.get("turn_id") != turn_id:
                        raise ValueError("Guidance is not bound to the excerpt's active turn")
                    # Input animation reconstructs the recorded human input, not a
                    # separately timed PTY frame; the committed row follows it.
                    add(index, "input", mode="typed", text=text, input="", delay=650)
                    add(index, "guidance", mode="instant", line='<span class="info">&gt; [Added to context]</span> ' + html.escape(text), delay=1400)
                else:
                    turn_id = metadata.get("turn_id")
                    add(index, "prompt", mode="instant", line='<span class="prompt">&gt;</span> ' + html.escape(shorten(text, 140)))
            elif role == "assistant" and kind in ("thinking", "text"):
                text = clean(block.get("thinking" if kind == "thinking" else "text", ""))
                if not text: continue
                prefix = '<span class="dim">► thinking</span> | ' if kind == "thinking" else '<span class="prompt">&gt;</span> '
                add(index, kind, mode="typed", prefixHtml=prefix, text=shorten(text, 160))
            elif role == "assistant" and kind == "tool_use":
                name = block.get("name", "Tool")
                summary = clean(tool_summary(name, block.get("input", {})))
                calls[block.get("id")] = (name, summary)
                add(index, "tool", mode="instant", toolUseId=block.get("id"), line='<span class="dim">▶ ◇</span> ' + tool_label(name) + ' ' + html.escape(summary))
            elif kind == "tool_result":
                metadata = block.get("_mink") or {}
                call_name, summary = calls.get(block.get("tool_use_id"), ("Tool", ""))
                name = metadata.get("tool_name") or call_name
                recorded_status = metadata.get("status")
                status = recorded_status.get("state") if isinstance(recorded_status, dict) else None
                marker, color = {"succeeded": ("✓", "ok"), "failed": ("✗ failed", "danger"), "blocked": ("⊘ blocked", "warn"), "interrupted": ("■ interrupted", "warn")}.get(status, ("? 状态未记录", "dim"))
                body = html.escape(clean(result_excerpt(block.get("content", "")))).replace("\n", "<br>")
                if "test result: FAILED" in body:
                    body = '<span class="danger">' + body + '</span>'
                add(index, "tool_result", mode="instant", toolUseId=block.get("tool_use_id"), toolStatus=status, line=f'<span class="dim">▼</span> <span class="{color}">{marker}</span> {tool_label(name)} {html.escape(summary)}<br><span class="dim">{body}</span>')
    return steps


def fmt_k(value):
    if value >= 1_000_000: return f"{value / 1_000_000:.1f}m"
    if value >= 1_000: return f"{value / 1_000:.1f}k"
    return str(value)


STAT_FIELDS = ("current_turn_count", "agent_request_count", "total_input_tokens", "total_cache_read_tokens", "total_cache_creation_tokens", "total_output_tokens")


def status_fields(stats):
    stats = stats or {}
    def number(key):
        return f"{stats[key]:,}" if key in stats else "—"
    complete = all(key in stats for key in STAT_FIELDS[2:5])
    total = sum(stats[key] for key in STAT_FIELDS[2:5]) if complete else None
    cache = f"{stats['total_cache_read_tokens'] * 100 // total}%" if total else "—"
    context, maximum = stats.get("current_context_tokens"), stats.get("max_context_tokens")
    pct = f"{context * 100 // maximum}%" if context is not None and maximum else "—"
    belief = stats.get("belief")
    return [
        {"id": "belief", "text": f"B:{belief:.2f}" if belief is not None else "B:—", "priority": 7},
        {"id": "turns", "text": f"T:{number('current_turn_count')}", "priority": 8},
        {"id": "requests", "text": f"R:{number('agent_request_count')}", "priority": 8},
        {"id": "input", "text": f"I:{fmt_k(total) if total is not None else '—'}({cache})", "priority": 5},
        {"id": "output", "text": f"O:{fmt_k(stats['total_output_tokens']) if 'total_output_tokens' in stats else '—'}", "priority": 5},
        {"id": "context", "text": f"C:{fmt_k(context) if context is not None else '—'}({pct})", "priority": 2},
    ]


def recorded_status(messages, events, final_stats):
    """Reconstruct settled counters, validating against the persisted totals."""
    current = dict.fromkeys(STAT_FIELDS, 0)
    requests, by_call, text = [], {}, ""
    source, initial = None, None
    pending = None
    for index, event in enumerate(events):
        kind = event.get("type")
        if kind == "turn_start":
            current["current_turn_count"] += 1
            if current["current_turn_count"] != 1:
                raise ValueError("Replay status requires a single isolated turn")
            current["belief"] = event.get("belief")
            source = index
            if initial is None: initial = (dict(current), source)
        elif kind == "text":
            text += event.get("content", "")
        elif kind == "usage":
            if event.get("kind") != "agent":
                raise ValueError("Replay status requires an agent-only recorded case")
            before = (dict(current), source)
            current["agent_request_count"] += 1
            for field, event_field in [("total_input_tokens", "input_tokens"), ("total_cache_read_tokens", "cache_read_input_tokens"), ("total_cache_creation_tokens", "cache_creation_input_tokens"), ("total_output_tokens", "output_tokens")]:
                if event_field not in event: raise ValueError("Incomplete usage observation")
                current[field] += event[event_field]
            for field, event_field in [("current_context_tokens", "context_tokens"), ("max_context_tokens", "max_context")]:
                if event.get(event_field) is not None: current[field] = event[event_field]
            source = index
            pending = {"before": before, "after": (dict(current), source), "text": text}
            requests.append(pending)
            text = ""
        elif kind == "tool_call":
            if pending is None: raise ValueError("Tool call without recorded usage")
            by_call[event["id"]] = pending
        elif kind == "tool_result":
            # Tool observations can change belief without persisting a value.
            current["belief"] = None
        elif kind == "turn_tracking":
            current["belief"] = event.get("belief")
            source = index
    if initial is None or not final_stats or any(current[key] != final_stats.get(key) for key in STAT_FIELDS):
        raise ValueError("Recorded usage does not reconcile with final stats")
    final = ({**current, **final_stats}, source)
    responses = {}
    for index, message in enumerate(messages):
        if message.get("role") != "assistant" or message.get("internal"): continue
        blocks = message.get("content", [])
        ids = [block["id"] for block in blocks if block.get("type") == "tool_use"]
        if ids:
            candidates = [by_call.get(call_id) for call_id in ids]
            if not candidates[0] or any(candidate is not candidates[0] for candidate in candidates):
                raise ValueError("Accepted tool batch does not match recorded events")
            responses[index] = candidates[0]
        else:
            body = "".join(block.get("text", "") for block in blocks if block.get("type") == "text")
            candidates = [request for request in requests if body and request["text"] == body]
            if len(candidates) != 1: raise ValueError("Accepted text does not uniquely match recorded events")
            responses[index] = candidates[0]
    return initial, responses, final


def enrich_steps(steps, session_label, stats=None, observations=None):
    result = []
    model, separator, cwd = session_label.partition(" @")
    base = [{"id": "model", "text": model, "priority": 1}]
    if separator: base.append({"id": "cwd", "text": f"@{cwd}", "priority": 4})
    states = {"prompt": "waiting", "thinking": "thinking", "text": "generating", "tool": "tool", "tool_result": "waiting", "input": "waiting", "guidance": "waiting"}
    current, source = observations[0] if observations else ({}, None)
    for step in steps:
        response = observations[1].get(step.get("sourceMessage")) if observations else None
        if response:
            current, source = response["after" if step["kind"] == "tool" else "before"]
        if step["kind"] == "tool_result": current = {**current, "belief": None}
        before = [*base, *status_fields(current)]
        frame = {"statusItems": before, "statusSourceEvent": source, "workState": states[step['kind']], "input": "", **step}
        result.append(frame)
    if result and result[-1]["kind"] == "text":
        result[-1]["workState"] = "idle"
        result[-1]["statusBefore"] = result[-1]["statusItems"]
        final, source = observations[2] if observations else (stats, None)
        result[-1]["statusItems"] = [*base, *status_fields(final)]
        result[-1]["statusSourceEvent"] = source
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    parser.add_argument("-o", "--output", type=Path, default=Path("docs/assets/hero-replay.json"))
    parser.add_argument("--repo-name", default="mink")
    parser.add_argument("--session-label", default="mink")
    parser.add_argument("--recording-root", type=Path)
    parser.add_argument("--stats", type=Path)
    parser.add_argument("--events", type=Path, help="Recorded events.jsonl; requires matching --stats")
    parser.add_argument("--case-title", default="Recorded session")
    parser.add_argument("--case-doc")
    parser.add_argument("--recorded-date")
    parser.add_argument("--mink-version")
    args = parser.parse_args()
    messages = load_messages(args.input)
    stats = json.loads(args.stats.read_text()) if args.stats else None
    observations = recorded_status(messages, load_messages(args.events), stats) if args.events else None
    steps = enrich_steps(build_steps(messages, args.repo_name, args.recording_root), args.session_label, stats, observations)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    def user_text(row):
        content = row.get("content", [])
        return content if isinstance(content, str) else " ".join(b.get("text", "") for b in content if b.get("type") == "text")

    prompts = [row for row in messages if row.get("role") == "user" and not row.get("internal") and user_text(row).strip()]
    guidance = [row for row in prompts if row.get("_mink", {}).get("guidance")]
    final_text = next((b.get("text", "") for row in reversed(messages) if row.get("role") == "assistant" for b in row.get("content", []) if b.get("type") == "text" and b.get("text", "").strip()), "")
    def clean(text):
        return sanitize_text(text, args.repo_name, args.recording_root)

    meta = {
        "sessionLabel": args.session_label,
        "maxVisibleRows": 18,
        "source": "Recorded Mink session; text excerpts, input animation and accelerated playback",
        "statsNote": "Settled usage reconstructed from recorded events; unrecorded values remain unknown; final totals reconciled with stats.json" if args.events else "Only final stats recorded; intermediate fields remain unknown",
        "caseTitle": args.case_title,
        "caseDoc": args.case_doc,
        "recordedDate": args.recorded_date,
        "version": args.mink_version,
        "guidanceCount": len(guidance),
        "task": clean(user_text(prompts[0])) if prompts else "",
        "guidance": clean(user_text(guidance[0])) if guidance else "",
        "result": clean(final_text.split("\n\n")[0].replace("`", "")),
    }
    args.output.write_text(json.dumps({"meta": meta, "steps": steps}, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
