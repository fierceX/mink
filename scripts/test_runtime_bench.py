"""Exercise benchmark workloads and failure reporting, without a model provider."""
import json
import hashlib
import os
from pathlib import Path
import resource
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


class RuntimeBenchTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        build = subprocess.run(
            ['cargo', 'bench', '-p', 'mink-core', '--bench', 'runtime_bench',
             '--no-run', '--message-format=json'],
            cwd=ROOT, text=True, stdout=subprocess.PIPE, check=True,
        )
        artifacts = [json.loads(line) for line in build.stdout.splitlines() if line.startswith('{')]
        cls.binary = next(item['executable'] for item in artifacts
                          if item.get('reason') == 'compiler-artifact'
                          and item.get('target', {}).get('name') == 'runtime_bench'
                          and item.get('executable'))

    def run_bench(self, *args, env=None, preexec_fn=None):
        return subprocess.run([self.binary, *args], cwd=ROOT, env=env,
                              preexec_fn=preexec_fn, text=True,
                              capture_output=True, timeout=90)

    def test_case_catalogue_and_invalid_arguments(self):
        listed = self.run_bench('--list')
        self.assertEqual(listed.returncode, 0, listed.stderr)
        self.assertEqual(len(listed.stdout.splitlines()), 25)
        for args, diagnostic in [(['--reps', '0'], '--reps must be greater than zero'),
                                 (['--case', 'missing-case'], 'no case matched')]:
            result = self.run_bench(*args)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(diagnostic, result.stderr)

    def test_mock_workloads_complete(self):
        cases = ['process_start', 'runtime_start', 'mock_turn_1_tool', 'sse_1k',
                 'replay_1k', 'compact_10', 'subagent_fanout_2']
        # process_start also selects process_start_cli by prefix; force an explicit skip.
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / 'report.json'
            env = dict(os.environ, MINK_BIN=str(Path(directory) / 'missing-cli'))
            result = self.run_bench('--quick', '--case', ','.join(cases), '--reps', '2', '--json',
                                    str(report), env=env)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            data = json.loads(report.read_text())
            self.assertEqual(data['failures'], [])
            self.assertEqual(data['skipped_cases'], ['process_start_cli'])
            self.assertEqual({sample['case'] for sample in data['samples']}, set(cases))
            fanout = next(sample for sample in data['samples'] if sample['case'] == 'subagent_fanout_2')
            self.assertEqual(fanout['extra']['subagent_requests'], 2)
            self.assertEqual(fanout['extra']['tool_errors'], 0)

    def test_failed_case_retains_report_and_exits_unsuccessfully(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / 'failing-cli'
            binary.write_text('#!/bin/sh\nexit 7\n')
            binary.chmod(0o700)
            report = Path(directory) / 'report.json'
            env = dict(os.environ, MINK_BIN=str(binary))
            result = self.run_bench('--case', 'process_start_cli', '--json', str(report), env=env)
            self.assertNotEqual(result.returncode, 0)
            data = json.loads(report.read_text())
            self.assertEqual(data['failures'][0]['case'], 'process_start_cli')
            self.assertEqual(data['samples'], [])

    def test_partial_density_is_reported_as_failure(self):
        def lower_fd_limit():
            _, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
            resource.setrlimit(resource.RLIMIT_NOFILE, (48, hard))

        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / 'report.json'
            result = self.run_bench('--case', 'idle_100', '--json', str(report),
                                    preexec_fn=lower_fd_limit)
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
            data = json.loads(report.read_text())
            self.assertEqual(data['failures'][0]['case'], 'idle_100')
            extra = data['samples'][0]['extra']
            self.assertLess(extra['created'], extra['requested'])
            self.assertIsNotNone(extra['failure'])


class ArchivedBenchReportsTests(unittest.TestCase):
    def test_report_integrity_and_summary(self):
        archive = ROOT / 'docs/development/benchmarks/data/runtime-2026-10-03'
        provenance = json.loads((archive / 'provenance.json').read_text())
        self.assertEqual(len(provenance['files']), 7)
        for entry in provenance['files']:
            raw = (archive / entry['file']).read_bytes()
            self.assertEqual(hashlib.sha256(raw).hexdigest(), entry['archived_sha256'])
            self.assertNotIn(b'/Users/', raw)
            if entry['file'].endswith('.json'):
                report = json.loads(raw)
                self.assertEqual(report['meta']['commit'], provenance['recorded_commit'])
                log = (archive / entry['file']).with_suffix('.txt').read_text()
                self.assertEqual(log.count('[case] '), len(report['samples']))
                for sample in report['samples']:
                    self.assertGreater(sample['n'], 0)
                    self.assertLessEqual(sample['min'], sample['p50'])
                    self.assertLessEqual(sample['p50'], sample['p95'])
                    self.assertLessEqual(sample['p95'], sample['max'])
                    self.assertIn(f"[case] {sample['case']} n={sample['n']} p50={sample['p50']:.2f}ms p95={sample['p95']:.2f}ms", log)
        latest = json.loads((archive / 'runtime-5a86554+dirty-20261003T110243Z.json').read_text())
        summary = (ROOT / 'docs/development/benchmarks/runtime-2026-10-03.md').read_text()
        for sample in latest['samples']:
            self.assertIn(f"| `{sample['case']}` | {sample['n']} | {sample['p50']:.3f} | {sample['p95']:.3f} |", summary)
        smoke = archive.parent / 'runtime-2026-10-04-smoke'
        sources = json.loads((smoke / 'provenance.json').read_text())
        for entry in sources['files']:
            raw = (smoke / entry['file']).read_bytes()
            self.assertEqual(hashlib.sha256(raw).hexdigest(), entry['archived_sha256'])
            self.assertNotIn(b'/Users/', raw)
            if entry['file'].endswith('.json'):
                report = json.loads(raw)
                self.assertEqual(report['failures'], [])
                self.assertEqual(report['skipped_cases'], [])
                for sample in report['samples']:
                    extra = sample['extra']
                    if sample['case'].startswith('subagent_fanout_'):
                        self.assertEqual(extra['subagent_requests'], extra['requested_subagents'])
                        self.assertEqual(extra['tool_errors'], 0)
                    else:
                        self.assertEqual(extra['probe_successes'], extra['probe_requested'])


if __name__ == '__main__':
    unittest.main()
