#!/usr/bin/env python3
"""Synthetic completion-ledger faults; not codec measurements."""
import copy
import json
import unittest
import test_perf_contract as fixtures


def declare_execution(manifest):
    order = [('base', 'head', 'base2'), ('head', 'base2', 'base'), ('base2', 'base', 'head')]
    schedule, ledger = [], []
    for number in range(1, manifest['rounds'] + 1):
        for side in order[(number - 1) % len(order)]:
            item = {'ordinal': len(schedule) + 1, 'round': number, 'side': side}
            schedule.append(item)
            ledger.append(dict(item, affinity=[0], **manifest['builds'][side]))
    manifest['execution'] = {'protocol': 'rotating-aba-v1', 'affinity': [0], 'schedule': schedule}
    return ledger


def execution_fixture():
    manifest, sides = fixtures.fixture(rounds=3)
    return manifest, sides, declare_execution(manifest)


def raw_ledger(rows, probe=None):
    lines = [json.dumps(row) for row in rows]
    if probe is not None:
        lines[0] = lines[0][:-1] + ',' + probe + '}'
    return {'execution.jsonl': ''.join(line + '\n' for line in lines)}


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

    def test_nonfinite_and_unencodable_completion_fields_are_rejected(self):
        for probe in ('"probe":1e999', '"probe":NaN', '"probe":Infinity',
                      '"probe":"\\ud800"', '"\\ud800":0'):
            with self.subTest(probe=probe):
                manifest, sides, rows = execution_fixture()
                self.assert_incomplete(manifest, sides, raw=raw_ledger(rows, probe))

    def test_deep_completion_preserves_structured_error_reports(self):
        manifest, sides, rows = execution_fixture()
        probe = '"probe":' + '[' * 16000 + '0' + ']' * 16000
        self.assert_incomplete(manifest, sides, raw=raw_ledger(rows, probe))

    def test_duplicate_surrogate_key_preserves_utf8_error_reports(self):
        manifest, sides, rows = execution_fixture()
        self.assert_incomplete(manifest, sides,
                               raw=raw_ledger(rows, '"\\ud800":0,"\\ud800":1'))

    def test_optional_completion_parse_faults_do_not_destroy_primary_reports(self):
        probes = ('"probe":1e999', '"probe":"\\ud800"',
                  '"\\ud800":0,"\\ud800":1',
                  '"probe":' + '[' * 16000 + '0' + ']' * 16000)
        for index, probe in enumerate(probes):
            with self.subTest(fault=index):
                manifest, sides, rows = execution_fixture()
                config, config_sides, _ = fixtures.config_fixture(manifest, rounds=3)
                config_rows = declare_execution(config)
                manifest['attachments']['config'] = {
                    'status': 'COMPLETE', 'reason': 'independent suite', 'suite': 'config'}
                raw = raw_ledger(rows)
                raw.update(fixtures.suite_raw(config, config_sides))
                raw['config/execution.jsonl'] = raw_ledger(config_rows, probe)['execution.jsonl']
                result, report, summary = self.invoke(
                    manifest, sides, raw=raw, flags=('--require-execution',))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn('Traceback', result.stderr)
                self.assertEqual(report['data_status'], 'COMPLETE')
                self.assertEqual(report['errors'], [])
                attachment = report['attachments']['config']
                self.assertEqual(attachment['status'], 'FAILED')
                self.assertEqual(attachment['report']['data_status'], 'INCOMPLETE')
                self.assertFalse(attachment['report']['accepted'])
                self.assertTrue(attachment['errors'])
                self.assertTrue(report['warnings'])
                self.assertIn('FAILED', summary)

    def test_valid_optional_completion_and_utf8_extensions_remain_complete(self):
        manifest, sides, rows = execution_fixture()
        config, config_sides, _ = fixtures.config_fixture(manifest, rounds=3)
        config_rows = declare_execution(config)
        manifest['attachments']['config'] = {
            'status': 'COMPLETE', 'reason': 'independent suite', 'suite': 'config'}
        probe = '"probe":{"说明":[null,true,1.25,"完成"]}'
        raw = raw_ledger(rows, probe)
        raw.update(fixtures.suite_raw(config, config_sides))
        raw['config/execution.jsonl'] = raw_ledger(config_rows, probe)['execution.jsonl']
        result, report, _ = self.invoke(manifest, sides, raw=raw, flags=('--require-execution',))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report['data_status'], 'COMPLETE')
        self.assertEqual(report['attachments']['config']['status'], 'COMPLETE')
        self.assertEqual(report['attachments']['config']['report']['data_status'], 'COMPLETE')
        self.assertEqual(report['warnings'], [])

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
