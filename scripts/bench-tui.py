#!/usr/bin/env python3
"""Build release TUI benchmarks, then time only the test executable (including RSS)."""
import json
from pathlib import Path
import subprocess
import sys

root = Path(__file__).resolve().parents[1]
build = subprocess.run(["cargo", "test", "-p", "mink-cli", "--release", "--features", "tui", "--lib", "--no-run", "--message-format=json"], cwd=root, text=True, stdout=subprocess.PIPE, check=True)
executable = None
for line in build.stdout.splitlines():
    event = json.loads(line)
    if event.get("reason") == "compiler-artifact" and event.get("executable") and event["target"]["name"] == "mink_cli":
        executable = event["executable"]
assert executable, "test executable not found"
flag = "-l" if sys.platform == "darwin" else "-v"
subprocess.run(["/usr/bin/time", flag, executable, "tui_release_", "--ignored", "--nocapture", "--test-threads=1"], cwd=root, check=True)
