#!/usr/bin/env python3
"""Analyze pronoun style in mink session thinking traces.

Usage:
    python3 scripts/analyze_thinking_style.py <session_dir> [--skip N]
    python3 scripts/analyze_thinking_style.py <conversation.jsonl> [--skip N]

Reads <session_dir>/conversation.jsonl (or a direct JSONL path), extracts assistant
thinking blocks in message order, skips the first N blocks (default 2, i.e. the
initial/prefab round), and reports per-block and aggregate counts and shares for
the styles: "we", "let's", "let me", "i".

Counting rules
--------------
- Whole words only, case-insensitive ("We", "LET'S", "I" all count).
- Exact word matching: "we" does not match inside "were" or "We'll";
  "i" does not match inside "in" or "i.e."; "let me" counts only as the
  adjacent pair "let" + "me".
- Separately, a "style family" total includes common contractions
  (we'll/we've/we're/we'd -> we; i'll/i'm/i've/i'd -> i), so the collective
  vs first-person-singular picture is complete without polluting the exact
  per-block counts.

Also reports total, average (mean), and P50 (50th percentile, linear
interpolation) of the analyzed thinking block character lengths.

The default skip of 2 implements "ignore the first round": the first user
message is the prefab initialization whose assistant turn is the first thinking
block; the second block is the follow-up "load full instructions" turn.
"""
import argparse
import json
import os
import sys
from collections import Counter

STYLES = ["we", "let's", "let me", "i"]
FAMILY = {
    "we'll": "we", "we've": "we", "we're": "we", "we'd": "we",
    "i'll": "i", "i'm": "i", "i've": "i", "i'd": "i",
}
PUNCT = ".,;:!?'\"()[]{}`<>"

FAMILY_EXPLAIN = {s: [k for k, v in FAMILY.items() if v == s] for s in STYLES}


def extract_thinking(rows):
    """Return assistant thinking block texts in message order."""
    blocks = []
    for r in rows:
        if r.get("role") != "assistant":
            continue
        content = r.get("content")
        if not isinstance(content, list):
            continue
        for b in content:
            if isinstance(b, dict) and b.get("type") == "thinking":
                blocks.append(b.get("thinking") or "")
    return blocks


def tokens(text):
    return [tok.lower().strip(PUNCT) for tok in text.split()]


def count_style(text, family=False):
    """Count style-word occurrences in one thinking block.

    family=False: exact words only. family=True: contractions map into
    their root style ("we'll" -> "we", "i'm" -> "i").
    """
    toks = [FAMILY.get(t, t) if family else t for t in tokens(text)]
    c = Counter()
    for i, tok in enumerate(toks):
        if tok == "we":
            c["we"] += 1
        elif tok == "let's":
            c["let's"] += 1
        elif tok == "let" and i + 1 < len(toks) and toks[i + 1] == "me":
            c["let me"] += 1
        elif tok == "i":
            c["i"] += 1
    return c


def percentile(values, p):
    """p-th percentile (0.0-1.0) with linear interpolation, numpy-style.

    rank = (n-1) * p, then linear interpolation between the two
    surrounding sorted values; identical to numpy.percentile(...,
    interpolation='linear') and Excel PERCENTILE.INC.
    """
    xs = sorted(values)
    if not xs:
        return 0.0
    if len(xs) == 1:
        return float(xs[0])
    rank = (len(xs) - 1) * p
    lo = int(rank)
    frac = rank - lo
    if lo + 1 < len(xs):
        return xs[lo] + frac * (xs[lo + 1] - xs[lo])
    return float(xs[lo])


def fmt_pct(part, total):
    if total == 0:
        return "0.0%"
    return f"{100.0 * part / total:.1f}%"


def print_totals(analyzed, label, family):
    totals = Counter()
    for t in analyzed:
        totals.update(count_style(t, family=family))
    total_occ = sum(totals.values())
    print(f"totals — {label}:")
    print(f"  occurrences: {total_occ}")
    for s in STYLES:
        n = totals[s]
        share = fmt_pct(n, total_occ)
        blocks_with = sum(1 for t in analyzed if count_style(t, family=family)[s] > 0)
        coverage = fmt_pct(blocks_with, len(analyzed))
        note = ""
        if family and FAMILY_EXPLAIN[s]:
            note = f"  (incl. {'/'.join(FAMILY_EXPLAIN[s])})"
        print(f"  {s:<7} {n:>4}  share of style occurrences: {share:>6}  "
              f"in {blocks_with}/{len(analyzed)} blocks ({coverage}){note}")
    print()
    return totals, total_occ


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("target", help="session dir or conversation.jsonl path")
    ap.add_argument("--skip", type=int, default=2, help="thinking blocks to skip (default 2 = first round)")
    args = ap.parse_args()

    if args.target.endswith(".jsonl"):
        path = args.target
    else:
        path = os.path.join(args.target, "conversation.jsonl")
    if not os.path.isfile(path):
        sys.exit(f"error: not found: {path}")

    rows = [json.loads(line) for line in open(path, encoding="utf-8") if line.strip()]
    blocks = extract_thinking(rows)

    if args.skip < 0:
        sys.exit("error: --skip must be >= 0")
    if args.skip > len(blocks):
        sys.exit(f"error: --skip {args.skip} exceeds {len(blocks)} thinking blocks")
    skipped = blocks[: args.skip]
    analyzed = blocks[args.skip:]

    print(f"file: {path}")
    print(f"thinking blocks: {len(blocks)}  (skipped first {args.skip}, analyzed {len(analyzed)})")
    print()
    if skipped:
        print("skipped (first round):")
        for i, t in enumerate(skipped, 1):
            preview = t[:70].replace("\n", " ")
            print(f"  [{i}] ({len(t)} chars) {preview}")
        print()

    headers = ["#", "chars", "we", "let's", "let me", "i"]
    rows_out = []
    for idx, t in enumerate(analyzed, start=args.skip + 1):
        c = count_style(t)
        rows_out.append([idx, len(t), c["we"], c["let's"], c["let me"], c["i"]])

    w = [len(h) for h in headers]
    for r in rows_out:
        for i, v in enumerate(r):
            w[i] = max(w[i], len(str(v)))
    print("per-block counts (exact words):")
    print("  " + "  ".join(str(h).rjust(w[i]) for i, h in enumerate(headers)))
    for r in rows_out:
        print("  " + "  ".join(str(v).rjust(w[i]) for i, v in enumerate(r)))
    print()

    total_chars = sum(len(t) for t in analyzed)
    avg_chars = total_chars / len(analyzed) if analyzed else 0.0
    p50_chars = percentile((len(t) for t in analyzed), 0.5)
    print(f"length: {total_chars} chars across {len(analyzed)} analyzed blocks, "
          f"average {avg_chars:.1f}, P50 {p50_chars:.1f} chars per block")
    print()

    totals, total_occ = print_totals(analyzed, "exact words", family=False)
    if total_occ == 0:
        print("no style words found in analyzed thinking blocks")
        return
    print_totals(analyzed, "style family (incl. contractions)", family=True)


if __name__ == "__main__":
    main()
