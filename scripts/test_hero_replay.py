"""Recorded replay export regressions; no provider or user session access."""
import unittest
from pathlib import Path
import build_hero_replay as replay


def prompt(text="task", guidance=False, turn="one"):
    return {"role": "user", "content": text, "_mink": {"guidance": guidance, "turn_id": turn}}


def result(status=None, content="Exit code: 0"):
    metadata = {"tool_name": "Bash"}
    if status is not None: metadata["status"] = {"state": status}
    return {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call-1", "content": content, "_mink": metadata}]}


class RecordedReplayTests(unittest.TestCase):
    def test_guidance_is_bound_to_the_original_turn_and_internal_input_is_skipped(self):
        rows = [prompt(), {"role": "user", "internal": True, "content": "diagnostic"}, prompt("keep empty values", True)]
        steps = replay.build_steps(rows, "demo")
        self.assertEqual([s["kind"] for s in steps], ["prompt", "input", "guidance"])
        self.assertEqual(steps[1]["text"], "keep empty values")
        self.assertEqual(steps[1]["sourceMessage"], steps[2]["sourceMessage"])
        self.assertEqual(steps[2]["sourceMessage"], 2)
        with self.assertRaises(ValueError):
            replay.build_steps([prompt(), prompt("late", True, "other")], "demo")

    def test_tool_calls_are_pending_and_results_keep_real_or_unknown_status(self):
        call = {"role": "assistant", "content": [{"type": "tool_use", "id": "call-1", "name": "Bash", "input": {"command": "echo '<script>'"}}]}
        for state, marker in [("failed", "✗ failed"), ("blocked", "⊘ blocked"), ("interrupted", "■ interrupted"), ("succeeded", "✓"), (None, "状态未记录")]:
            steps = replay.build_steps([call, result(state)], "demo")
            self.assertIn("◇", steps[0]["line"])
            self.assertNotIn("✓", steps[0]["line"])
            self.assertIn(marker, steps[1]["line"])
            self.assertEqual(steps[0]["toolUseId"], steps[1]["toolUseId"])
            self.assertNotIn("<script>", steps[0]["line"])
            self.assertIn("&lt;script&gt;", steps[0]["line"])

    def test_stats_are_recorded_only_at_the_end_and_include_cache_creation(self):
        stats = {"current_turn_count": 1, "agent_request_count": 3, "total_input_tokens": 10, "total_cache_read_tokens": 5, "total_cache_creation_tokens": 3, "total_output_tokens": 8}
        steps = replay.build_steps([prompt(), {"role": "assistant", "content": [{"type": "text", "text": "done"}]}], "demo")
        frames = replay.enrich_steps(steps, "model", stats)
        self.assertEqual(next(item["text"] for item in frames[0]["statusItems"] if item["id"] == "requests"), "R:—")
        self.assertEqual([item["text"] for item in frames[-1]["statusItems"]], ["model", "B:—", "T:1", "R:3", "I:18(27%)", "O:8", "C:—(—)"])
        self.assertEqual(frames[-1]["workState"], "idle")
        self.assertTrue(all(next(i["text"] for i in f["statusItems"] if i["id"] == "input") == "I:—(—)" for f in replay.enrich_steps(steps, "model")))
        incomplete = dict(stats)
        del incomplete["total_cache_creation_tokens"]
        self.assertEqual(next(i["text"] for i in replay.enrich_steps(steps, "model", incomplete)[-1]["statusItems"] if i["id"] == "input"), "I:—(—)")

    def test_status_uses_tui_units_order_priority_and_unknown_context_limit(self):
        steps = [{"kind": "text", "text": "done"}]
        stats = {"current_turn_count": 1, "agent_request_count": 9, "total_input_tokens": 8637, "total_cache_read_tokens": 35840, "total_cache_creation_tokens": 0, "total_output_tokens": 5566, "current_context_tokens": 8829}
        frame = replay.enrich_steps(steps, "flash @env-reader", stats)[0]
        self.assertEqual([item["text"] for item in frame["statusItems"]], ["flash", "@env-reader", "B:—", "T:1", "R:9", "I:44.5k(80%)", "O:5.6k", "C:8.8k(—)"])
        self.assertEqual([item["priority"] for item in frame["statusItems"]], [1, 4, 7, 8, 8, 5, 5, 2])
        stats["max_context_tokens"] = 100_000
        self.assertEqual(replay.enrich_steps(steps, "flash", stats)[0]["statusItems"][-1]["text"], "C:8.8k(8%)")
        self.assertEqual([replay.fmt_k(n) for n in [0, 999, 1000, 1_000_000]], ["0", "999", "1.0k", "1.0m"])

    def test_red_tests_are_kept_even_when_the_shell_pipeline_exits_zero(self):
        steps = replay.build_steps([result("succeeded", "Exit code: 0\nprivate listing\ntest result: FAILED. 3 passed; 3 failed\n")], "demo")
        self.assertEqual(steps[0]["toolStatus"], "succeeded")
        self.assertIn("test result: FAILED. 3 passed; 3 failed", steps[0]["line"])
        self.assertNotIn("private listing", steps[0]["line"])
        self.assertEqual(replay.result_excerpt("test result: ok. 6 passed; 0 failed;\ntest result: ok. 0 passed; 0 failed;"), "test result: ok. 6 passed; 0 failed;")

    def recorded_fixture(self):
        messages = [prompt(), {"role": "assistant", "content": [{"type": "thinking", "thinking": "inspect"}, {"type": "tool_use", "id": "call-1", "name": "Bash", "input": {}}]}, result("succeeded"), prompt("keep empty values", True), {"role": "assistant", "content": [{"type": "text", "text": "done"}]}]
        usage = {"type": "usage", "kind": "agent", "input_tokens": 10, "cache_read_input_tokens": 5, "cache_creation_input_tokens": 3, "output_tokens": 8, "context_tokens": 20, "max_context": 100}
        events = [{"type": "turn_start", "belief": .75}, dict(usage), {"type": "tool_call", "id": "call-1"}, {"type": "tool_result", "tool_use_id": "call-1"}, {"type": "text", "content": "discarded"}, dict(usage), {"type": "text", "content": "done"}, dict(usage), {"type": "turn_tracking", "belief": .9}]
        stats = {"current_turn_count": 1, "agent_request_count": 3, "total_input_tokens": 30, "total_cache_read_tokens": 15, "total_cache_creation_tokens": 9, "total_output_tokens": 24, "current_context_tokens": 22}
        return messages, events, stats

    def test_recorded_usage_progresses_at_accepted_boundaries_and_keeps_discarded_usage(self):
        messages, events, stats = self.recorded_fixture()
        observations = replay.recorded_status(messages, events, stats)
        frames = replay.enrich_steps(replay.build_steps(messages, "demo"), "model @demo", stats, observations)
        field = lambda frame, name: next(item["text"] for item in frame["statusItems"] if item["id"] == name)
        self.assertEqual(field(frames[1], "requests"), "R:0")
        call = next(f for f in frames if f["kind"] == "tool")
        self.assertEqual(field(call, "input"), "I:18(27%)")
        self.assertEqual(call["statusSourceEvent"], 1)
        guide = next(f for f in frames if f["kind"] == "guidance")
        self.assertEqual(field(guide, "requests"), "R:1")
        self.assertEqual(field(guide, "belief"), "B:—")
        self.assertEqual(next(i["text"] for i in frames[-1]["statusBefore"] if i["id"] == "requests"), "R:2")
        self.assertEqual(field(frames[-1], "requests"), "R:3")
        self.assertEqual(field(frames[-1], "belief"), "B:0.90")
        self.assertEqual(field(frames[-1], "context"), "C:22(22%)")

    def test_recorded_status_rejects_mismatched_totals_calls_and_ambiguous_text(self):
        messages, events, stats = self.recorded_fixture()
        with self.assertRaises(ValueError): replay.recorded_status(messages, events, {**stats, "agent_request_count": 9})
        events[2]["id"] = "unrelated"
        with self.assertRaises(ValueError): replay.recorded_status(messages, events, stats)
        events[2]["id"] = "call-1"
        events[4]["content"] = "done"
        with self.assertRaises(ValueError): replay.recorded_status(messages, events, stats)

    def test_sanitization_removes_control_sequences_and_refuses_credentials(self):
        text = "\x1b]0;title\x07\x1b[32m" + str(Path.home()) + "/project\x1b[0m"
        self.assertEqual(replay.sanitize_text(text, "demo"), "/home/user/project")
        with self.assertRaises(ValueError):
            replay.sanitize_text("api_key='dummy-token-for-export-test'", "demo")
        with self.assertRaises(ValueError):
            replay.sanitize_text('{"api_key": "dummy-token-for-export-test"}', "demo")


if __name__ == "__main__":
    unittest.main()
