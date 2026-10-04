#!/usr/bin/env python3
"""Publish core, skipping only a verified exact version on crates.io."""
import json
from pathlib import Path
import subprocess
import tomllib
from urllib.error import HTTPError
from urllib.request import Request, urlopen


def already_published(version: str) -> bool:
    request = Request(
        f"https://crates.io/api/v1/crates/mink-core/{version}",
        headers={"User-Agent": "mink-release-check", "Accept": "application/json"},
    )
    try:
        with urlopen(request, timeout=30) as response:
            data = json.load(response)
    except HTTPError as error:
        error.close()
        if error.code == 404:
            return False
        raise
    entry = data["version"]
    if entry["crate"] != "mink-core" or entry["num"] != version or entry["yanked"]:
        raise ValueError("registry response does not identify an available exact release")
    return True


def publish(version: str, root: Path) -> None:
    if already_published(version):
        print(f"Verified mink-core {version} on crates.io; skipping publish")
        return
    subprocess.run(
        ["cargo", "publish", "-p", "mink-core", "--registry", "crates-io", "--locked"],
        cwd=root, check=True,
    )


if __name__ == "__main__":
    root = Path(__file__).resolve().parent.parent
    version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    publish(version, root)
