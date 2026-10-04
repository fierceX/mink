#!/usr/bin/env python3
"""Verify an installed wheel from an isolated interpreter and local provider."""
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib

SMOKE = r'''
import importlib.metadata
import importlib.resources
import json
from pathlib import Path
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from mink_agent import AgentSession, SandboxConfig

expected = sys.argv[1]
assert importlib.metadata.version("mink-agent") == expected
binary = Path(str(importlib.resources.files("mink_agent") / "_binary" / "mink-core"))
assert binary.is_file(), "installed wheel is missing its bundled binary"
binary.chmod(0o755)
version = subprocess.run([str(binary), "--version"], check=True, capture_output=True, text=True, timeout=10).stdout
# Both CLI entry points use the Mink brand in their version banner.
assert version.split()[:2] == ["mink", expected], version

class Provider(BaseHTTPRequestHandler):
    requests = []
    def log_message(self, *args):
        pass
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.requests.append(body)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        frames = [
            {"choices": [{"index": 0, "delta": {"content": "wheel fixture ok"}, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 12, "completion_tokens": 4}},
        ]
        for frame in frames:
            self.wfile.write(("data: " + json.dumps(frame) + "\n\n").encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
session = AgentSession(SandboxConfig(
    mink_home=str(Path.cwd() / "home"),
    session_id="wheel-smoke",
    api_key="fixture-key",
    api_url=f"http://127.0.0.1:{server.server_port}/v1",
    model="fixture-model",
    sandbox_backend="off",
    enabled_tools=[],
    max_context=0,
    max_tokens=256,
    timeout_secs=30,
))
try:
    first = session.run("Return the fixture answer")
    second = session.run("Continue the fixture session")
    for result in [first, second]:
        assert result["exit_code"] == 0 and result["status"] == "ok", result
        assert result["text"] == "wheel fixture ok", result
    assert first["session_id"] == second["session_id"] == "wheel-smoke"
    assert len(Provider.requests) == 2
    assert len(Provider.requests[1]["messages"]) > len(Provider.requests[0]["messages"]), "session history was not reused"
finally:
    session.close()
    server.shutdown()
    server.server_close()
    thread.join(timeout=5)
print(f"Installed wheel {expected}: import, bundled version, two fixture turns and session reuse passed")
'''


if __name__ == "__main__":
    root = Path(__file__).resolve().parent.parent
    version = tomllib.loads((root / "pyproject.toml").read_text())["project"]["version"]
    with tempfile.TemporaryDirectory(prefix="mink-wheel-smoke-") as temporary:
        # -I excludes the checkout/PYTHONPATH: importing source cannot mask a
        # missing or broken installed wheel. The bundled binary is mandatory.
        import os
        env = {k: v for k, v in os.environ.items() if not k.startswith("MINK_") and k not in {"MODEL", "DEEPSEEK_API_KEY", "OPENAI_API_KEY", "OPENAI_BASE_URL"}}
        env["HOME"] = temporary
        subprocess.run([sys.executable, "-I", "-c", SMOKE, version], cwd=temporary, env=env, check=True, timeout=90)
