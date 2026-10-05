#!/usr/bin/env python3
"""Offline artifact acquisition contracts; all archives/metadata are fixtures."""
from datetime import datetime, timezone
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import zipfile

ROOT = Path(__file__).resolve().parents[2]
FILE = ROOT / 'tools/perf/fetch_corpus.py'
spec = importlib.util.spec_from_file_location('fetch_corpus', FILE)
fetch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fetch)


class CorpusTests(unittest.TestCase):
    def archive(self, name='SYNTHETIC.bin'):
        stream = io.BytesIO()
        with zipfile.ZipFile(stream, 'w') as archive:
            archive.writestr(name, b'SYNTHETIC INPUT, NOT A PERFORMANCE CORPUS')
        return stream.getvalue()

    def acquire(self, root, archive, metadata=None):
        now = datetime.now(timezone.utc).isoformat().replace('+00:00', 'Z')
        replies = [[{'databaseId':123, 'createdAt':now}],
                   metadata or {'total_count':1, 'artifacts':[{'id':456, 'name':'corpus-cache',
                                                             'expired':False, 'size_in_bytes':len(archive)}]}]
        def download(command, **kwargs):
            self.assertEqual(command[-1], 'repos/FIXTURE/repo/actions/artifacts/456/zip')
            kwargs['stdout'].write(archive)
            return subprocess.CompletedProcess(command, 0)
        with mock.patch.object(fetch, 'gh', side_effect=replies), mock.patch.object(fetch.subprocess, 'run', side_effect=download):
            return fetch.acquire(root, 'FIXTURE/repo')

    def test_disabled_cli_never_calls_network(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as tmp:
            output = Path(tmp) / 'output'
            result = subprocess.run([sys.executable, str(FILE), '--output-dir', str(output), '--enabled', 'false'],
                                    env=dict(os.environ, PATH=''), capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            status = json.loads((output / 'status.json').read_text())
            self.assertEqual(status['status'], 'NOT_RUN')
            self.assertIn('no network', status['reason'])

    def test_exact_artifact_id_and_full_input_hash_recorded(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as tmp:
            root = Path(tmp)
            result = self.acquire(root, self.archive())
            self.assertEqual(result['status'], 'COMPLETE')
            self.assertEqual(result['artifact_id'], 456)
            data = (root / 'SYNTHETIC.bin').read_bytes()
            self.assertEqual(result['files'][0]['sha256'], hashlib.sha256(data).hexdigest())

    def test_missing_artifact_is_explicit_not_run(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as tmp:
            result = self.acquire(Path(tmp), self.archive(), {'total_count':0, 'artifacts':[]})
            self.assertEqual(result['status'], 'NOT_RUN')

    def test_unsafe_zip_and_incomplete_enumeration_rejected(self):
        for name in ('../outside.bin', '/outside.bin', 'nested\\outside.bin'):
            with self.subTest(name=name), tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as tmp:
                with self.assertRaises(ValueError):
                    self.acquire(Path(tmp), self.archive(name))
                self.assertFalse(list(Path(tmp).glob('*.bin')))
        with tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as tmp:
            with self.assertRaises(ValueError):
                self.acquire(Path(tmp), self.archive(), {'total_count':101, 'artifacts':[]})

    def test_network_failure_is_explicit_failed_optional_leg(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as tmp:
            output = Path(tmp) / 'output'
            result = subprocess.run([sys.executable, str(FILE), '--output-dir', str(output), '--enabled', 'true', '--repo', 'FIXTURE/repo'],
                                    env=dict(os.environ, PATH=''), capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            status = json.loads((output / 'status.json').read_text())
            self.assertEqual(status['status'], 'FAILED')
            self.assertIn('::warning::', result.stdout)


if __name__ == '__main__':
    unittest.main(verbosity=2)
