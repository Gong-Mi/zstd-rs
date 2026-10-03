#!/usr/bin/env python3
"""Offline contract fixtures, NEVER measurements or performance evidence.

Every test executes the production compare.py CLI with independent expected cases.
The original workflow's head-only encode acceptance was observed RED before this
suite replaced its YAML extraction; this suite has no PyYAML dependency.
"""
import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
COMPARE = ROOT / "tools/perf/compare.py"
SCRATCH = Path.home() / ".hermes/cache/scratch"


def fixture(rounds=2):
    base = {"source_sha": "a" * 40, "binary_sha256": "b" * 64}
    head = {"source_sha": "c" * 40, "binary_sha256": "d" * 64}
    cases = []
    for kind, leg in (("sample", "stream"), ("sample", "known"), ("encode", "fastest")):
        case = {"kind": kind, "corpus": "synthetic|corpus", "leg": leg,
                "bytes": 4096, "sha256": "e" * 64, "params": {"iters": 2, "ref_level": 1}}
        if kind == "sample":
            case["compressed_sha256"] = "f" * 64
        cases.append(case)
    manifest = {"schema": 1, "rounds": rounds, "harness_sha256": "1" * 64,
                "lock_sha256": "2" * 64, "runner": "offline-contract-fixture",
                "builds": {"base": base, "head": head, "base2": copy.deepcopy(base)},
                "cases": cases,
                "attachments": {"config": {"status": "NOT_RUN", "reason": "no tuning probe"},
                                "profile": {"status": "NOT_RUN", "reason": "no production probe"}}}
    sides = {}
    for side in ("base", "head", "base2"):
        rows = []
        for case in cases:
            for number in range(1, rounds + 1):
                row = dict(copy.deepcopy(case), schema=1, round=number, side=side,
                           **manifest["builds"][side], ruzstd_ms=10.0, ruzstd_cpu_ms=8.0,
                           c_ms=2.0, c_cpu_ms=1.5,
                           impl_samples=[{"wall_ms": 10.0, "cpu_ms": 8.0},
                                         {"wall_ms": 12.0, "cpu_ms": 7.0}],
                           ref_samples=[{"wall_ms": 2.0, "cpu_ms": 1.5},
                                        {"wall_ms": 3.0, "cpu_ms": 1.0}])
                if case["kind"] == "encode":
                    row.update(ruzstd_bytes=2048, c_bytes=1024, ruzstd_ratio=2.0, c_ratio=4.0)
                rows.append(row)
        sides[side] = rows
    return manifest, sides


def config_fixture(main, rounds=2):
    manifest, sides = fixture(rounds)
    manifest["builds"] = {"base": copy.deepcopy(main["builds"]["head"]),
                          "base2": copy.deepcopy(main["builds"]["head"]),
                          "head": dict(main["builds"]["head"], binary_sha256="9" * 64)}
    for side, rows in sides.items():
        for row in rows:
            row.update(manifest["builds"][side])
    raw = {"config/manifest.json": json.dumps(manifest)}
    raw.update({"config/" + side + ".jsonl": "".join(json.dumps(row) + "\n" for row in rows)
                for side, rows in sides.items()})
    return manifest, sides, raw


def set_time(row, wall=None, cpu=None, reference=False):
    samples = "ref_samples" if reference else "impl_samples"
    wall_field = "c_ms" if reference else "ruzstd_ms"
    cpu_field = "c_cpu_ms" if reference else "ruzstd_cpu_ms"
    if wall is None:
        wall = row[wall_field]
    if cpu is None:
        cpu = row[cpu_field]
    row[wall_field], row[cpu_field] = wall, cpu
    row[samples] = [{"wall_ms": wall, "cpu_ms": cpu},
                    {"wall_ms": wall * 1.2, "cpu_ms": cpu * 0.8}]


def set_encoding(row, impl_bytes=2048, ref_bytes=1024):
    row.update(ruzstd_bytes=impl_bytes, c_bytes=ref_bytes,
               ruzstd_ratio=row["bytes"] / impl_bytes, c_ratio=row["bytes"] / ref_bytes)


def suite_raw(manifest, sides):
    result = {"config/manifest.json": json.dumps(manifest)}
    result.update({"config/" + side + ".jsonl": "".join(json.dumps(row) + "\n" for row in rows)
                   for side, rows in sides.items()})
    return result


class PerfContractRegression(unittest.TestCase):
    def invoke(self, manifest, sides, *, raw=None, flags=()):
        self.assertTrue(COMPARE.is_file(), "production comparator CLI has not been implemented")
        SCRATCH.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="perf-contract-", dir=SCRATCH) as directory:
            root = Path(directory)
            (root / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
            for side, rows in sides.items():
                (root / (side + ".jsonl")).write_text(
                    "".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")
            for name, text in (raw or {}).items():
                (root / name).parent.mkdir(parents=True, exist_ok=True)
                (root / name).write_text(text, encoding="utf-8")
            result = subprocess.run(
                [sys.executable, str(COMPARE), "--manifest", str(root / "manifest.json"),
                 "--data-dir", str(root), "--output", str(root / "report.json"),
                 "--summary", str(root / "report.md"), *flags],
                cwd=ROOT, capture_output=True, text=True, timeout=30)
            self.assertTrue((root / "report.json").is_file(), result.stderr)
            self.assertTrue((root / "report.md").is_file(), result.stderr)
            report = json.loads((root / "report.json").read_text(encoding="utf-8"))
            summary = (root / "report.md").read_text(encoding="utf-8")
        return result, report, summary

    def test_head_only_encoding_cannot_be_complete(self):
        manifest, sides = fixture()
        for side in ("base", "base2"):
            sides[side] = [row for row in sides[side] if row["kind"] != "encode"]
        result, report, _ = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(report["data_status"], "INCOMPLETE")
        self.assertTrue(any("encode" in error for error in report["errors"]))

    def test_complete_is_not_a_gain_claim(self):
        result, report, summary = self.invoke(*fixture())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["data_status"], "COMPLETE")
        self.assertEqual(report["performance_status"], "NO_GAIN")
        self.assertEqual(len(report["cases"]), 3)
        self.assertIn("synthetic&#124;corpus", summary)


    def test_config_is_an_independent_default_tuned_default_suite(self):
        manifest, sides = fixture()
        _, _, raw = config_fixture(manifest, rounds=3)
        manifest["attachments"]["config"] = {
            "status": "COMPLETE", "reason": "paired default/tuned/default suite", "suite": "config"}
        result, report, summary = self.invoke(manifest, sides, raw=raw)
        self.assertEqual(result.returncode, 0, result.stderr)
        attachment = report["attachments"]["config"]
        self.assertEqual(attachment["status"], "COMPLETE")
        self.assertEqual(attachment["report"]["data_status"], "COMPLETE")
        self.assertEqual(attachment["report"]["sides"]["base"]["expected_rows"], 9)
        self.assertIn("Independent default/tuned/default A/B/A", summary)


    def assert_incomplete(self, manifest, sides, *, raw=None, flags=()):
        result, report, summary = self.invoke(manifest, sides, raw=raw, flags=flags)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(report["data_status"], "INCOMPLETE")
        self.assertEqual(report["performance_status"], "NOT_EVALUATED")
        self.assertFalse(report["accepted"])
        self.assertTrue(report["errors"])
        self.assertIn("INCOMPLETE", summary)
        return report

    def test_both_sides_omitting_encode_is_not_an_expected_set(self):
        manifest, sides = fixture()
        for side in sides:
            sides[side] = [row for row in sides[side] if row["kind"] != "encode"]
        report = self.assert_incomplete(manifest, sides)
        missing = [error for error in report["errors"] if "missing round" in error]
        self.assertEqual(len(missing), 6)
        self.assertEqual({error.split(":")[0] for error in missing}, {"base", "head", "base2"})

    def test_missing_empty_and_meta_only_sides(self):
        for side in ("base", "head", "base2"):
            for mode in ("missing", "empty", "meta-only"):
                with self.subTest(side=side, mode=mode):
                    manifest, sides = fixture()
                    if mode == "missing":
                        sides.pop(side)
                    else:
                        sides[side] = [] if mode == "empty" else [{"schema": 1, "kind": "meta"}]
                    report = self.assert_incomplete(manifest, sides)
                    self.assertEqual(report["sides"][side]["valid_rows"], 0)
                    self.assertEqual(sum("missing round" in error for error in report["errors"]), 6)

    def test_partial_side_reports_every_missing_case_round(self):
        manifest, sides = fixture(rounds=3)
        sides["head"] = [sides["head"][0]]
        report = self.assert_incomplete(manifest, sides, flags=("--fail-on-regression",))
        self.assertEqual(sum("missing round" in error for error in report["errors"]), 8)

    def test_unexpected_case_corpus_leg_bytes_hash_or_parameters(self):
        mutations = {"corpus": "another", "leg": "fastest", "bytes": 8192,
                     "sha256": "0" * 64, "params": {"iters": 2, "ref_level": 2}}
        for field, value in mutations.items():
            with self.subTest(field=field):
                manifest, sides = fixture()
                sides["head"][0][field] = value
                report = self.assert_incomplete(manifest, sides)
                self.assertTrue(any("unexpected case" in error for error in report["errors"]))
                self.assertTrue(any("missing round" in error for error in report["errors"]))

    def test_extra_case_and_duplicate_round_are_rejected(self):
        for mode in ("extra", "duplicate"):
            with self.subTest(mode=mode):
                manifest, sides = fixture()
                row = copy.deepcopy(sides["head"][0])
                if mode == "extra":
                    row["corpus"] = "unrequested"
                sides["head"].append(row)
                report = self.assert_incomplete(manifest, sides)
                self.assertTrue(any(("unexpected case" if mode == "extra" else "duplicate round") in error
                                    for error in report["errors"]))

    def test_rounds_must_be_unique_in_declared_integer_range(self):
        for value in (0, -1, 3, True, 1.0, "1", None):
            with self.subTest(round=value):
                manifest, sides = fixture()
                sides["head"][0]["round"] = value
                self.assert_incomplete(manifest, sides)
        manifest, sides = fixture()
        sides["head"][1]["round"] = 1
        self.assert_incomplete(manifest, sides)

    def test_line_and_meta_kinds_are_strict(self):
        for row in ([1, 2], None, 4, {"kind": "phase", "schema": 1},
                    {"kind": "meta"}, {"kind": "meta", "schema": True},
                    {"kind": "meta", "schema": 1, "side": "base"}):
            with self.subTest(row=row):
                manifest, sides = fixture()
                sides["head"].append(row)
                self.assert_incomplete(manifest, sides)

    def test_no_default_leg_and_required_row_fields(self):
        for field in ("schema", "corpus", "leg", "bytes", "sha256", "params", "round", "side",
                      "source_sha", "binary_sha256", "compressed_sha256", "ruzstd_ms", "ruzstd_cpu_ms",
                      "c_ms", "c_cpu_ms", "impl_samples", "ref_samples"):
            with self.subTest(field=field):
                manifest, sides = fixture()
                del sides["head"][0][field]
                self.assert_incomplete(manifest, sides)

    def test_sample_compressed_hash_matches_manifest(self):
        manifest, sides = fixture()
        sides["base"][0]["compressed_sha256"] = "0" * 64
        report = self.assert_incomplete(manifest, sides)
        self.assertTrue(any("compressed_sha256 does not match" in error for error in report["errors"]))

    def test_provenance_identity_is_not_inferred(self):
        for side in ("base", "head", "base2"):
            for field, value in (("side", "other"), ("source_sha", "0" * 40),
                                 ("binary_sha256", "0" * 64), ("source_sha", "not-a-sha")):
                with self.subTest(side=side, field=field):
                    manifest, sides = fixture()
                    sides[side][0][field] = value
                    self.assert_incomplete(manifest, sides)

    def test_base2_manifest_must_equal_base_binary_and_source(self):
        for field, value in (("binary_sha256", "9" * 64), ("source_sha", "9" * 40)):
            with self.subTest(field=field):
                manifest, sides = fixture()
                manifest["builds"]["base2"][field] = value
                for row in sides["base2"]:
                    row[field] = value
                report = self.assert_incomplete(manifest, sides)
                self.assertTrue(any("base2 build must exactly equal" in error for error in report["errors"]))

    def test_manifest_hash_formats_are_not_repaired(self):
        for field in ("harness_sha256", "lock_sha256"):
            for value in ("0" * 63, "0x" + "0" * 64, " " + "0" * 64, "g" * 64, None):
                with self.subTest(field=field, value=value):
                    manifest, sides = fixture()
                    manifest[field] = value
                    self.assert_incomplete(manifest, sides)
        for field, value in (("source_sha", "a" * 39), ("binary_sha256", "b" * 65)):
            manifest, sides = fixture()
            manifest["builds"]["head"][field] = value
            self.assert_incomplete(manifest, sides)

    def test_manifest_required_fields_types_and_expected_cases(self):
        mutations = (("schema", True), ("schema", 2), ("rounds", 0), ("rounds", 1.5),
                     ("runner", " "), ("runner", None), ("builds", []),
                     ("cases", []), ("cases", {}), ("attachments", []))
        for field, value in mutations:
            with self.subTest(field=field, value=value):
                manifest, sides = fixture()
                manifest[field] = value
                self.assert_incomplete(manifest, sides)
        for kind, leg in (("encode", "fastest"), ("sample", "stream"), ("sample", "known")):
            with self.subTest(kind=kind, leg=leg):
                manifest, sides = fixture()
                manifest["cases"] = [case for case in manifest["cases"]
                                     if (case["kind"], case["leg"]) != (kind, leg)]
                for side in sides:
                    sides[side] = [row for row in sides[side] if (row["kind"], row["leg"]) != (kind, leg)]
                self.assert_incomplete(manifest, sides)

    def test_duplicate_manifest_cases_are_rejected(self):
        manifest, sides = fixture()
        manifest["cases"].append(copy.deepcopy(manifest["cases"][0]))
        self.assert_incomplete(manifest, sides)

    def test_same_corpus_different_params_is_a_distinct_case(self):
        manifest, sides = fixture()
        case = copy.deepcopy(manifest["cases"][0])
        case["params"]["ref_level"] = 9
        manifest["cases"].append(case)
        for side in sides:
            for original in list(sides[side][:2]):
                row = copy.deepcopy(original)
                row["params"] = copy.deepcopy(case["params"])
                sides[side].append(row)
        result, report, _ = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(report["cases"]), 4)
        self.assertEqual(report["sides"]["head"]["expected_rows"], 8)

    def test_canonical_nested_parameters_ignore_object_order_not_content(self):
        manifest, sides = fixture()
        for case in manifest["cases"]:
            case["params"]["options"] = {"b": [1, True, {"x": "value"}], "a": 2.5}
        for side, rows in sides.items():
            for row in rows:
                row["params"] = {"options": {"a": 2.5, "b": [1, True, {"x": "value"}]},
                                 "ref_level": 1, "iters": 2}
        result, report, _ = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0, result.stderr)
        sides["head"][0]["params"]["options"]["b"][2]["x"] = "changed"
        self.assert_incomplete(manifest, sides)

    def test_duplicate_json_keys_at_any_depth_are_rejected(self):
        for location in ("manifest", "row", "nested"):
            with self.subTest(location=location):
                manifest, sides = fixture()
                if location == "manifest":
                    raw = {"manifest.json": json.dumps(manifest).replace('"schema": 1', '"schema": 1, "schema": 1', 1)}
                else:
                    lines = [json.dumps(row) for row in sides["head"]]
                    if location == "row":
                        lines[0] = lines[0].replace('"ruzstd_ms": 10.0', '"ruzstd_ms": 10.0, "ruzstd_ms": 10.0', 1)
                    else:
                        lines[0] = lines[0].replace('"iters": 2', '"iters": 2, "iters": 2', 1)
                    raw = {"head.jsonl": "\n".join(lines)}
                report = self.assert_incomplete(manifest, sides, raw=raw)
                self.assertTrue(any("duplicate JSON key" in error for error in report["errors"]))

    def test_non_json_garbage_and_malformed_manifest_produce_reports(self):
        for text in ("log noise\n", "{\n", "[]\n", "null\n"):
            with self.subTest(text=text):
                manifest, sides = fixture()
                self.assert_incomplete(manifest, sides, raw={"head.jsonl": text})
                self.assert_incomplete(manifest, sides, raw={"manifest.json": text})

    def test_blank_lines_and_valid_metadata_do_not_add_measurements(self):
        manifest, sides = fixture()
        raw = {"head.jsonl": "\n \t\n" + json.dumps({"schema": 1, "kind": "meta", "note": "fixture"}) + "\n" +
               "\n\n".join(json.dumps(row) for row in reversed(sides["head"])) + "\n"}
        result, report, _ = self.invoke(manifest, sides, raw=raw)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["sides"]["head"]["valid_rows"], 6)
        self.assertEqual(report["sides"]["head"]["meta_rows"], 1)

    def test_all_metrics_must_be_finite_positive_numbers_not_bool(self):
        for field in ("ruzstd_ms", "ruzstd_cpu_ms", "c_ms", "c_cpu_ms"):
            for value in (0, -1, True, "1", None, float("nan"), float("inf"), -float("inf")):
                with self.subTest(field=field, value=value):
                    manifest, sides = fixture()
                    sides["head"][0][field] = value
                    self.assert_incomplete(manifest, sides)

    def test_nested_nonfinite_json_fields_and_overflow_literals(self):
        for location in ("params", "raw_sample", "meta", "manifest"):
            with self.subTest(location=location):
                manifest, sides = fixture()
                if location == "params":
                    sides["head"][0]["params"]["nested"] = {"bad": [float("nan")]}
                elif location == "raw_sample":
                    sides["head"][0]["impl_samples"][0]["extra"] = float("inf")
                elif location == "meta":
                    sides["head"].append({"kind": "meta", "schema": 1, "bad": float("inf")})
                else:
                    manifest["nested"] = {"bad": [float("inf")]}
                self.assert_incomplete(manifest, sides)
        manifest, sides = fixture()
        raw = {"head.jsonl": "\n".join(json.dumps(row) for row in sides["head"]).replace('"ruzstd_ms": 10.0', '"ruzstd_ms": 1e999', 1)}
        self.assert_incomplete(manifest, sides, raw=raw)

    def test_iters_and_raw_sample_counts_are_required_positive_integers(self):
        for value in (0, -1, True, 2.0, "2", None):
            with self.subTest(iters=value):
                manifest, sides = fixture()
                sides["head"][0]["params"]["iters"] = value
                self.assert_incomplete(manifest, sides)
        for field in ("impl_samples", "ref_samples"):
            for samples in ([], None, [{"wall_ms": 10.0, "cpu_ms": 8.0}], [1, 2],
                            [{"wall_ms": 0, "cpu_ms": 8}, {"wall_ms": 12, "cpu_ms": 7}],
                            [{"wall_ms": 10, "cpu_ms": True}, {"wall_ms": 12, "cpu_ms": 7}]):
                with self.subTest(field=field, samples=samples):
                    manifest, sides = fixture()
                    sides["head"][0][field] = samples
                    self.assert_incomplete(manifest, sides)

    def test_summary_cpu_must_accompany_selected_min_wall(self):
        for field, wrong in (("ruzstd_cpu_ms", 7.0), ("c_cpu_ms", 1.0),
                             ("ruzstd_ms", 12.0), ("c_ms", 3.0)):
            with self.subTest(field=field):
                manifest, sides = fixture()
                sides["head"][0][field] = wrong
                report = self.assert_incomplete(manifest, sides)
                self.assertTrue(any("same selected minimum-wall" in error for error in report["errors"]))

    def test_selection_is_not_dependent_on_raw_sample_position(self):
        manifest, sides = fixture()
        for side in sides:
            for row in sides[side]:
                row["impl_samples"].reverse()
                row["ref_samples"].reverse()
        result, report, _ = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["cases"][0]["sides"]["head"]["cpu_ms"], 8.0)

    def test_exact_wall_ties_use_first_sample_cpu(self):
        manifest, sides = fixture()
        sides["head"][0]["impl_samples"][1]["wall_ms"] = 10.0
        result, _, _ = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0, result.stderr)
        sides["head"][0]["ruzstd_cpu_ms"] = 7.0
        self.assert_incomplete(manifest, sides)

    def test_encode_requires_all_byte_and_ratio_fields(self):
        for field in ("ruzstd_bytes", "c_bytes", "ruzstd_ratio", "c_ratio"):
            with self.subTest(field=field):
                manifest, sides = fixture()
                del sides["head"][4][field]
                self.assert_incomplete(manifest, sides)

    def test_encoding_bytes_are_positive_int_and_ratios_match(self):
        for field in ("ruzstd_bytes", "c_bytes"):
            for value in (0, -1, True, 2048.0, "2048", None):
                with self.subTest(field=field, value=value):
                    manifest, sides = fixture()
                    sides["head"][4][field] = value
                    self.assert_incomplete(manifest, sides)
        for field in ("ruzstd_ratio", "c_ratio"):
            for value in (0, -1, True, "2", None, 1.0, float("nan"), float("inf")):
                with self.subTest(field=field, value=value):
                    manifest, sides = fixture()
                    sides["head"][4][field] = value
                    self.assert_incomplete(manifest, sides)

    def test_encode_output_must_remain_consistent_across_rounds(self):
        for side in ("base", "head", "base2"):
            for reference in (False, True):
                with self.subTest(side=side, reference=reference):
                    manifest, sides = fixture()
                    set_encoding(sides[side][4], impl_bytes=2048 if reference else 1024,
                                 ref_bytes=2048 if reference else 1024)
                    report = self.assert_incomplete(manifest, sides)
                    self.assertTrue(any("inconsistent encoding" in error for error in report["errors"]))

    def test_time_gains_cannot_hide_encoding_ratio_tradeoff(self):
        manifest, sides = fixture()
        for row in sides["head"]:
            set_time(row, wall=8.0, cpu=6.4)
            if row["kind"] == "encode":
                set_encoding(row, impl_bytes=3072)
        for gate in ((), ("--fail-on-regression",)):
            result, report, summary = self.invoke(manifest, sides, flags=gate)
            self.assertEqual(result.returncode, 2 if gate else 0, result.stderr)
            self.assertEqual(report["data_status"], "COMPLETE")
            self.assertEqual(report["performance_status"], "NOT_ACCEPTED")
            self.assertEqual(report["cases"][2]["performance_status"], "TRADEOFF")
            self.assertLess(report["cases"][2]["paired"]["ratio"]["median_delta_pct"], 0)
            self.assertIn("TRADEOFF", summary)

    def test_ratio_improvement_alone_is_not_time_gain(self):
        manifest, sides = fixture()
        for row in sides["head"]:
            if row["kind"] == "encode":
                set_encoding(row, impl_bytes=1024)
        result, report, _ = self.invoke(manifest, sides, flags=("--fail-on-regression",))
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["performance_status"], "NO_GAIN")
        self.assertEqual(report["cases"][2]["paired"]["ratio"]["median_delta_pct"], 100.0)

    def test_simultaneous_wall_cpu_gain_and_explicit_gate(self):
        manifest, sides = fixture()
        for row in sides["head"]:
            set_time(row, wall=8.0, cpu=6.4)
        result, report, _ = self.invoke(manifest, sides, flags=("--fail-on-regression",))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["accepted"])
        self.assertEqual(report["performance_status"], "GAIN")
        self.assertEqual({case["performance_status"] for case in report["cases"]}, {"GAIN"})
        self.assertAlmostEqual(report["cases"][0]["paired"]["wall"]["median_delta_pct"], -20.0)

    def test_gain_in_one_case_and_no_change_elsewhere_is_accepted(self):
        manifest, sides = fixture()
        for row in sides["head"][:2]:
            set_time(row, wall=8.0, cpu=6.4)
        result, report, _ = self.invoke(manifest, sides, flags=("--fail-on-regression",))
        self.assertEqual(result.returncode, 0)
        self.assertTrue(report["accepted"])
        self.assertEqual([case["performance_status"] for case in report["cases"]],
                         ["GAIN", "NO_CHANGE", "NO_CHANGE"])

    def test_regression_default_exit_zero_and_gate_exit_two(self):
        manifest, sides = fixture()
        for row in sides["head"]:
            set_time(row, wall=12.0, cpu=9.6)
        for gate in ((), ("--fail-on-regression",)):
            result, report, _ = self.invoke(manifest, sides, flags=gate)
            self.assertEqual(result.returncode, 2 if gate else 0)
            self.assertEqual(report["performance_status"], "REGRESSION")
            self.assertFalse(report["accepted"])

    def test_direction_conflict_and_single_metric_gain_are_unresolved(self):
        for wall, cpu in ((8.0, 9.6), (12.0, 6.4), (8.0, 8.0), (10.0, 6.4),
                          (9.95, 8.04)):
            with self.subTest(wall=wall, cpu=cpu):
                manifest, sides = fixture()
                for row in sides["head"]:
                    set_time(row, wall=wall, cpu=cpu)
                result, report, _ = self.invoke(manifest, sides, flags=("--fail-on-regression",))
                self.assertEqual(result.returncode, 2)
                self.assertEqual(report["performance_status"], "NOT_ACCEPTED")
                self.assertEqual({case["performance_status"] for case in report["cases"]}, {"UNRESOLVED"})

    def test_no_gain_is_not_accepted_by_explicit_gate(self):
        result, report, summary = self.invoke(*fixture(), flags=("--fail-on-regression",))
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["performance_status"], "NO_GAIN")
        self.assertIn("no gain", summary)

    def test_threshold_must_be_strictly_exceeded_for_both_metrics(self):
        manifest, sides = fixture()
        for row in sides["head"]:
            set_time(row, wall=9.9, cpu=7.92)
        result, report, _ = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(report["performance_status"], "NO_GAIN")
        result, report, _ = self.invoke(manifest, sides, flags=("--min-effect-pct", "0.5", "--fail-on-regression"))
        self.assertEqual(result.returncode, 0)
        self.assertTrue(report["accepted"])

    def test_aa_noise_is_per_case_per_metric_max_not_average(self):
        manifest, sides = fixture()
        for row in sides["head"]:
            set_time(row, wall=8.0, cpu=6.4)
        set_time(sides["base2"][0], wall=13.0, cpu=8.0)
        set_time(sides["base2"][1], wall=10.0, cpu=8.4)
        result, report, _ = self.invoke(manifest, sides, flags=("--fail-on-regression",))
        self.assertEqual(result.returncode, 2)
        first = report["cases"][0]
        self.assertEqual(first["paired"]["wall"]["aa_noise_pct"], 30.0)
        self.assertEqual(first["paired"]["wall"]["threshold_pct"], 30.0)
        self.assertEqual(first["paired"]["cpu"]["aa_noise_pct"], 5.0)
        self.assertEqual(first["performance_status"], "UNRESOLVED")
        self.assertEqual(report["cases"][1]["performance_status"], "GAIN")
        self.assertEqual(report["cases"][1]["paired"]["wall"]["aa_noise_pct"], 0.0)

    def test_negative_aa_fluctuation_is_absolute_noise(self):
        manifest, sides = fixture()
        for row in sides["base2"]:
            set_time(row, wall=8.0, cpu=6.4)
        for row in sides["head"]:
            set_time(row, wall=9.0, cpu=7.2)
        result, report, _ = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(report["performance_status"], "NO_GAIN")
        self.assertEqual(report["cases"][0]["paired"]["wall"]["aa_noise_pct"], 20.0)

    def test_estimator_is_round_paired_median_not_ratio_of_minima(self):
        manifest, sides = fixture(rounds=3)
        for side in sides:
            for row in sides[side]:
                if side == "head":
                    wall = (8.0, 10.0, 12.0)[row["round"] - 1]
                    set_time(row, wall=wall, cpu=wall * 0.8)
        result, report, _ = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0)
        first = report["cases"][0]
        self.assertEqual(first["paired"]["wall"]["median_delta_pct"], 0.0)
        self.assertEqual([row["delta_pct"] for row in first["paired"]["wall"]["round_deltas_pct"]], [-20.0, 0.0, 20.0])
        self.assertEqual(first["sides"]["head"]["diagnostic_min_wall_ms"], 8.0)
        self.assertEqual(first["performance_status"], "NO_CHANGE")
        for side in sides:
            sides[side].reverse()
        _, reversed_report, _ = self.invoke(manifest, sides)
        self.assertEqual(reversed_report["cases"], report["cases"])

    def test_percent_median_not_ratio_of_unpaired_medians(self):
        manifest, sides = fixture(rounds=2)
        for row in sides["base"] + sides["base2"]:
            wall = 1.0 if row["round"] == 1 else 100.0
            set_time(row, wall=wall, cpu=wall)
        for row in sides["head"]:
            wall = 2.0 if row["round"] == 1 else 50.0
            set_time(row, wall=wall, cpu=wall)
        result, report, _ = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(report["cases"][0]["paired"]["wall"]["median_delta_pct"], 25.0)
        self.assertEqual(report["performance_status"], "REGRESSION")

    def test_all_reference_metrics_ratios_and_raw_counts_are_visible(self):
        manifest, sides = fixture(rounds=3)
        for row in sides["head"]:
            set_time(row, wall=4.0, cpu=3.0, reference=True)
            if row["kind"] == "encode":
                set_encoding(row, impl_bytes=2048, ref_bytes=2048)
        result, report, summary = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0)
        case = report["cases"][2]
        self.assertEqual(case["sides"]["head"]["reference_wall_ms"], 4.0)
        self.assertEqual(case["sides"]["base"]["c_ratio"], 4.0)
        self.assertEqual(case["sides"]["head"]["c_ratio"], 2.0)
        self.assertEqual(case["paired"]["reference_ratio"]["median_delta_pct"], -50.0)
        self.assertEqual(case["paired"]["reference_wall"]["median_delta_pct"], 100.0)
        for side in case["sides"].values():
            self.assertEqual(side["round_count"], 3)
            self.assertEqual(side["impl_sample_count"], 6)
            self.assertEqual(side["ref_sample_count"], 6)
        self.assertIn("C wall / CPU", summary)
        self.assertIn("C ratio", summary)
        self.assertIn("3 / 6 / 6", summary)

    def test_invalid_min_effect_still_produces_reports(self):
        for value in ("nan", "inf", "-1"):
            with self.subTest(value=value):
                self.assert_incomplete(*fixture(), flags=("--min-effect-pct", value))

    def test_optional_not_run_failed_are_visible_without_fake_zero_gain(self):
        for status in ("NOT_RUN", "FAILED"):
            with self.subTest(status=status):
                manifest, sides = fixture()
                manifest["attachments"]["config"] = {"status": status, "reason": "not measured|reason"}
                manifest["attachments"]["profile"] = {"status": status, "reason": "no probe"}
                result, report, summary = self.invoke(manifest, sides)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(report["attachments"]["config"]["status"], status)
                self.assertNotIn("report", report["attachments"]["config"])
                self.assertNotIn("phase_budget", report["attachments"]["profile"])
                self.assertIn(status + ": not measured&#124;reason", summary)

    def test_config_missing_suite_or_invalid_path_warns_only(self):
        for suite in (None, "missing", "..", "/absolute", ".", "config/../../elsewhere", 3, "bad\x00path"):
            with self.subTest(suite=suite):
                manifest, sides = fixture()
                manifest["attachments"]["config"] = {"status": "COMPLETE", "reason": "independent", "suite": suite}
                result, report, _ = self.invoke(manifest, sides)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(report["data_status"], "COMPLETE")
                self.assertEqual(report["attachments"]["config"]["status"], "FAILED")
                self.assertTrue(report["warnings"])
                self.assertFalse(report["errors"])

    def test_config_incomplete_suite_is_failed_not_primary_incomplete(self):
        for failure in ("missing_side", "missing_case", "duplicate", "bad_identity", "malformed"):
            with self.subTest(failure=failure):
                manifest, sides = fixture()
                config, config_sides, _ = config_fixture(manifest)
                manifest["attachments"]["config"] = {"status": "COMPLETE", "reason": "independent", "suite": "config"}
                if failure == "missing_side":
                    config_sides.pop("base2")
                elif failure == "missing_case":
                    for side in config_sides:
                        config_sides[side] = [row for row in config_sides[side] if row["kind"] != "encode"]
                elif failure == "duplicate":
                    config_sides["head"].append(copy.deepcopy(config_sides["head"][0]))
                elif failure == "bad_identity":
                    config_sides["base"][0]["binary_sha256"] = "8" * 64
                raw = suite_raw(config, config_sides)
                if failure == "malformed":
                    raw["config/manifest.json"] = "malformed"
                result, report, _ = self.invoke(manifest, sides, raw=raw)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(report["data_status"], "COMPLETE")
                self.assertEqual(report["attachments"]["config"]["status"], "FAILED")
                self.assertTrue(report["warnings"])

    def test_config_requires_primary_head_default_binary_and_all_source_ids(self):
        for failure in ("default_binary", "default_source", "tuned_source"):
            with self.subTest(failure=failure):
                manifest, sides = fixture()
                config, config_sides, _ = config_fixture(manifest)
                manifest["attachments"]["config"] = {"status": "COMPLETE", "reason": "independent", "suite": "config"}
                target_sides = ("head",) if failure == "tuned_source" else ("base", "base2")
                field = "binary_sha256" if failure == "default_binary" else "source_sha"
                value = "8" * (64 if field == "binary_sha256" else 40)
                for side in target_sides:
                    config["builds"][side][field] = value
                    for row in config_sides[side]:
                        row[field] = value
                result, report, _ = self.invoke(manifest, sides, raw=suite_raw(config, config_sides))
                self.assertEqual(result.returncode, 0)
                self.assertEqual(report["attachments"]["config"]["status"], "FAILED")
                self.assertEqual(report["attachments"]["config"]["report"]["data_status"], "COMPLETE")

    def test_config_recursion_is_forbidden_and_warns(self):
        manifest, sides = fixture()
        config, config_sides, _ = config_fixture(manifest)
        config["attachments"]["config"] = {"status": "COMPLETE", "reason": "nested", "suite": "config"}
        manifest["attachments"]["config"] = {"status": "COMPLETE", "reason": "independent", "suite": "config"}
        result, report, _ = self.invoke(manifest, sides, raw=suite_raw(config, config_sides))
        self.assertEqual(result.returncode, 0)
        self.assertEqual(report["attachments"]["config"]["status"], "FAILED")
        self.assertTrue(any("nested config attachments are forbidden" in warning for warning in report["warnings"]))

    def test_config_has_independent_noise_and_is_not_compared_with_earlier_head(self):
        manifest, sides = fixture()
        for row in sides["head"]:
            set_time(row, wall=5.0, cpu=4.0)
        for row in sides["base2"]:
            set_time(row, wall=12.0, cpu=9.6)
        config, config_sides, _ = config_fixture(manifest)
        for row in config_sides["head"]:
            set_time(row, wall=8.0, cpu=6.4)
        manifest["attachments"]["config"] = {"status": "COMPLETE", "reason": "independent", "suite": "config"}
        result, report, summary = self.invoke(manifest, sides, raw=suite_raw(config, config_sides))
        self.assertEqual(result.returncode, 0)
        independent = report["attachments"]["config"]["report"]
        self.assertEqual(independent["performance_status"], "GAIN")
        self.assertEqual(independent["cases"][0]["paired"]["wall"]["median_delta_pct"], -20.0)
        self.assertEqual(independent["cases"][0]["paired"]["wall"]["aa_noise_pct"], 0.0)
        self.assertEqual(independent["cases"][0]["sides"]["base"]["wall_ms"], 10.0)
        self.assertIn("not primary-head versus later tuning", summary)

    def test_config_regression_is_reported_independently_from_primary_gate(self):
        manifest, sides = fixture()
        for row in sides["head"]:
            set_time(row, wall=8.0, cpu=6.4)
        config, config_sides, _ = config_fixture(manifest)
        for row in config_sides["head"]:
            set_time(row, wall=12.0, cpu=9.6)
        manifest["attachments"]["config"] = {"status": "COMPLETE", "reason": "independent", "suite": "config"}
        result, report, _ = self.invoke(manifest, sides, raw=suite_raw(config, config_sides),
                                        flags=("--fail-on-regression",))
        self.assertEqual(result.returncode, 0)
        self.assertTrue(report["accepted"])
        self.assertEqual(report["attachments"]["config"]["status"], "COMPLETE")
        self.assertEqual(report["attachments"]["config"]["report"]["performance_status"], "REGRESSION")


    def test_invalid_unicode_nested_values_produce_reports_not_tracebacks(self):
        manifest, sides = fixture()
        sides["head"][0]["params"]["bad_unicode"] = "\ud800"
        self.assert_incomplete(manifest, sides)
        manifest, sides = fixture()
        manifest["cases"][0]["params"]["bad_unicode"] = "\ud800"
        self.assert_incomplete(manifest, sides)

    def test_encoding_output_byte_counts_are_preserved_as_integers(self):
        result, report, _ = self.invoke(*fixture())
        self.assertEqual(result.returncode, 0)
        for stats in report["cases"][2]["sides"].values():
            self.assertIs(type(stats["ruzstd_bytes"]), int)
            self.assertIs(type(stats["c_bytes"]), int)

    def test_invalid_manifest_case_nested_fields(self):
        mutations = (("kind", "meta"), ("corpus", ""), ("corpus", []), ("leg", None),
                     ("bytes", True), ("bytes", -1), ("bytes", 1.0), ("sha256", "x" * 64),
                     ("params", []), ("params", {}), ("params", {"iters": False}),
                     ("compressed_sha256", "bad"))
        for field, value in mutations:
            with self.subTest(field=field, value=value):
                manifest, sides = fixture()
                manifest["cases"][0][field] = value
                self.assert_incomplete(manifest, sides)

    def test_sample_and_encode_only_fields_are_not_interchanged(self):
        for kind in ("sample", "encode"):
            with self.subTest(kind=kind):
                manifest, sides = fixture()
                if kind == "sample":
                    sides["head"][0]["c_ratio"] = 4.0
                else:
                    sides["head"][4]["compressed_sha256"] = "f" * 64
                self.assert_incomplete(manifest, sides)

    def test_ratio_rounding_tolerance_is_small_and_explicit(self):
        manifest, sides = fixture()
        for row in sides["head"]:
            if row["kind"] == "encode":
                row["ruzstd_ratio"] += 1e-10
                row["c_ratio"] -= 1e-10
        result, report, _ = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["performance_status"], "NO_GAIN")
        sides["head"][4]["ruzstd_ratio"] += 1e-5
        self.assert_incomplete(manifest, sides)

    def test_ratio_degradation_below_time_threshold_is_still_tradeoff(self):
        manifest, sides = fixture()
        for row in sides["head"]:
            set_time(row, wall=8.0, cpu=6.4)
            if row["kind"] == "encode":
                set_encoding(row, impl_bytes=2049)
        result, report, _ = self.invoke(manifest, sides, flags=("--fail-on-regression",))
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["cases"][2]["performance_status"], "TRADEOFF")

    def test_c_reference_drift_is_not_substituted_for_same_binary_aa(self):
        manifest, sides = fixture()
        for row in sides["head"]:
            set_time(row, wall=9.0, cpu=7.2)
            set_time(row, wall=4.0, cpu=3.0, reference=True)
        result, report, _ = self.invoke(manifest, sides, flags=("--fail-on-regression",))
        self.assertEqual(result.returncode, 0)
        self.assertTrue(report["accepted"])
        self.assertEqual(report["cases"][0]["paired"]["wall"]["aa_noise_pct"], 0.0)
        self.assertEqual(report["cases"][0]["paired"]["reference_wall"]["median_delta_pct"], 100.0)

    def test_finite_input_overflow_percentage_is_incomplete_not_json_infinity(self):
        manifest, sides = fixture()
        for row in sides["base"] + sides["base2"]:
            set_time(row, wall=1e-300, cpu=1e-300)
        for row in sides["head"]:
            set_time(row, wall=1e300, cpu=1e300)
        report = self.assert_incomplete(manifest, sides)
        self.assertTrue(any("computed percent difference" in error for error in report["errors"]))

    def test_large_finite_equal_times_do_not_overflow_medians(self):
        manifest, sides = fixture()
        for side in sides:
            for row in sides[side]:
                set_time(row, wall=1e308, cpu=1e308)
        result, report, _ = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report["cases"][0]["sides"]["head"]["wall_ms"], 1e308)
        self.assertEqual(report["performance_status"], "NO_GAIN")

    def test_invalid_late_line_cannot_be_hidden_by_complete_earlier_data(self):
        manifest, sides = fixture()
        lines = "\n".join(json.dumps(row) for row in sides["head"])
        self.assert_incomplete(manifest, sides, raw={"head.jsonl": lines + "\ntrailing garbage\n"})

    def test_config_old_unpaired_head_tuned_form_is_not_consumed(self):
        manifest, sides = fixture()
        manifest["attachments"]["config"] = {"status": "COMPLETE", "reason": "legacy unpaired",
                                               "file": "head_tuned.jsonl", "rounds": 2,
                                               "build": manifest["builds"]["head"]}
        raw = {"head_tuned.jsonl": "\n".join(json.dumps(row) for row in sides["head"])}
        result, report, _ = self.invoke(manifest, sides, raw=raw)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(report["attachments"]["config"]["status"], "FAILED")
        self.assertNotIn("report", report["attachments"]["config"])

    def test_error_table_and_parameter_pipes_are_escaped(self):
        manifest, sides = fixture()
        for case in manifest["cases"]:
            case["params"]["label"] = "A|B\n<bad>"
        for side in sides:
            for row in sides[side]:
                row["params"]["label"] = "A|B\n<bad>"
        result, _, summary = self.invoke(manifest, sides)
        self.assertEqual(result.returncode, 0)
        self.assertIn("A&#124;B", summary)
        self.assertIn("&lt;bad&gt;", summary)
        self.assertNotIn("A|B", summary)
        sides["head"].pop()
        _, report, summary = self.invoke(manifest, sides)
        self.assertEqual(report["data_status"], "INCOMPLETE")
        self.assertIn("## Errors", summary)
        self.assertNotIn("A|B", summary)


    def test_tiny_times_cannot_use_absolute_tolerance_to_hide_wrong_selected_cpu(self):
        manifest, sides = fixture()
        set_time(sides["head"][0], wall=1e-30, cpu=8e-30)
        sides["head"][0]["ruzstd_cpu_ms"] = 8e-29
        self.assert_incomplete(manifest, sides)

    def test_tiny_ratio_cannot_use_absolute_tolerance_to_hide_wrong_byte_ratio(self):
        manifest, sides = fixture()
        for side in sides:
            for row in sides[side]:
                if row["kind"] == "encode":
                    set_encoding(row, impl_bytes=2 ** 60)
        sides["head"][4]["ruzstd_ratio"] = 1e-13
        self.assert_incomplete(manifest, sides)

    def test_exact_encoding_byte_degradation_cannot_be_rounded_into_a_gain(self):
        manifest, sides = fixture()
        for side in sides:
            for row in sides[side]:
                if side == "head":
                    set_time(row, wall=8.0, cpu=6.4)
                if row["kind"] == "encode":
                    set_encoding(row, impl_bytes=2 ** 60 + (1 if side == "head" else 0))
        result, report, _ = self.invoke(manifest, sides, flags=("--fail-on-regression",))
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["cases"][2]["performance_status"], "TRADEOFF")


if __name__ == "__main__":
    unittest.main(verbosity=2)
