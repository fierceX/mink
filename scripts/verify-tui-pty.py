#!/usr/bin/env python3
"""Local PTY/Tmux TUI smoke and latency checks against a synthetic SSE provider.

Build: cargo build -p mink-cli --no-default-features --features tui
Run: python3 scripts/verify-tui-pty.py target/debug/mink [--tmux]
SSH: python3 scripts/verify-tui-pty.py /remote/mink --ssh HOST --remote-root /remote/test-root [--tmux]
No user sessions or provider credentials are used.
"""
import argparse
import contextlib
import errno
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import shutil
import signal
import struct
import sys
import subprocess
import tempfile
import termios
import threading
import time


class Provider(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        stop_test = "stop_test" in json.dumps(request.get("messages", []))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        def emit(delta, reason=None):
            payload = {"id": "synthetic", "choices": [{"index": 0, "delta": delta, "finish_reason": reason}]}
            self.wfile.write(("data: " + json.dumps(payload) + "\n\n").encode())
            self.wfile.flush()
        try:
            emit({"content": "WAITING_TAG" if stop_test else "SCROLLBACK_ONCE\n\n"})
            if stop_test:
                time.sleep(2)
            else:
                for text in ["````text\n", "~~~\n```\n", "mixed fence\n", "````\n", "answer complete"]:
                    emit({"content": text})
                    time.sleep(0.025)
            emit({}, "stop")
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass


class Surface:
    def __init__(self, command, env, directory):
        self.master, self.slave = pty.openpty()
        self.resize(80, 24)
        def claim_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        self.process = subprocess.Popen(command, stdin=self.slave, stdout=self.slave, stderr=self.slave, cwd=directory, env=env, preexec_fn=claim_terminal)
        self.data = bytearray()
        self.bytes = 0

    def resize(self, width, height):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        if hasattr(self, "process"):
            os.kill(self.process.pid, signal.SIGWINCH)

    def pump(self, duration=0.1, stop_when=None):
        deadline = time.monotonic() + duration
        result = bytearray()
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.master], [], [], max(0, deadline - time.monotonic()))
            if not ready:
                break
            try:
                chunk = os.read(self.master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:  # PTY hangup on Linux.
                    break
                raise
            if not chunk:
                break
            self.bytes += len(chunk)
            self.data.extend(chunk)
            result.extend(chunk)
            # Answer Inline's cursor position query. Keyboard enhancement is disabled.
            if b"\x1b[6n" in chunk:
                os.write(self.master, b"\x1b[1;1R")
            if stop_when and stop_when():
                break
        return bytes(result)

    def wait(self, text, offset=0, timeout=10):
        deadline = time.monotonic() + timeout
        needle = text.encode()
        def present():
            raw = bytes(self.data[offset:])
            clean = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", raw)
            clean = re.sub(rb"\x1b\][^\x07]*(?:\x07|\x1b\\)", b"", clean)
            return needle in raw or needle.replace(b" ", b"") in clean.replace(b" ", b"")
        while not present():
            self.pump(0.02, stop_when=present)
            if self.process.poll() is not None or time.monotonic() > deadline:
                raise AssertionError(f"Missing {text!r}; process={self.process.poll()}; tail={bytes(self.data[-1500:])!r}")

    def paste(self, text):
        offset = len(self.data)
        os.write(self.master, b"\x1b[200~" + text.encode() + b"\x1b[201~")
        return offset

    def command(self, text):
        os.write(self.master, b"\x15")  # Ctrl+U, whole draft before cursor.
        self.paste(text)
        os.write(self.master, b"\r")
        self.pump(0.15)

    def settle(self, timeout=3):
        deadline = time.monotonic() + timeout
        while self.pump(0.3):
            if time.monotonic() >= deadline:
                raise AssertionError("terminal did not become idle after initialization")

    def wait_exit(self, timeout=10):
        deadline = time.monotonic() + timeout
        while self.process.poll() is None:
            self.pump(0.05)
            if time.monotonic() >= deadline:
                raise AssertionError(f"process did not exit; tail={bytes(self.data[-1000:])!r}")

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.wait_exit(2)
            except AssertionError:
                self.process.kill()
                self.process.wait(timeout=2)
        os.close(self.master)
        os.close(self.slave)


class Remote:
    def __init__(self, target, root):
        self.target, self.root = target, root

    def run(self, args, **kwargs):
        return subprocess.run(["ssh", "-o", "BatchMode=yes", self.target, shlex.join(args)], check=True, **kwargs)

    def output(self, args):
        return self.run(args, stdout=subprocess.PIPE, text=True).stdout

    @contextlib.contextmanager
    def temporary_directory(self):
        directory = self.output(["mktemp", "-d", self.root.rstrip("/") + "/pty.XXXXXX"]).strip()
        try:
            yield directory
        finally:
            self.run(["rm", "-rf", "--", directory])


def verify(binary, mode, base_url, use_tmux, remote=None):
    with tempfile.TemporaryDirectory(prefix="mink-pty-") as directory, \
            (remote.temporary_directory() if remote else contextlib.nullcontext(directory)) as work_directory:
        env = os.environ.copy()
        env.update(MINK_HOME=directory, MINK_KEYBOARD_ENHANCEMENT="off", TERM="xterm-256color", MINK_SIGNAL_POLICY="off")
        forward = []
        if remote:
            port = remote.output(["python3", "-c", "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1])"]).strip()
            local_port = base_url.split(":")[-1].split("/")[0]
            forward = ["-o", "ExitOnForwardFailure=yes", "-R", f"127.0.0.1:{port}:127.0.0.1:{local_port}"]
            base_url = f"http://127.0.0.1:{port}/v1"
        args = [binary, "--tui=" + mode, "--api-key", "synthetic", "--base-url", base_url]
        # Check Mink's terminal while its controlling session still exists. On
        # macOS tcgetattr on the parent slave fails after session-leader exit.
        restore_report = Path(work_directory) / "terminal-restore.json"
        wrapper = """import json, subprocess, sys, termios
before = termios.tcgetattr(0)
result = subprocess.run(sys.argv[2:])
with open(sys.argv[1], 'w') as report:
    json.dump({'restored': termios.tcgetattr(0) == before, 'returncode': result.returncode}, report)
sys.exit(result.returncode)
"""
        args = ["python3" if remote else sys.executable, "-c", wrapper, str(restore_report), *args]
        if remote:
            args = ["env", "MINK_HOME=" + work_directory, "MINK_KEYBOARD_ENHANCEMENT=off", "TERM=xterm-256color", "MINK_SIGNAL_POLICY=off", *args]
        socket = "mink-qa-" + str(os.getpid()) + "-" + mode
        def tmux(args, capture=False, quiet=False):
            command = ["tmux", "-L", socket, *args]
            stderr = subprocess.DEVNULL if quiet else None
            if remote:
                return remote.run(command, stdout=subprocess.PIPE if capture else subprocess.DEVNULL, stderr=stderr).stdout
            return subprocess.run(command, env=env, cwd=directory, check=True, stdout=subprocess.PIPE if capture else subprocess.DEVNULL, stderr=stderr).stdout
        if use_tmux:
            inner = "cd " + shlex.quote(work_directory) + " && exec " + shlex.join(args)
            tmux(["-f", "/dev/null", "new-session", "-d", "-s", "qa", "-x", "80", "-y", "24", inner])
            tmux(["set-option", "-s", "escape-time", "0"])
            command = ["tmux", "-L", socket, "attach-session", "-t", "qa"]
        else:
            command = args
        if remote:
            inner = "cd " + shlex.quote(work_directory) + " && exec " + shlex.join(command)
            command = ["ssh", "-tt", "-o", "BatchMode=yes", *forward, remote.target, inner]
        surface = Surface(command, env, directory)
        try:
            surface.wait("Enter: send task")
            surface.settle()
            # Truly idle terminals produce no drawing output.
            assert not surface.pump(0.25), "idle TUI redrew"
            latencies = []
            for index in range(20):
                os.write(surface.master, b"\x15")
                surface.pump(0.025)
                marker = f"LATENCY_{index:02d}"
                start = time.monotonic()
                offset = surface.paste(marker)
                surface.wait(marker, offset)
                latencies.append((time.monotonic() - start) * 1000)
            surface.command("normal_test")
            surface.wait("answer complete")
            surface.pump(0.2)
            for panel, marker in [("/help", "Commands:"), ("/status", "Cache hit:"), ("/inputs", "explicitly resume"), ("/details", "Enter: open")]:
                surface.command(panel)
                surface.wait(marker)
                if use_tmux:
                    tmux(["send-keys", "-t", "qa", "Escape"])
                    surface.pump(0.6)
                else:
                    os.write(surface.master, b"\x1b")
                    surface.pump(0.1)
            for width in [40, 120, 80]:
                surface.resize(width, 24)
                surface.pump(0.15)
            if use_tmux and mode == "inline":
                capture = tmux(["capture-pane", "-p", "-S", "-", "-t", "qa"], capture=True)
                assert capture.count(b"SCROLLBACK_ONCE") == 1, capture.decode(errors="replace")
            surface.command("stop_test")
            surface.wait("WAITING_TAG")
            streaming_latencies = []
            for index in range(10):
                os.write(surface.master, b"\x15")
                surface.pump(0.025)
                marker = f"STREAM_LATENCY_{index:02d}"
                start = time.monotonic()
                offset = surface.paste(marker)
                surface.wait(marker, offset)
                streaming_latencies.append((time.monotonic() - start) * 1000)
            os.write(surface.master, b"\x03")
            surface.wait("Turn interrupted")
            surface.command("/quit")
            surface.wait_exit()
            surface.pump(0.05)
            assert surface.process.returncode == 0, surface.process.returncode
            report = json.loads(remote.output(["cat", str(restore_report)]) if remote else restore_report.read_text())
            assert report == {"restored": True, "returncode": 0}, report
            if not use_tmux:
                assert b"\x1b[?2004l" in surface.data, "bracketed paste was not disabled"
            if mode == "full" and not use_tmux:
                assert b"\x1b[?1049l" in surface.data, "alternate screen was not restored"
            if sys.platform == "darwin" and not remote and not use_tmux:
                # One completed task and one interrupted task: each produces
                # one terminal-owned notification, not two OSC protocols.
                assert surface.data.count(b"\x1b]9;") == 2, "missing or duplicate macOS notification"
                assert b"\x1b]777;notify;" not in surface.data, "duplicate macOS notification protocol"
            latencies.sort()
            streaming_latencies.sort()
            surface_name = ("ssh+" if remote else "") + ("tmux" if use_tmux else "pty")
            print(json.dumps({"surface": surface_name, "mode": mode, "input_draw_p95_ms": round(latencies[18], 2), "streaming_input_draw_p95_ms": round(streaming_latencies[-1], 2), "terminal_bytes": surface.bytes, "idle_draw_bytes": 0, "terminal_restored": True}), flush=True)
        finally:
            surface.close()
            if use_tmux:
                try:
                    tmux(["kill-server"], quiet=True)
                except subprocess.CalledProcessError:
                    pass  # The last pane exiting may already stop this isolated server.


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("--tmux", action="store_true")
    parser.add_argument("--ssh", help="SSH host alias; the binary must already exist on that host")
    parser.add_argument("--remote-root", help="Existing isolated remote directory for temporary test sessions")
    options = parser.parse_args()
    if options.ssh and not options.remote_root:
        parser.error("--ssh requires --remote-root")
    if options.tmux and not options.ssh and not shutil.which("tmux"):
        parser.error("tmux is required for --tmux")
    remote = Remote(options.ssh, options.remote_root) if options.ssh else None
    binary = options.binary if remote else str(Path(options.binary).resolve())
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        for mode in ["full", "inline"]:
            verify(binary, mode, f"http://127.0.0.1:{server.server_port}/v1", options.tmux, remote)
        if remote:
            return  # No-TTY initialization is already covered by the local harness.
        with tempfile.TemporaryDirectory(prefix="mink-no-tty-") as directory:
            env = os.environ.copy()
            env.update(MINK_HOME=directory, MINK_KEYBOARD_ENHANCEMENT="off")
            failed = subprocess.run([binary, "--tui", "--api-key", "synthetic"], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, cwd=directory)
            assert failed.returncode != 0 and b"TUI error" in failed.stderr, failed.stderr
            print(json.dumps({"initialization_failure_reported": True}), flush=True)
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
