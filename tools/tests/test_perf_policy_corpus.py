import hashlib
import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

PATH = Path(__file__).resolve().parents[1] / "perf" / "policy_corpus.py"
SPEC = importlib.util.spec_from_file_location("policy_corpus", PATH)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class PolicyCorpusTests(unittest.TestCase):
    def test_exact_bytes_and_hashes_are_deterministic(self):
        first, second = MODULE.payloads(), MODULE.payloads()
        self.assertEqual(first, second)
        self.assertEqual(len(first), 2)
        for data in first.values():
            self.assertEqual(len(data), 128 * 1024)
            self.assertEqual(set(data[:1024]), set(range(256)))
        self.assertEqual(first["period-256.bin"], bytes(range(256)) * 512)
        with tempfile.TemporaryDirectory() as directory:
            rows = MODULE.generate(directory)
            for row in rows:
                data = (Path(directory) / row["name"]).read_bytes()
                self.assertEqual(row["bytes"], len(data))
                self.assertEqual(row["sha256"], hashlib.sha256(data).hexdigest())

    def test_matches_independent_rust_vectors(self):
        expected = {'period-256.bin': '59f410ae5e17962412e2aed4f815918f634932f2abf084f00bb638c4db017850', 'prefix-repeated-tail.bin': '2f7b6cf057aef9521fb8b2424db06b317197c4761221ebca6e4c367d7c47101c'}
        for name, data in MODULE.payloads().items():
            self.assertEqual(hashlib.sha256(data).hexdigest(), expected[name])

    def test_exclusive_create_rejects_raced_existing_file(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "period-256.bin"
            target.write_bytes(b"protected")
            target.unlink()
            real_open = MODULE.os.open
            def raced_open(name, flags, *args, **kwargs):
                if name == "period-256.bin" and flags & MODULE.os.O_CREAT:
                    target.write_bytes(b"protected")
                return real_open(name, flags, *args, **kwargs)
            with patch.object(MODULE.os, "open", side_effect=raced_open):
                with self.assertRaises(FileExistsError):
                    MODULE.generate(directory)
            self.assertEqual(target.read_bytes(), b"protected")

    def test_dangling_file_symlink_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            output = base / "inputs"
            output.mkdir()
            destination = base / "outside"
            (output / "period-256.bin").symlink_to(destination)
            with self.assertRaises(FileExistsError):
                MODULE.generate(output)
            self.assertFalse(destination.exists())

    def test_existing_manifest_is_rejected_before_writes(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "policy-manifest.json"
            target.write_text("protected")
            with self.assertRaises(FileExistsError):
                MODULE.generate(directory)
            self.assertEqual(target.read_text(), "protected")
            self.assertFalse((Path(directory) / "period-256.bin").exists())

    def test_symlink_output_directory_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            destination = base / "outside"
            destination.mkdir()
            output = base / "inputs"
            output.symlink_to(destination, target_is_directory=True)
            with self.assertRaises(FileExistsError):
                MODULE.generate(output)
            self.assertEqual(list(destination.iterdir()), [])

    def test_existing_inputs_are_not_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            MODULE.generate(directory)
            saved = (Path(directory) / "period-256.bin").read_bytes()
            with self.assertRaises(FileExistsError):
                MODULE.generate(directory)
            self.assertEqual((Path(directory) / "period-256.bin").read_bytes(), saved)


if __name__ == "__main__":
    unittest.main()
