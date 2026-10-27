"""Pin the Python SDK defaults that bound long-running sessions."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from mink_agent import SandboxConfig  # noqa: E402


class LongRunDefaults(unittest.TestCase):
    def test_defaults_allow_extended_work(self):
        cfg = SandboxConfig()
        self.assertEqual(cfg.timeout_secs, 7_200)
        self.assertEqual(cfg.sub_agent_timeout, 1_800)
        self.assertEqual(cfg.max_turns, 200)
        self.assertEqual(cfg.tool_timeout, 7_200)
        self.assertEqual(cfg.tool_timeout_max, 7_200)


if __name__ == "__main__":
    unittest.main()
