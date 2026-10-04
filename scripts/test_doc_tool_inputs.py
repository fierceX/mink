import unittest
from doc_tool_inputs import check


class ToolExampleTests(unittest.TestCase):
    def test_envelopes_and_mode_specific_inputs_reject_legacy_shapes(self):
        schemas = {
            "hashline": {"Edit": {"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"], "additionalProperties": False}},
            "replace": {"Edit": {"type": "object", "properties": {"path": {"type": "string"}, "edits": {"type": "array", "minItems": 1, "items": {"type": "object", "properties": {"old_text": {"type": "string"}, "new_text": {"type": "string"}}, "required": ["old_text", "new_text"], "additionalProperties": False}}}, "required": ["path", "edits"], "additionalProperties": False}},
        }
        good = '```json\n{"name":"Edit","arguments":{"input":"[file#TAG]"}}\n```'
        self.assertEqual(check(good, schemas), 1)
        for arguments in ['{"path":"file","patch":"legacy"}', '{"input":2}', '{"input":"ok","extra":true}']:
            with self.assertRaises(AssertionError):
                check('```json\n{"name":"Edit","arguments":' + arguments + '}\n```', schemas)
        prefix = '## `Edit`\n### Replace\n```json\n'
        self.assertEqual(check(prefix + '{"path":"file","edits":[{"old_text":"a","new_text":"b"}]}\n```', schemas), 1)
        with self.assertRaises(AssertionError):
            check(prefix + '{"path":"file","edits":[]}\n```', schemas)


if __name__ == "__main__":
    unittest.main()
