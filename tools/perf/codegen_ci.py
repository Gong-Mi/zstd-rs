#!/usr/bin/env python3
"""Bounded normal-link codegen and three independent evidence suites.

Reuses the locked harness/run_ab/compare. Calibration is never gain approval.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

from codegen_capture import diagnostic_env

ROOT = Path(__file__).resolve().parents[2]
PERF = ROOT / 'tools/perf'


def execute(command, *, cwd=None, env=None, log=None):
    if log is None:
        return subprocess.run([str(x) for x in command], cwd=cwd, env=env,
                              text=True, capture_output=True, check=True).stdout
    with Path(log).open('w') as output:
        subprocess.run([str(x) for x in command], cwd=cwd, env=env,
                       stdout=output, stderr=subprocess.STDOUT, check=True)
    return ''


def bounded(value):
    number = int(value)
    if not 1 <= number <= 64:
        raise argparse.ArgumentTypeError('bounded experiment requires 1..64')
    return number


def calibration_status(name, report):
    if name == 'candidate-pr50':
        return None
    if report.get('data_status') != 'COMPLETE':
        return 'INCOMPLETE'
    states = [case['performance_status'] for case in report.get('cases', [])]
    return ('UNRESOLVED' if any(s in ('GAIN', 'REGRESSION', 'TRADEOFF', 'UNRESOLVED') for s in states)
            else 'NO_SIGNAL_IN_THIS_BUDGET')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--base-sha', required=True)
    parser.add_argument('--candidate-sha', required=True)
    parser.add_argument('--cc', required=True)
    parser.add_argument('--llvm-suffix', default='')
    parser.add_argument('--rounds', type=bounded, default=6)
    parser.add_argument('--iters', type=bounded, default=7)
    parser.add_argument('--size-mb', type=bounded, default=16)
    parser.add_argument('--encode-mb', type=bounded, default=4)
    args = parser.parse_args()
    for value in (args.base_sha, args.candidate_sha):
        if re.fullmatch('[a-f0-9]{40}', value) is None:
            parser.error('freeze full lowercase source SHAs, not branch names')
    if args.work.exists():
        parser.error('fresh work directory required')
    work = args.work.resolve(); work.mkdir(parents=True)
    evidence = work / 'evidence'; evidence.mkdir()
    source_manifest = {
        'schema': 1, 'tooling_sha': execute(['git', 'rev-parse', 'HEAD'], cwd=ROOT).strip(),
        'base_sha': args.base_sha, 'candidate_sha': args.candidate_sha,
        'rounds': args.rounds, 'iters': args.iters, 'size_mb': args.size_mb,
        'encode_mb': args.encode_mb, 'scope': 'diagnostic codegen/calibration; no candidate acceptance',
        'suites': {}}
    (evidence / 'experiment.json').write_text(json.dumps(source_manifest, indent=2) + '\n')
    builds = {}
    for label, sha in (('baseA', args.base_sha), ('baseB', args.base_sha),
                       ('candidate', args.candidate_sha)):
        subject = work / 'subjects' / label
        execute(['git', 'worktree', 'add', '--detach', subject, sha], cwd=ROOT)
        harness = subject / 'tools/perf/harness'
        if harness.exists():
            raise ValueError('subject already carries a harness; inspect before replacing it')
        shutil.copytree(PERF / 'harness', harness)
        target = work / 'targets' / label
        env = diagnostic_env(os.environ)
        env.update(CC=args.cc, CXX=args.cc.replace('clang', 'clang++'),
                   CARGO_BUILD_JOBS='4', CARGO_TARGET_DIR=str(target))
        log = evidence / (label + '-build.log')
        execute(['cargo', 'build', '--locked', '--release', '-vv', '--manifest-path', harness / 'Cargo.toml'],
                cwd=subject, env=env, log=log)
        capture = evidence / 'captures' / label
        command = [sys.executable, '-B', PERF / 'codegen_capture.py', '--subject', subject,
                   '--target', target, '--harness', harness, '--output', capture,
                   '--build-log', log, '--cc', args.cc]
        for flag, tool in (('dis', 'llvm-dis'), ('opt', 'opt'), ('nm', 'llvm-nm'),
                           ('objdump', 'llvm-objdump'), ('cxxfilt', 'llvm-cxxfilt')):
            command.extend(['--' + flag, tool + args.llvm_suffix])
        execute(command, env=env, log=evidence / (label + '-capture.log'))
        builds[label] = {'source': sha, 'binary': capture / 'binary', 'subject': subject,
                         'capture': capture}
        print('captured', label, sha, flush=True)
    policy = work / 'policy'
    execute([sys.executable, '-B', PERF / 'policy_corpus.py', '--output-dir', policy])
    suites = [('same-binary-aa', 'baseA', 'baseA'),
              ('independent-build-aa', 'baseA', 'baseB'),
              ('candidate-pr50', 'baseA', 'candidate')]
    incomplete = False
    for name, left, right in suites:
        suite = evidence / 'suites' / name
        before, after = builds[left], builds[right]
        command = [sys.executable, '-B', PERF / 'run_ab.py', '--base-bin', before['binary'],
                   '--head-bin', after['binary'], '--base-source', before['source'],
                   '--head-source', after['source'], '--harness-dir', PERF / 'harness',
                   '--repo-dir', builds['baseA']['subject'], '--output-dir', suite,
                   '--rounds', str(args.rounds), '--iters', str(args.iters),
                   '--size-mb', str(args.size_mb), '--encode-mb', str(args.encode_mb),
                   '--timeout', '100', '--corpus-file', policy / 'period-256.bin',
                   '--corpus-file', policy / 'prefix-repeated-tail.bin']
        result = subprocess.run([str(x) for x in command], capture_output=True, text=True)
        (evidence / (name + '-runner.log')).write_text(result.stdout + result.stderr)
        suite.mkdir(parents=True, exist_ok=True)
        compare = [sys.executable, '-B', PERF / 'compare.py', '--manifest', suite / 'manifest.json',
                   '--data-dir', suite, '--output', suite / 'report.json',
                   '--summary', suite / 'report.md', '--require-execution']
        comparison = subprocess.run([str(x) for x in compare], capture_output=True, text=True)
        (evidence / (name + '-compare.log')).write_text(comparison.stdout + comparison.stderr)
        report = json.loads((suite / 'report.json').read_text()) if (suite / 'report.json').exists() else {}
        completed = result.returncode == comparison.returncode == 0 and report.get('data_status') == 'COMPLETE'
        incomplete |= not completed
        calibration = calibration_status(name, report)
        source_manifest['suites'][name] = {
            'data_status': report.get('data_status', 'INCOMPLETE'),
            'performance_status': report.get('performance_status', 'NOT_EVALUATED'),
            'original_report_accepted': report.get('accepted', False),
            'calibration_status': calibration, 'optimization_approved': False,
            'runner_exit': result.returncode, 'compare_exit': comparison.returncode}
        (evidence / 'experiment.json').write_text(json.dumps(source_manifest, indent=2) + '\n')
        print(name, source_manifest['suites'][name], flush=True)
    if incomplete:
        raise SystemExit('required suite incomplete; inspect preserved raw artifacts')


if __name__ == '__main__':
    main()
