#!/usr/bin/env python3
"""Verify a built `mink-server` binary embeds the exact bytes of `apps/web/dist`.

`build.rs` publishes the manifest from a verified staging directory; this check
is the release-binary counterpart: it proves the index, the assets the index
references, and every produced asset are present verbatim. A binary-size check
alone cannot catch a manifest that names files the binary does not contain.

Usage: verify-embedded-web.py <mink-server-binary> <apps/web/dist>
"""

from __future__ import annotations

import sys
from pathlib import Path


def asset_refs(index: str) -> list[str]:
    refs: list[str] = []
    rest = index
    marker = "./assets/"
    while True:
        start = rest.find(marker)
        if start < 0:
            break
        candidate = rest[start + 2 :]
        end = len(candidate)
        for position, char in enumerate(candidate):
            if char in "\"' )\t\r\n":
                end = position
                break
        reference = candidate[:end]
        if reference not in refs:
            refs.append(reference)
        rest = rest[start + len(marker) :]
    return refs


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print("usage: verify-embedded-web.py <mink-server-binary> <dist-dir>", file=sys.stderr)
        return 2
    binary_path = Path(argv[1])
    dist = Path(argv[2])
    if not binary_path.is_file():
        print(f"error: binary not found: {binary_path}", file=sys.stderr)
        return 1
    index_path = dist / "index.html"
    if not index_path.is_file():
        print(f"error: {index_path} is missing (the web build did not run)", file=sys.stderr)
        return 1
    index = index_path.read_bytes()
    if not index:
        print(f"error: {index_path} is empty", file=sys.stderr)
        return 1
    refs = asset_refs(index.decode("utf-8", errors="replace"))
    if not any(reference.endswith(".js") for reference in refs):
        print("error: index.html references no script asset", file=sys.stderr)
        return 1
    if not any(reference.endswith(".css") for reference in refs):
        print("error: index.html references no stylesheet asset", file=sys.stderr)
        return 1

    required: list[Path] = [index_path]
    for reference in refs:
        path = dist / reference
        if not path.is_file():
            print(f"error: referenced asset is missing from dist: {reference}", file=sys.stderr)
            return 1
        required.append(path)
    for path in sorted(dist.rglob("*")):
        if path.is_file() and path not in required:
            required.append(path)

    binary = binary_path.read_bytes()
    missing: list[str] = []
    verified = 0
    for path in required:
        data = path.read_bytes()
        if not data:
            continue
        if binary.find(data) < 0:
            missing.append(str(path.relative_to(dist)) if path != index_path else "index.html")
        else:
            verified += 1
    if missing:
        print("error: these assets are not embedded verbatim in the binary:", file=sys.stderr)
        for name in missing:
            print(f"  - {name}", file=sys.stderr)
        return 1
    print(f"verified {verified} embedded web assets from {dist} in {binary_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
