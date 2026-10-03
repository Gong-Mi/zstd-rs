#!/usr/bin/env python3
"""Static workflow wiring plus executable ref-resolution contracts, no CI claim."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]


def workflow(name):
    data = yaml.safe_load((ROOT / '.github/workflows' / name).read_text())
    # PyYAML YAML-1.1 interprets unquoted on as True; GitHub uses YAML-1.2.
    if True in data:
        data['on'] = data.pop(True)
    return data


class WorkflowTests(unittest.TestCase):
    def test_every_route_uses_same_locked_harness_runner_and_compare(self):
        data = workflow('perf.yml')
        self.assertEqual(set(data['on']), {'push', 'pull_request', 'workflow_dispatch'})
        steps = data['jobs']['ab']['steps']
        scripts = '\n'.join(s.get('run', '') for s in steps)
        self.assertIn('tools/perf/run_ab.py', scripts)
        self.assertIn('tools/perf/compare.py', scripts)
        self.assertNotIn('gap_vs_c', scripts)
        self.assertNotIn('get(\'leg\',\'stream\')', scripts)
        self.assertNotIn('100% 落在', scripts)
        self.assertEqual(scripts.count('cargo build --locked --release'), 3)
        self.assertIn('cp -R tooling/tools/perf/harness/.', scripts)
        self.assertIn('--repo-dir "$GITHUB_WORKSPACE/head"', scripts)
        self.assertIn('--output-dir "$GITHUB_WORKSPACE/perf-results"', scripts)
        self.assertIn('exit "$status"', scripts)
        upload = next(s for s in steps if s.get('uses', '').startswith('actions/upload-artifact'))
        self.assertEqual(upload['if'], 'always()')
        self.assertIn('build-records/', upload['with']['path'])
        self.assertEqual(data['on']['workflow_dispatch']['inputs']['fail_on_regression']['default'], False)

    def test_retains_full_production_matrix_and_adds_real_direct_miri_target(self):
        data = workflow('ci.yml')
        scripts = '\n'.join(s.get('run', '') for j in data['jobs'].values() for s in j['steps'])
        for command in ('cargo hack check --workspace --feature-powerset',
                        'cargo hack clippy --workspace --feature-powerset',
                        'cargo hack test --workspace --feature-powerset',
                        'cargo msrv verify --path cli/', 'cargo msrv verify --path ruzstd/',
                        'cargo +nightly miri test ringbuffer',
                        'miri test -p ruzstd --lib decoding::decode_buffer::tests::short_writer -- --exact',
                        'miri test -p ruzstd --test direct_output_miri'):
            self.assertIn(command, scripts)
        self.assertNotIn('miri test short_Writer', scripts)
        self.assertIn('test result: ok. 1 passed;', scripts)

    def test_miri_count_gate_rejects_zero_discoveries(self):
        steps = workflow('ci.yml')['jobs']['nightly-stuff']['steps']
        for title, filename, expected in (
                ('Miri short writer exact test (zero discoveries fail)', 'short-writer-miri.log', 1),
                ('Miri direct output API (real buffers, no C FFI)', 'direct-output-miri.log', 10)):
            script = next(s['run'] for s in steps if s.get('name') == title)
            guard = re.search(r'python3 -c "([^\n]*)"', script).group(1)
            for count in (0, expected):
                with self.subTest(title=title, count=count), tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as tmp:
                    Path(tmp, filename).write_text(f'test result: ok. {count} passed; 0 failed;\n')
                    result = subprocess.run([sys.executable, '-c', guard], cwd=tmp,
                                            capture_output=True, text=True, timeout=10)
                    self.assertEqual(result.returncode, 0 if count == expected else 1)

    def test_contract_ci_executes_python_and_real_rust_tests(self):
        data = workflow('perf-contract.yml')
        scripts = '\n'.join(s.get('run', '') for j in data['jobs'].values() for s in j['steps'])
        self.assertIn("unittest discover -s tools/tests -p 'test_perf_*.py'", scripts)
        self.assertIn('cargo test --locked --manifest-path tools/perf/harness/Cargo.toml', scripts)
        self.assertIn('cargo test -p ruzstd --features dict_builder --test cross_validate --test direct_output_miri', scripts)
        self.assertIn('git diff --exit-code', scripts)

    def test_resolve_refs_executes_with_smoke_or_manual_bounds(self):
        step = next(s for s in workflow('perf.yml')['jobs']['ab']['steps']
                    if s.get('name') == 'Resolve requested refs and bounded experiment')
        source = re.search(r"python3 - <<'PY'\n(.*?)\nPY", step['run'], re.S).group(1)
        for event in ('push', 'pull_request', 'workflow_dispatch'):
            with self.subTest(event=event), tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as tmp:
                output = Path(tmp) / 'output'
                env = dict(os.environ, EVENT=event, PR_BASE='1'*40, PR_HEAD='2'*40,
                           INPUT_BASE='literal-base', INPUT_HEAD='literal-head', INPUT_REPS='',
                           INPUT_ITERS='', INPUT_SIZE='', INPUT_ENCODE='', GITHUB_SHA='3'*40,
                           GITHUB_OUTPUT=str(output))
                result = subprocess.run([sys.executable, '-'], input=source, env=env,
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                values = dict(line.split('=', 1) for line in output.read_text().splitlines())
                self.assertEqual(values['base'], '1'*40 if event == 'pull_request' else 'literal-base')
                self.assertEqual(values['head'], '2'*40 if event == 'pull_request' else 'literal-head')
                self.assertEqual(values['size'], '16' if event == 'workflow_dispatch' else '1')
                env['INPUT_REPS'] = '0'
                result = subprocess.run([sys.executable, '-'], input=source, env=env,
                                        capture_output=True, text=True, timeout=10)
                self.assertNotEqual(result.returncode, 0)

    def test_all_real_run_blocks_have_valid_bash_syntax(self):
        for filename in ('ci.yml', 'perf.yml', 'perf-contract.yml'):
            for job in workflow(filename)['jobs'].values():
                for step in job['steps']:
                    if 'run' not in step:
                        continue
                    script = re.sub(r'\$\{\{.*?\}\}', 'fixture', step['run'], flags=re.S)
                    result = subprocess.run(['bash', '-n'], input=script, text=True,
                                            capture_output=True, timeout=10)
                    self.assertEqual(result.returncode, 0, filename + result.stderr)


if __name__ == '__main__':
    unittest.main(verbosity=2)
