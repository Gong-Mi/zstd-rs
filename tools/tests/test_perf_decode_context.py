import importlib.util
import os
import subprocess
import tempfile
import yaml
from pathlib import Path
from types import SimpleNamespace
import unittest

SPEC = importlib.util.spec_from_file_location("runner", Path(__file__).parents[1] / "perf" / "run_ab.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class DecodeContextTests(unittest.TestCase):
    def args(self):
        return SimpleNamespace(iters=3, size_mb=1, encode_mb=1, repo_dir="repo", corpus_file=[])

    def test_existing_callers_keep_mixed_default(self):
        self.assertNotIn("--decode-only", MODULE.argv(self.args()))

    def test_decode_only_is_forwarded_once(self):
        args = self.args()
        args.decode_only = True
        self.assertEqual(MODULE.argv(args).count("--decode-only"), 1)

    def test_successful_config_is_not_forwarded_to_decode_only(self):
        workflow = yaml.safe_load((Path(__file__).parents[2] / ".github/workflows/perf.yml").read_text())
        command = next(s["run"] for s in workflow["jobs"]["ab"]["steps"]
                       if s["name"] == "Run immutable-plan interleaved suites")
        with tempfile.TemporaryDirectory() as directory:
            p = Path(directory)
            (p / "bins").mkdir()
            # Argument-construction fixture only: this file is never executed.
            tuned = p / "bins/head_tuned"
            tuned.write_text("configuration argument fixture")
            tuned.chmod(0o755)
            shell = "python3(){ printf '%s\t' \"$@\"; printf '\n'; }; git(){ printf '%040d' 0; };\n" + command
            result = subprocess.run(["bash", "-c", shell], cwd=p,
                                    env=dict(os.environ, GITHUB_WORKSPACE=str(p),
                                             REPS="1", ITERS="1", SIZE="1", ENCODE="1"),
                                    capture_output=True, text=True, check=True)
            calls = [line.split("\t") for line in result.stdout.splitlines()]
            self.assertEqual(len(calls), 2)
            mixed, isolated = calls
            self.assertIn("--tuned-bin", mixed)
            self.assertIn("--decode-only", isolated)
            self.assertNotIn("--tuned-bin", isolated)
            self.assertNotIn("--config-failed", isolated)

    def test_explicit_mixed_remains_mixed(self):
        args = self.args()
        args.decode_only = False
        self.assertNotIn("--decode-only", MODULE.argv(args))


if __name__ == "__main__":
    unittest.main()
