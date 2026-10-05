#!/usr/bin/env python3
"""Validate release versions before building or publishing any channel."""
import argparse
import json
from pathlib import Path
import re
import tomllib


def require(condition, message):
    if not condition:
        raise ValueError(message)


def check(root: Path, tag: str | None = None) -> str:
    cargo = tomllib.loads((root / "Cargo.toml").read_text())
    version = cargo["workspace"]["package"]["version"]
    require(re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version), f"invalid version: {version}")
    minimum_rust = cargo["workspace"]["package"]["rust-version"]
    require(minimum_rust == "1.99", "MSRV mismatch")
    for file in ["README.md", "crates/mink-core/README.md", "docs/start/quickstart.md", "docs/integration/rust.md", "docs/integration/python.md", "docs/index.html", "docs/development/web-and-site.md"]:
        require(f"Rust {minimum_rust}" in (root / file).read_text(), f"documented MSRV: {file}")
    workflow = (root / ".github/workflows/ci.yml").read_text()
    require(f"dtolnay/rust-toolchain@{minimum_rust}.0" in workflow, "CI MSRV toolchain")
    if tag is not None:
        require(tag == f"v{version}", f"tag {tag!r} does not match v{version}")
    for name in ["mink-core", "mink-cli", "mink-server"]:
        package = tomllib.loads((root / f"crates/{name}/Cargo.toml").read_text())
        require(package["package"]["version"] == {"workspace": True}, f"{name} version inheritance")
        require(package["package"]["rust-version"] == {"workspace": True}, f"{name} MSRV inheritance")
        if name != "mink-core":
            require(package["dependencies"]["mink"]["version"] == version, f"{name} core version")
    packages = tomllib.loads((root / "Cargo.lock").read_text())["package"]
    for name in ["mink-core", "mink-cli", "mink-server"]:
        matches = [p for p in packages if p["name"] == name and "source" not in p]
        require(len(matches) == 1 and matches[0]["version"] == version, f"{name} lock version")
    require(tomllib.loads((root / "pyproject.toml").read_text())["project"]["version"] == version, "Python version")
    web = root / "crates/mink-server/web"
    require(json.loads((web / "package.json").read_text())["version"] == version, "Web version")
    lock = json.loads((web / "package-lock.json").read_text())
    require(lock["version"] == lock["packages"][""]["version"] == version, "Web lock version")
    index = (root / "docs/index.html").read_text()
    require(f'v{version} / OPEN SOURCE' in index, "homepage version")
    for file in ["README.md", "crates/mink-core/README.md", "docs/integration/rust.md", "docs/index.html"]:
        examples = re.findall(r'package = "mink-core", version = "([^"]+)"', (root / file).read_text())
        require(examples and set(examples) == {version}, f"installation version: {file}")
    changelog = (root / "CHANGELOG.md").read_text()
    require(re.search(rf"^## v{re.escape(version)} \(\d{{4}}-\d{{2}}-\d{{2}}\)$", changelog, re.M), "missing release notes")
    return version


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag")
    args = parser.parse_args()
    version = check(Path(__file__).resolve().parent.parent, args.tag)
    print(f"Release versions match: {version}")
