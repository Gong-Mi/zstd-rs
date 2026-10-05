#!/usr/bin/env python3
"""Synthetic completion-ledger faults; not codec measurements."""
import copy
import json
import unittest
import test_perf_contract as fixtures


def execution_fixture():
    manifest, sides = fixtures.fixture(rounds=3)
    order = [('base', 'head', 'base2'), ('head', 'base2', 'base'), ('base2', 'base', 'head')]
    schedule, ledger = [], []
    for number, names in enumerate(order, 1):
        for side in names:
            item = {'ordinal': len(schedule) + 1, 'round': number, 'side': side}
            schedule.append(item)
            ledger.append(dict(item, affinity=[0], **manifest['builds'][side]))
    manifest['execution'] = {'protocol': 'rotating-aba-v1', 'affinity': [0], 'schedule': schedule}
    return manifest, sides, ledger


def raw_ledger(rows):
    return {'execution.jsonl': ''.join(json.dumps(row) + '\n' for row in rows)}


class T0ExecutionTests(unittest.TestCase):
    invoke = fixtures.PerfContractRegression.invoke
    assert_incomplete = fixtures.PerfContractRegression.assert_incomplete

    def test_valid_ledger_is_complete_not_performance_approval(self):
        manifest, sides, rows = execution_fixture()
        result, report, _ = self.invoke(manifest, sides, raw=raw_ledger(rows))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report['data_status'], 'COMPLETE')
        self.assertFalse(report['accepted'])
        self.assertEqual(report['provenance']['execution']['affinity'], [0])

    def test_required_completion_ledger_cannot_be_missing_or_empty(self):
        for raw in (None, raw_ledger([])):
            manifest, sides, _ = execution_fixture()
            self.assert_incomplete(manifest, sides, raw=raw)

    def test_required_protocol_cannot_be_downgraded_to_legacy(self):
        manifest, sides, rows = execution_fixture()
        manifest.pop('execution')
        self.assert_incomplete(manifest, sides, raw=raw_ledger(rows), flags=('--require-execution',))

    def test_incorrect_completions_fail_closed(self):
        for fault in ('drop', 'extra', 'order', 'affinity', 'affinity-bool', 'source', 'binary', 'ordinal-bool'):
            with self.subTest(fault=fault):
                manifest, sides, rows = execution_fixture()
                if fault == 'drop': rows.pop()
                elif fault == 'extra': rows.append(copy.deepcopy(rows[-1]))
                elif fault == 'order': rows[0], rows[1] = rows[1], rows[0]
                elif fault == 'affinity': rows[0]['affinity'] = [0, 1]
                elif fault == 'affinity-bool': rows[0]['affinity'] = [False]
                elif fault == 'source': rows[0]['source_sha'] = '9' * 40
                elif fault == 'binary': rows[0]['binary_sha256'] = '9' * 64
                else: rows[0]['ordinal'] = True
                self.assert_incomplete(manifest, sides, raw=raw_ledger(rows))

    def test_duplicate_completion_fields_are_rejected(self):
        manifest, sides, rows = execution_fixture()
        raw = raw_ledger(rows)
        raw['execution.jsonl'] = raw['execution.jsonl'].replace(
            '"ordinal": 1', '"ordinal": 99, "ordinal": 1', 1)
        self.assert_incomplete(manifest, sides, raw=raw)

    def test_declared_schedule_and_cpu_are_strict(self):
        for fault in ('protocol', 'schedule', 'schedule-bool', 'cpu', 'cpu-bool'):
            with self.subTest(fault=fault):
                manifest, sides, rows = execution_fixture()
                if fault == 'protocol': manifest['execution']['protocol'] = 'unknown'
                elif fault == 'schedule': manifest['execution']['schedule'].pop()
                elif fault == 'schedule-bool': manifest['execution']['schedule'][0]['ordinal'] = True
                elif fault == 'cpu': manifest['execution']['affinity'] = []
                else: manifest['execution']['affinity'] = [False]
                self.assert_incomplete(manifest, sides, raw=raw_ledger(rows))


if __name__ == '__main__':
    unittest.main(verbosity=2)
