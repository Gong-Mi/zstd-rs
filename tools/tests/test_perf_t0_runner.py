#!/usr/bin/env python3
"""T0 regressions reuse the existing synthetic runner fixture, not timing evidence."""
import json
import os
import unittest
from pathlib import Path
import test_perf_runner as fixtures


class T0RunnerTests(unittest.TestCase):
    root: Path
    trace: Path
    binary: Path

    setUp = fixtures.RunnerTests.setUp
    tearDown = fixtures.RunnerTests.tearDown
    run_runner = fixtures.RunnerTests.run_runner

    def test_outer_order_rotates_and_declares_single_core(self):
        result = self.run_runner(extra=['--rounds', '3'])
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = [json.loads(x) for x in self.trace.read_text().splitlines()]
        actual = [(x['side'], x['round']) for x in calls if x['round'] > 0]
        self.assertEqual(actual, [('base', 1), ('head', 1), ('base2', 1),
                                  ('head', 2), ('base2', 2), ('base', 2),
                                  ('base2', 3), ('base', 3), ('head', 3)])
        manifest = json.loads((self.root/'result/manifest.json').read_text())
        self.assertEqual(manifest['execution']['protocol'], 'rotating-aba-v1')
        self.assertEqual(len(manifest['execution']['affinity']), 1)
        self.assertTrue(all(x['affinity'] == manifest['execution']['affinity'] for x in calls))
        rows = [json.loads(x) for x in (self.root/'result/execution.jsonl').read_text().splitlines()]
        self.assertEqual([(x['side'], x['round']) for x in rows], actual)
        self.assertTrue(all(x['affinity'] == manifest['execution']['affinity'] for x in rows))

    def test_cpu_outside_current_affinity_is_rejected(self):
        self.assertTrue(hasattr(os, 'sched_getaffinity'))
        unavailable = max(os.sched_getaffinity(0)) + 100
        result = self.run_runner(extra=['--cpu', str(unavailable)])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('affinity', result.stderr)

    def test_base_plan_mismatch_rejected_before_measurement(self):
        text = self.binary.read_text()
        text = text.replace("'bytes':4", "'bytes':5")
        self.binary.write_text(text)
        result = self.run_runner()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('plan mismatch', result.stderr)
        calls = [json.loads(x) for x in self.trace.read_text().splitlines()]
        self.assertFalse(any(x['round'] > 0 for x in calls))


if __name__ == '__main__':
    unittest.main(verbosity=2)
