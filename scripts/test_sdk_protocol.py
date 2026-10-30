"""Pin how the Python SDK interprets the Agent JSONL final event.

The SDK must only claim task success when a valid ``final`` event proves it.
Process exit status and protocol terminal status are separate signals; an
exit code of 0 cannot synthesize success and cannot upgrade a failure.
"""

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from mink_agent import AgentError, AgentSession, SandboxConfig  # noqa: E402

FAKE_BINARY = r'''#!/usr/bin/env python3
import json
import os
import sys
import time

mode = os.environ.get("MINK_FAKE_MODE", "")


def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


if mode != "ignore_stdin":
    sys.stdin.read()

if mode == "empty_ok":
    pass
elif mode == "not_json_ok":
    print("not-json")
elif mode == "text_only":
    emit({"type": "text", "content": "partial answer"})
elif mode == "thinking_text_only":
    emit({"type": "thinking", "content": "hmm"})
    emit({"type": "text", "content": "partial answer"})
elif mode == "final_ok":
    emit({
        "type": "final",
        "version": 3,
        "status": "ok",
        "billing_turn_id": "turn-1",
        "session_id": "s-1",
        "session_ref": "s-1",
        "home": "/tmp/home",
        "cwd": "/tmp",
        "events_path": "/tmp/events.jsonl",
        "conversation_path": "/tmp/conversation.jsonl",
        "artifacts_dir": "/tmp/artifacts",
        "summary_path": "/tmp/summary.txt",
        "usage_path": "/tmp/usage.jsonl",
        "tool_call_count": 0,
        "tool_error_count": 0,
        "error": None,
        "usage_records": [],
        "usage": {},
        "future_field": {"kept": True},
    })
elif mode == "final_no_status":
    emit({"type": "final", "version": 3})
elif mode == "final_bad_status_type":
    emit({"type": "final", "version": 3, "status": 5})
elif mode == "final_unknown_status":
    emit({"type": "final", "version": 3, "status": "bogus"})
elif mode == "final_ok_exit3":
    emit({"type": "final", "version": 3, "status": "ok", "error": None})
    sys.stdout.flush()
    sys.exit(3)
elif mode == "final_failed":
    emit({"type": "final", "version": 3, "status": "failed", "error": "model rejected"})
elif mode == "final_interrupted":
    emit({"type": "final", "version": 3, "status": "interrupted"})
elif mode == "final_max_turns":
    emit({"type": "final", "version": 3, "status": "max_turns_exceeded"})
elif mode == "final_then_sleep":
    emit({"type": "final", "version": 3, "status": "ok"})
    sys.stdout.flush()
    time.sleep(30)
elif mode == "stderr_only":
    sys.stderr.write("boom from agent\n")
    sys.stderr.flush()
    sys.exit(1)
elif mode == "exit7":
    sys.exit(7)
elif mode == "ignore_stdin":
    sys.stderr.write("closed stdin\n")
    sys.stderr.flush()
    sys.exit(0)
else:
    raise SystemExit("unknown MINK_FAKE_MODE: " + mode)
'''


class SDKProtocolHarness(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory()
        cls.tmpdir = Path(cls._tmp.name)
        binary = cls.tmpdir / "mink-core"
        binary.write_text(FAKE_BINARY, encoding="utf-8")
        binary.chmod(0o755)
        cls._previous_binary = os.environ.get("MINK_BINARY")
        os.environ["MINK_BINARY"] = str(binary)

    @classmethod
    def tearDownClass(cls):
        if cls._previous_binary is None:
            os.environ.pop("MINK_BINARY", None)
        else:
            os.environ["MINK_BINARY"] = cls._previous_binary
        cls._tmp.cleanup()

    def setUp(self):
        self._mode = os.environ.get("MINK_FAKE_MODE")
        # Keep the request well below the pipe buffer unless a test opts out.
        self._mission = None

    def tearDown(self):
        if self._mode is None:
            os.environ.pop("MINK_FAKE_MODE", None)
        else:
            os.environ["MINK_FAKE_MODE"] = self._mode
        if self._mission is not None:
            self._mission.cleanup()

    def make_session(self, mode, *, timeout_secs=10):
        os.environ["MINK_FAKE_MODE"] = mode
        home = tempfile.mkdtemp(dir=self.tmpdir)
        return AgentSession(
            SandboxConfig(
                mink_home=home,
                session_layout="isolated",
                cwd=str(self.tmpdir),
                sandbox_backend="off",
                api_key="fixture-key",
                api_url="http://127.0.0.1:1/v1",
                model="fixture-model",
                timeout_secs=timeout_secs,
            )
        )

    def collect(self, mode, *, timeout_secs=10):
        session = self.make_session(mode, timeout_secs=timeout_secs)
        try:
            events = list(session.raw_stream("hello"))
        finally:
            session.close()
        self.assertTrue(events, "SDK must always yield a final event")
        self.assertEqual(events[-1].get("type"), "final")
        return events[:-1], events[-1]

    def run_result(self, mode, *, timeout_secs=10):
        session = self.make_session(mode, timeout_secs=timeout_secs)
        try:
            return session.run("hello")
        finally:
            session.close()


class MissingFinal(SDKProtocolHarness):
    def test_empty_stdout_exit_zero_is_failed(self):
        _, final = self.collect("empty_ok")
        self.assertEqual(final["status"], "failed")
        self.assertIn("final", final["error"].lower())
        self.assertEqual(final["exit_code"], 0)

    def test_unparsable_stdout_exit_zero_is_failed(self):
        _, final = self.collect("not_json_ok")
        self.assertEqual(final["status"], "failed")
        self.assertIsNotNone(final["error"])
        self.assertEqual(final["exit_code"], 0)

    def test_streamed_content_is_kept_but_terminal_status_fails(self):
        events, final = self.collect("text_only")
        self.assertEqual([e.get("content") for e in events], ["partial answer"])
        self.assertEqual(final["status"], "failed")
        result = self.run_result("text_only")
        self.assertEqual(result["text"], "partial answer")
        self.assertEqual(result["status"], "failed")

    def test_stderr_only_reports_diagnostics(self):
        _, final = self.collect("stderr_only")
        self.assertEqual(final["status"], "failed")
        self.assertEqual(final["exit_code"], 1)
        self.assertIn("boom from agent", final["stderr"])

    def test_nonzero_exit_without_output_is_failed(self):
        _, final = self.collect("exit7")
        self.assertEqual(final["status"], "failed")
        self.assertEqual(final["exit_code"], 7)
        self.assertIn("7", final["error"])


class FinalValidation(SDKProtocolHarness):
    def test_final_without_status_is_a_protocol_failure(self):
        _, final = self.collect("final_no_status")
        self.assertEqual(final["status"], "failed")
        self.assertIn("status", final["error"])

    def test_final_with_wrong_status_type_is_a_protocol_failure(self):
        _, final = self.collect("final_bad_status_type")
        self.assertEqual(final["status"], "failed")
        self.assertIsNotNone(final["error"])

    def test_final_with_unknown_status_is_a_protocol_failure(self):
        _, final = self.collect("final_unknown_status")
        self.assertEqual(final["status"], "failed")
        self.assertIn("bogus", final["error"])

    def test_valid_ok_final_keeps_extension_fields(self):
        _, final = self.collect("final_ok")
        self.assertEqual(final["status"], "ok")
        self.assertEqual(final["future_field"], {"kept": True})
        self.assertEqual(final["exit_code"], 0)


class ProtocolVersusProcessExit(SDKProtocolHarness):
    def test_legal_failure_is_not_upgraded_by_exit_zero(self):
        _, final = self.collect("final_failed")
        self.assertEqual(final["status"], "failed")
        self.assertEqual(final["error"], "model rejected")

    def test_legal_interruption_is_preserved(self):
        result = self.run_result("final_interrupted")
        self.assertEqual(result["status"], "interrupted")
        self.assertEqual(result["error"], "interrupted")

    def test_max_turns_status_is_preserved(self):
        result = self.run_result("final_max_turns")
        self.assertEqual(result["status"], "max_turns_exceeded")

    def test_ok_final_with_nonzero_exit_is_not_success(self):
        _, final = self.collect("final_ok_exit3")
        self.assertEqual(final["status"], "failed")
        self.assertEqual(final["exit_code"], 3)
        self.assertIn("3", final["error"])

    def test_final_without_exit_is_terminated_and_failed(self):
        _, final = self.collect("final_then_sleep", timeout_secs=1)
        self.assertEqual(final["status"], "failed")
        self.assertIsNotNone(final["error"])


class ProcessLifecycle(SDKProtocolHarness):
    def test_early_stdin_close_raises_with_stderr(self):
        os.environ["MINK_FAKE_MODE"] = "ignore_stdin"
        home = tempfile.mkdtemp(dir=self.tmpdir)
        session = AgentSession(
            SandboxConfig(
                mink_home=home,
                session_layout="isolated",
                cwd=str(self.tmpdir),
                sandbox_backend="off",
                # A request larger than the pipe buffer makes the early
                # child exit deterministic: the parent write hits EPIPE.
                mission_content="# Mission\n\n" + ("payload " * 40000),
                timeout_secs=10,
            )
        )
        try:
            with self.assertRaises(AgentError) as ctx:
                list(session.raw_stream("hello"))
        finally:
            session.close()
        self.assertIn("closed stdin", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
