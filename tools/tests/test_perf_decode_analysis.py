#!/usr/bin/env python3
"""Offline fixture tests only; not observed performance measurements."""
import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parents[1] / "perf"))
import analyze_decode_only as diagnostic
import compare as core
from test_perf_contract import fixture


class DecodeDiagnosticTests(unittest.TestCase):
    def context(self):
        mixed, rows = fixture()
        mixed["builds"]["head"]["source_sha"] = mixed["builds"]["base"]["source_sha"]
        for case in mixed["cases"]:
            case["params"]["experiment_context"] = "mixed/decode-encode"
        isolated = copy.deepcopy(mixed)
        isolated["cases"] = [c for c in isolated["cases"] if c["kind"] == "sample"]
        for case in isolated["cases"]:
            case["params"]["experiment_context"] = "decode-only/no-rust-encoding"
        for side, mode in (("base", 0), ("head", 1), ("base2", 0)):
            rows[side] = [r for r in rows[side] if r["kind"] == "sample"]
            for row in rows[side]:
                row["source_sha"] = mixed["builds"][side]["source_sha"]
                row["params"]["experiment_context"] = "decode-only/no-rust-encoding"
                row["diagnostic_mode"] = mode
                row["rust_encode_calls_so_far"] = 0
        return mixed, isolated, rows

    def run_case(self, mutate=None, default=False):
        mixed, isolated, rows = self.context()
        if mutate:
            mutate(mixed, isolated, rows)
        with tempfile.TemporaryDirectory() as directory:
            p = Path(directory)
            (p / "mixed.json").write_text(json.dumps(mixed))
            (p / "manifest.json").write_text(json.dumps(isolated))
            for side, data in rows.items():
                (p / (side + ".jsonl")).write_text("".join(json.dumps(r) + "\n" for r in data))
            if default:
                return core.compare(p / "manifest.json", p)
            return diagnostic.analyze(p / "manifest.json", p / "mixed.json", p)

    def test_diagnostic_complete_never_approves(self):
        result = self.run_case()
        self.assertEqual(result["data_status"], "COMPLETE")
        self.assertTrue(result["diagnostic_only"])
        self.assertFalse(result["accepted"])

    def test_production_gate_still_rejects_missing_encoding(self):
        result = self.run_case(default=True)
        self.assertEqual(result["data_status"], "INCOMPLETE")
        self.assertIn("manifest: required encode case is absent", result["errors"])

    def test_nonzero_encoding_counter_rejected(self):
        def mutate(m, i, r):
            r["head"][0]["rust_encode_calls_so_far"] = 1
        self.assertEqual(self.run_case(mutate)["data_status"], "INCOMPLETE")

    def test_wrong_mode_rejected(self):
        def mutate(m, i, r):
            r["base"][0]["diagnostic_mode"] = 1
        self.assertEqual(self.run_case(mutate)["data_status"], "INCOMPLETE")

    def test_identity_change_between_contexts_rejected(self):
        def mutate(m, i, r):
            m["builds"]["head"]["binary_sha256"] = "9" * 64
        self.assertEqual(self.run_case(mutate)["data_status"], "INCOMPLETE")

    def test_changed_iteration_count_between_contexts_rejected(self):
        def mutate(m, i, r):
            for case in i["cases"]:
                case["params"]["iters"] = 1
            for data in r.values():
                for row in data:
                    row["params"]["iters"] = 1
                    row["impl_samples"] = row["impl_samples"][:1]
                    row["ref_samples"] = row["ref_samples"][:1]
        self.assertEqual(self.run_case(mutate)["data_status"], "INCOMPLETE")

    def test_changed_measurement_api_between_contexts_rejected(self):
        def mutate(m, i, r):
            for case in i["cases"]:
                case["params"]["impl_api"] = "different-api"
            for data in r.values():
                for row in data:
                    row["params"]["impl_api"] = "different-api"
        self.assertEqual(self.run_case(mutate)["data_status"], "INCOMPLETE")

    def test_ambiguous_input_identity_with_different_params_rejected(self):
        def mutate(m, i, r):
            extra = copy.deepcopy(i["cases"][0])
            extra["params"]["threads"] = 2
            i["cases"].append(extra)
            for side in r:
                extra_rows = [copy.deepcopy(x) for x in r[side] if x["leg"] == extra["leg"]]
                for row in extra_rows:
                    row["params"]["threads"] = 2
                r[side].extend(extra_rows)
        self.assertEqual(self.run_case(mutate)["data_status"], "INCOMPLETE")

    def test_missing_real_round_row_rejected(self):
        def mutate(m, i, r):
            r["base2"].pop()
        self.assertEqual(self.run_case(mutate)["data_status"], "INCOMPLETE")


if __name__ == "__main__":
    unittest.main()
