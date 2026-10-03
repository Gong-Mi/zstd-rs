#!/usr/bin/env python3
"""Execute the actual runner with explicit SYNTHETIC fixture programs.

These tests prove orchestration/identity/failure contracts, not codec performance.
The CI smoke also compiles and executes the real Rust/C measurement program.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
RUNNER = ROOT / "tools/perf/run_ab.py"

PROGRAM = r'''
import json, os, pathlib, sys
args = sys.argv[1:]
if args == ['--identity']:
    print('IDENTITY_JSON')
    raise SystemExit(0)
round_no = int(args[args.index('--round') + 1]) if '--round' in args else 0
side = args[args.index('--side') + 1] if '--side' in args else 'plan'
with open(os.environ['TRACE_PATH'], 'a') as fh:
    fh.write(json.dumps({'side':side, 'round':round_no, 'binary':os.environ.get('PERF_BINARY_SHA256')}) + '\n')
cases = [{'kind':kind,'corpus':'SYNTHETIC-fixture','leg':leg,'bytes':4,'sha256':'f'*64,
          'params':{'iters':1,'ref_level':1}} for kind,leg in
         [('sample','stream'),('sample','known'),('encode','fastest')]]
if '--plan' in args:
    print(json.dumps({'schema':1, 'cases':cases}))
else:
    if os.environ.get('FAIL_SIDE') == side:
        raise SystemExit(7)
    for case in cases:
        print(json.dumps(dict(case, schema=1, round=round_no, side=side,
                              source_sha=os.environ['PERF_SOURCE_SHA'],
                              binary_sha256=os.environ['PERF_BINARY_SHA256'], fixture=True)))
'''


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR"))
        self.root = Path(self.temp.name)
        self.corpus = self.root / "corpus"
        self.corpus.mkdir()
        self.harness = self.root / "harness"
        (self.harness / "src").mkdir(parents=True)
        (self.harness / "Cargo.toml").write_text("synthetic manifest\n")
        (self.harness / "Cargo.lock").write_text("synthetic lock\n")
        (self.harness / "build.rs").write_text("// synthetic build identity fixture\n")
        (self.harness / "src/main.rs").write_text("// synthetic fixture\n")
        self.binary = self.root / "fixture-bin"
        digest = hashlib.sha256()
        for name in ('Cargo.toml', 'build.rs', 'src/main.rs'):
            digest.update(name.encode() + b'\0')
            digest.update((self.harness / name).read_bytes() + b'\0')
        lock_sha = hashlib.sha256((self.harness / 'Cargo.lock').read_bytes()).hexdigest()
        self.head_binary = self.root / 'fixture-head'
        for binary, source in ((self.binary, '1' * 40), (self.head_binary, '2' * 40)):
            identity = json.dumps(dict(source_sha=source, harness_sha256=digest.hexdigest(), lock_sha256=lock_sha))
            binary.write_text("#!" + sys.executable + "\n" + PROGRAM.replace('IDENTITY_JSON', identity))
            binary.chmod(0o755)
        self.trace = self.root / "trace.jsonl"

    def tearDown(self):
        self.temp.cleanup()

    def run_runner(self, extra=(), env=None, output=None):
        command = [sys.executable, str(RUNNER), "--base-bin", str(self.binary),
                   "--head-bin", str(self.head_binary), "--base-source", "1" * 40,
                   "--head-source", "2" * 40, "--repo-dir", str(self.corpus),
                   "--harness-dir", str(self.harness), "--output-dir",
                   str(output or self.root / "result"), "--rounds", "2", "--iters", "1", *extra]
        return subprocess.run(command, env=dict(os.environ, TRACE_PATH=str(self.trace), **(env or {})),
                              capture_output=True, text=True, timeout=30)

    def test_real_runner_interleaves_and_freezes_identity_before_measurement(self):
        result = self.run_runner()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = [json.loads(line) for line in self.trace.read_text().splitlines()]
        self.assertEqual([(x['side'], x['round']) for x in calls],
                         [('plan', 0), ('base', 1), ('head', 1), ('base2', 1),
                          ('base', 2), ('head', 2), ('base2', 2)])
        manifest = json.loads((self.root / "result/manifest.json").read_text())
        self.assertEqual(manifest['builds']['base'], manifest['builds']['base2'])
        self.assertEqual(len(manifest['cases']), 3)
        for side in ('base', 'head', 'base2'):
            rows = [json.loads(line) for line in (self.root / 'result' / (side + '.jsonl')).read_text().splitlines()]
            self.assertEqual(len(rows), 6)
            self.assertEqual({x['round'] for x in rows}, {1, 2})
            self.assertTrue(all(x['binary_sha256'] == manifest['builds'][side]['binary_sha256'] for x in rows))
            self.assertTrue(all(x['fixture'] for x in rows))
        self.assertEqual(manifest['attachments']['profile']['status'], 'NOT_RUN')

    def test_binary_source_mismatch_fails_before_measurement(self):
        result = self.run_runner(extra=['--head-source', '3' * 40])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('source identity', result.stderr)
        if self.trace.exists():
            self.assertFalse(any(json.loads(line)['round'] > 0
                                 for line in self.trace.read_text().splitlines()))

    def test_required_process_failure_stays_nonzero(self):
        result = self.run_runner(env={'FAIL_SIDE':'head'})
        self.assertEqual(result.returncode, 1)
        self.assertIn('FAILED', result.stderr)
        self.assertTrue((self.root / 'result/manifest.json').is_file())

    def test_output_must_not_change_corpus_checkout(self):
        result = self.run_runner(output=self.corpus / 'results')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('outside', result.stderr)

    def test_refuses_old_results_and_short_source_tokens(self):
        output = self.root / 'result'
        output.mkdir()
        result = self.run_runner()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('already exists', result.stderr)
        result = self.run_runner(extra=['--base-source', '123'], output=self.root / 'other')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('full lowercase', result.stderr)

    def test_optional_configuration_is_a_separate_paired_suite(self):
        result = self.run_runner(extra=['--tuned-bin', str(self.head_binary)])
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest = json.loads((self.root / 'result/manifest.json').read_text())
        self.assertEqual(manifest['attachments']['config']['status'], 'COMPLETE')
        config = json.loads((self.root / 'result/config/manifest.json').read_text())
        self.assertEqual({x['source_sha'] for x in config['builds'].values()}, {'2'*40})
        self.assertEqual(config['builds']['base'], config['builds']['base2'])
        self.assertEqual(len((self.root / 'result/config/base.jsonl').read_text().splitlines()), 6)

    def test_optional_missing_configuration_binary_is_visible_nonfatal(self):
        result = self.run_runner(extra=['--tuned-bin', str(self.root / 'missing')])
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest = json.loads((self.root / 'result/manifest.json').read_text())
        self.assertEqual(manifest['attachments']['config']['status'], 'FAILED')
        self.assertTrue(manifest['attachments']['config']['reason'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
