#!/usr/bin/env python3
"""Regression-first scope commit: synthetic fixtures, never performance evidence.

The current workflow must reject a required encoding leg that only head emitted.
This test intentionally starts RED; implementation follows on the same Draft PR.
"""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]


class PerfContractRegression(unittest.TestCase):
    def test_head_only_encoding_cannot_be_complete(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/perf.yml").read_text())
        step = next(step for step in workflow["jobs"]["ab"]["steps"]
                    if step.get("name") == "Compare")
        source = re.search(r"^python3 - <<'PY'\n(.*?)^PY\s*$", step["run"], re.M | re.S).group(1)
        sample = {"kind": "sample", "corpus": "synthetic", "leg": "stream",
                  "bytes": 4096, "params": {"ref_level": 1, "iters": 1},
                  "ruzstd_ms": 10.0, "ruzstd_cpu_ms": 8.0,
                  "c_ms": 2.0, "c_cpu_ms": 1.5}
        encode = dict(sample, kind="encode", leg="fastest", c_ratio=4.0, ruzstd_ratio=2.5)
        with tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR")) as directory:
            root = Path(directory)
            for side in ("base", "head", "base2"):
                rows = [sample] + ([encode] if side == "head" else [])
                (root / (side + ".jsonl")).write_text("".join(json.dumps(row) + "\n" for row in rows))
            env = dict(os.environ, GITHUB_WORKSPACE=str(root),
                       GITHUB_STEP_SUMMARY=str(root / "summary.md"), REPS="1", ITERS="1")
            result = subprocess.run([sys.executable, "-"], input=source, cwd=root,
                                    env=env, text=True, capture_output=True, timeout=30)
        self.assertNotEqual(result.returncode, 0,
                            "required head-only encoding was silently accepted:\n" + result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
