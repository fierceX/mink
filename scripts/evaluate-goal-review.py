#!/usr/bin/env python3
"""Run the real-provider audit evaluation without exposing local credentials.

Writes sanitized results to target/bench by default; raw runtime sessions stay
under the requested output directory and must not be published.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tomllib
from datetime import datetime, timezone


def enrich_results(result, output):
    """Count local audit calls without publishing paths or model content."""
    for case in result.get("cases", []):
        root = output / "private-sessions" / case["case"]
        reads = {"reviewer": 0, "host": 0}
        inspections = {"Read": 0, "Glob": 0, "Grep": 0}
        for events in root.rglob("events.jsonl"):
            child = "subagents" in events.parts
            for line in events.read_text().splitlines():
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if event.get("type") == "tool_call" and event.get("name") in inspections:
                    inspections[event["name"]] += 1
                    path = event.get("input", {}).get("path", "")
                    if event["name"] == "Read" and isinstance(path, str) and "://" not in path:
                        reads["reviewer" if child else "host"] += 1
        case["workspace_read_requests"] = reads
        case["inspection_requests"] = inspections
        for snapshot in root.rglob("goal.json"):
            if "subagents" in snapshot.parts:
                continue
            try:
                goal = json.loads(snapshot.read_text()).get("current") or {}
            except ValueError:
                continue
            reason = goal.get("review_failure_reason") or ""
            if reason:
                case["review_failure_category"] = next((label for marker, label in [
                    ("MaxTurnsExceeded", "host_submission_round_limit"),
                    ("format_recovery_exhausted", "format_recovery_exhausted"),
                    ("token soft limit", "token_budget"),
                    ("deadline", "deadline"),
                    ("host agent did not submit", "host_submission_unavailable"),
                ] if marker in reason), "other_review_failure")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=Path.home() / ".minkrc")
    parser.add_argument("--output", type=Path, default=Path("target/bench/goal-review"))
    parser.add_argument("--draft-only", action="store_true", help="Run two real draft discussions without confirming execution.")
    parser.add_argument("--force-full", action="store_true", help="Compare a full-inspection prompt policy on the same runtime.")
    parser.add_argument("--cases", nargs="+", choices=["complete", "partial", "forged_claim", "missing_evidence", "external_blocker", "partial_admission", "ambiguous", "optional_next_step", "contradictory", "unverifiable"], help="Run only these audit regressions.")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[1]
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    # Capture the source identity before the run, including new implementation
    # files which a tracked-only diff would omit. Never publish source content.
    paths = set(subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "crates", "scripts/evaluate-goal-review.py"], cwd=repo).decode().split("\0")) - {""}
    digest = hashlib.sha256()
    for path in sorted(paths):
        file = repo / path
        if file.is_file():
            digest.update(path.encode() + b"\0" + file.read_bytes() + b"\0")
    provenance = {
        "started_at": datetime.now(timezone.utc).isoformat(),
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
        "dirty": bool(subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=repo, text=True).strip()),
        "source_sha256": digest.hexdigest(),
        "source_scope": "git-listed crates sources and evaluate-goal-review.py, including nonignored new files",
    }
    provider = tomllib.loads(args.config.read_text()).get("provider", {}) if args.config.exists() else {}
    key = os.environ.get("DEEPSEEK_API_KEY") or provider.get("api_key")
    if not key:
        (output / "results.json").write_text(json.dumps({"skipped": "no configured provider credential"}) + "\n")
        print("Skipped: no configured provider credential.")
        return
    env = os.environ.copy()
    env.update(MINK_EVAL_API_KEY=key,
               MINK_EVAL_BASE_URL=os.environ.get("DEEPSEEK_BASE_URL") or provider.get("base_url", "https://api.deepseek.com"),
               MINK_EVAL_MODEL=provider.get("model_aliases", {}).get(provider.get("model", "flash"), provider.get("model", "deepseek-v4-flash")),
               MINK_EVAL_DIRECTORY=str(output / "private-sessions"))
    if args.cases:
        env["MINK_EVAL_CASES"] = ",".join(args.cases)
    if args.force_full:
        env["MINK_EVAL_FORCE_FULL"] = "1"
    if args.draft_only:
        env["MINK_EVAL_DRAFT_ONLY"] = "1"
    run = subprocess.run(["cargo", "run", "-q", "-p", "mink-core", "--example", "goal_review_eval"], cwd=repo, env=env, text=True, capture_output=True)
    # Provider diagnostics can contain URLs; keep them in local logs only.
    (output / "private-run.log").write_text(run.stdout + "\n" + run.stderr)
    if run.returncode:
        (output / "results.json").write_text(json.dumps({"failed": "evaluation process failed", "exit_code": run.returncode}) + "\n")
        raise SystemExit("Evaluation failed; private diagnostics remain in the output directory.")
    result = json.loads(run.stdout.strip().splitlines()[-1])
    result.update(provenance)
    enrich_results(result, output)
    valid = [c for c in result["cases"] if (c.get("valid_reviews", 0) + c.get("lightweight_continuations", 0)) > 0 and not c.get("review_failure", False)]
    result["false_completions"] = sum(c["completed"] and not c["expected_complete"] for c in valid)
    result["false_rejections"] = sum(not c["completed"] and c["expected_complete"] for c in valid)
    result["invalid_or_failed_samples"] = len(result["cases"]) - len(valid)
    (output / "results.json").write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n")
    print(json.dumps(result.get("draft_formation") or {"samples": len(result["cases"]), "false_completions": result["false_completions"], "false_rejections": result["false_rejections"], "invalid_or_failed_samples": result["invalid_or_failed_samples"]}))


if __name__ == "__main__":
    main()
