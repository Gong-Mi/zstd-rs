#!/usr/bin/env python3
"""Strict, stdlib-only schema-1 A/B/A performance data comparator.

Exit contract: 0 = complete primary data, NOT proof of a gain; 1 = incomplete
primary data or report I/O failure; with --fail-on-regression, 2 = complete but
NOT_ACCEPTED (a regression, ratio tradeoff, unresolved direction, or no gain).
The explicit gate accepts only at least one GAIN and all other cases GAIN or
NO_CHANGE. Optional attachment failure does not invalidate primary data.

Each metric uses the median of round-matched head/base percent differences.
A/A noise is max(abs(base2/base percent difference)) for that case AND metric;
threshold = max(min_effect_pct, noise). Both wall and CPU must STRICTLY exceed
threshold in the same direction. This is a conservative operational screen,
not a significance test or a cross-run speedup trajectory. Encoding ratios
are input_bytes/output_bytes; a reduced implementation ratio is a tradeoff.

Each round selects one minimum-wall raw sample (first wins exact ties). Its
CPU must accompany that same sample, including for the C reference. Across-
round minima are diagnostics only. Provenance hashes are validated and matched
to the independent manifest, not recomputed from absent source/build artifacts.
"""
import argparse
from decimal import Decimal
import html
import json
import math
from pathlib import Path
import re
import sys

SIDES = ("base", "head", "base2")
LEGS = ("stream", "known", "fastest")
ATTACHMENT_STATES = ("NOT_RUN", "FAILED", "COMPLETE")
REL_TOL = 1e-9
ABS_TOL = 0.0


def positive_int(value):
    return type(value) is int and value > 0


def finite_number(value, positive=False):
    if type(value) not in (int, float):
        return False
    try:
        return math.isfinite(value) and (not positive or value > 0)
    except OverflowError:
        return False


def same_number(left, right):
    return math.isclose(left, right, rel_tol=REL_TOL, abs_tol=ABS_TOL)


def nonempty_string(value):
    return isinstance(value, str) and bool(value.strip())


def hex_value(value, length):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-fA-F]{%d}" % length, value) is not None


def reject_constant(value):
    raise ValueError("nonfinite JSON constant: " + value)


def unique_object(pairs):
    obj = {}
    for key, value in pairs:
        if key in obj:
            raise ValueError("duplicate JSON key: " + repr(key))
        obj[key] = value
    return obj


def finite_tree(value, path="$"):
    """JSON may parse 1e999 as inf even when parse_constant is strict."""
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError("nonfinite nested field at " + path)
    if isinstance(value, str):
        # Escaped lone surrogates are accepted by json.loads but cannot be
        # emitted as UTF-8 reports; reject them at the input boundary.
        value.encode("utf-8", errors="strict")
    if isinstance(value, dict):
        for key, child in value.items():
            key.encode("utf-8", errors="strict")
            finite_tree(child, path + "." + key)
    elif isinstance(value, list):
        for i, child in enumerate(value):
            finite_tree(child, path + "[%d]" % i)


def strict_json(text):
    value = json.loads(text, object_pairs_hook=unique_object, parse_constant=reject_constant)
    finite_tree(value)
    return value


def canonical_params(params):
    return json.dumps(params, sort_keys=True, separators=(",", ":"), ensure_ascii=False,
                      allow_nan=False)


def case_key(case):
    return (case["kind"], case["corpus"], case["leg"], case["bytes"], case["sha256"],
            canonical_params(case["params"]))


def case_label(case):
    return "%s/%s/%s bytes=%s sha256=%s params=%s" % (
        case["kind"], case["corpus"], case["leg"], case["bytes"], case["sha256"],
        canonical_params(case["params"]))


def check(condition, message, errors):
    if not condition:
        errors.append(message)
    return condition


def validate_build(build, path, errors):
    if not check(isinstance(build, dict), path + ": expected build object", errors):
        return False
    start = len(errors)
    check(hex_value(build.get("source_sha"), 40), path + ": invalid source_sha (40 hex)", errors)
    check(hex_value(build.get("binary_sha256"), 64), path + ": invalid binary_sha256 (64 hex)", errors)
    return len(errors) == start


def validate_case(case, path, errors):
    if not check(isinstance(case, dict), path + ": expected case object", errors):
        return False
    start = len(errors)
    check(case.get("kind") in ("sample", "encode"), path + ": invalid case kind", errors)
    check(nonempty_string(case.get("corpus")), path + ": missing/nonempty corpus", errors)
    check(case.get("leg") in LEGS, path + ": missing/invalid leg", errors)
    check(positive_int(case.get("bytes")), path + ": bytes must be a positive integer", errors)
    check(hex_value(case.get("sha256"), 64), path + ": invalid sha256 (64 hex)", errors)
    params = case.get("params")
    if check(isinstance(params, dict) and bool(params), path + ": params must be nonempty object", errors):
        check(positive_int(params.get("iters")), path + ": params.iters must be a positive integer", errors)
    if case.get("kind") == "sample":
        check(hex_value(case.get("compressed_sha256"), 64), path + ": invalid compressed_sha256 (64 hex)", errors)
    elif case.get("kind") == "encode":
        check("compressed_sha256" not in case, path + ": compressed_sha256 is sample-only", errors)
    return len(errors) == start


def load_manifest(path, errors):
    try:
        manifest = strict_json(Path(path).read_text(encoding="utf-8"))
    except (OSError, ValueError, RecursionError) as exc:
        errors.append("manifest: " + str(exc))
        return {}, {}
    if not check(isinstance(manifest, dict), "manifest: expected object", errors):
        return {}, {}
    check(type(manifest.get("schema")) is int and manifest.get("schema") == 1,
          "manifest: schema must be integer 1", errors)
    check(positive_int(manifest.get("rounds")), "manifest: rounds must be a positive integer", errors)
    for key in ("harness_sha256", "lock_sha256"):
        check(hex_value(manifest.get(key), 64), "manifest: invalid " + key + " (64 hex)", errors)
    check(nonempty_string(manifest.get("runner")), "manifest: runner must be nonempty string", errors)
    builds = manifest.get("builds")
    if check(isinstance(builds, dict), "manifest: builds must be an object", errors):
        for side in SIDES:
            validate_build(builds.get(side), "manifest.builds." + side, errors)
        check(isinstance(builds.get("base"), dict) and builds.get("base2") == builds["base"],
              "manifest: base2 build must exactly equal base (same source AND binary)", errors)
    cases = manifest.get("cases")
    expected = {}
    if check(isinstance(cases, list) and bool(cases), "manifest: cases must be nonempty list", errors):
        for index, case in enumerate(cases):
            if validate_case(case, "manifest.cases[%d]" % index, errors):
                key = case_key(case)
                if key in expected:
                    errors.append("manifest: duplicate expected case " + case_label(case))
                else:
                    expected[key] = case
        for leg in ("stream", "known"):
            check(any(case["kind"] == "sample" and case["leg"] == leg for case in expected.values()),
                  "manifest: required sample leg " + leg + " is absent", errors)
        check(any(case["kind"] == "encode" for case in expected.values()),
              "manifest: required encode case is absent", errors)
    attachments = manifest.get("attachments")
    if check(isinstance(attachments, dict), "manifest: attachments must be object", errors):
        for name in ("config", "profile"):
            declaration = attachments.get(name)
            if not check(isinstance(declaration, dict), "manifest.attachments." + name + ": expected object", errors):
                continue
            prefix = "manifest.attachments." + name
            check(declaration.get("status") in ATTACHMENT_STATES, prefix + ": invalid status", errors)
            check(isinstance(declaration.get("reason"), str), prefix + ": reason must be string", errors)
            # Config is optional independent-suite metadata. Missing/invalid
            # suite/build/data is handled as FAILED + warnings after primary
            # comparison; old file/head_tuned declarations are not consumed.
    return manifest, expected


def validate_samples(row, name, wall_field, cpu_field, path, errors):
    samples = row.get(name)
    iters = row["params"]["iters"]
    if not check(isinstance(samples, list) and len(samples) == iters,
                 path + ": " + name + " length must equal params.iters", errors):
        return
    start = len(errors)
    for index, sample in enumerate(samples):
        label = path + ": " + name + "[%d]" % index
        if not check(isinstance(sample, dict), label + " must be object", errors):
            continue
        for field in ("wall_ms", "cpu_ms"):
            check(finite_number(sample.get(field), positive=True),
                  label + "." + field + " must be finite > 0", errors)
    if len(errors) == start and all(finite_number(row.get(field), positive=True)
                                    for field in (wall_field, cpu_field)):
        selected = min(samples, key=lambda sample: sample["wall_ms"])
        for field, raw_field in ((wall_field, "wall_ms"), (cpu_field, "cpu_ms")):
            check(row[field] == selected[raw_field],
                  path + ": " + field + " must match the same selected minimum-wall " + name + " sample", errors)


def validate_row(row, path, side, rounds, build, expected, errors):
    start = len(errors)
    if not check(isinstance(row, dict), path + ": JSONL row must be object", errors):
        return None
    check(type(row.get("schema")) is int and row.get("schema") == 1,
          path + ": schema must be integer 1", errors)
    if not check(row.get("kind") in ("meta", "sample", "encode"), path + ": unknown kind", errors):
        return None
    if row["kind"] == "meta":
        # Metadata is optional and never contributes measurements. Validate any
        # claimed identity without requiring harness metadata to repeat it.
        for field, wanted in (("side", side), ("source_sha", build.get("source_sha")),
                              ("binary_sha256", build.get("binary_sha256"))):
            if field in row:
                check(row[field] == wanted, path + ": meta " + field + " identity mismatch", errors)
        return None
    if not validate_case(row, path, errors):
        return None
    key = case_key(row)
    wanted = expected.get(key)
    if wanted is None:
        errors.append(path + ": unexpected case " + case_label(row))
    elif row["kind"] == "sample":
        check(row["compressed_sha256"] == wanted["compressed_sha256"],
              path + ": compressed_sha256 does not match manifest", errors)
    check(type(row.get("round")) is int and 1 <= row["round"] <= rounds,
          path + ": round must be unique integer in 1..%d" % rounds, errors)
    check(row.get("side") == side, path + ": side identity mismatch, expected " + side, errors)
    for field in ("source_sha", "binary_sha256"):
        check(row.get(field) == build.get(field) and hex_value(row.get(field), 40 if field == "source_sha" else 64),
              path + ": " + field + " identity mismatch with manifest", errors)
    for field in ("ruzstd_ms", "ruzstd_cpu_ms", "c_ms", "c_cpu_ms"):
        check(finite_number(row.get(field), positive=True), path + ": " + field + " must be finite > 0", errors)
    validate_samples(row, "impl_samples", "ruzstd_ms", "ruzstd_cpu_ms", path, errors)
    validate_samples(row, "ref_samples", "c_ms", "c_cpu_ms", path, errors)
    if row["kind"] == "encode":
        for byte_field, ratio_field in (("ruzstd_bytes", "ruzstd_ratio"), ("c_bytes", "c_ratio")):
            byte_ok = check(positive_int(row.get(byte_field)), path + ": " + byte_field + " must be positive integer", errors)
            ratio_ok = check(finite_number(row.get(ratio_field), positive=True),
                             path + ": " + ratio_field + " must be finite > 0 (no defaults)", errors)
            if byte_ok and ratio_ok:
                try:
                    ratio = row["bytes"] / row[byte_field]
                    check(finite_number(ratio, positive=True) and same_number(row[ratio_field], ratio),
                          path + ": " + ratio_field + " must equal input bytes / output bytes", errors)
                except OverflowError:
                    errors.append(path + ": unrepresentable byte ratio")
    else:
        check(not any(field in row for field in ("ruzstd_bytes", "c_bytes", "ruzstd_ratio", "c_ratio")),
              path + ": encoding bytes/ratio fields are encode-only", errors)
    return (key, row) if len(errors) == start else None


def load_side(path, side, rounds, build, expected, errors):
    records = {key: {} for key in expected}
    counts = {"file": str(path), "observed_rows": 0, "valid_rows": 0, "meta_rows": 0,
              "expected_rows": len(expected) * rounds}
    seen = set()
    try:
        lines = Path(path).read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as exc:
        errors.append(side + ": cannot read required data file: " + str(exc))
        lines = []
    for line_number, line in enumerate(lines, 1):
        if not line.strip():
            continue
        counts["observed_rows"] += 1
        label = "%s:%d" % (side, line_number)
        try:
            row = strict_json(line)
        except (ValueError, RecursionError) as exc:
            errors.append(label + ": " + str(exc))
            continue
        result = validate_row(row, label, side, rounds, build, expected, errors)
        if isinstance(row, dict) and row.get("kind") == "meta":
            counts["meta_rows"] += 1
        if result is not None:
            key, row = result
            identity = (key, row["round"])
            if identity in seen:
                errors.append(label + ": duplicate round %d for " % row["round"] + case_label(expected[key]))
                continue
            seen.add(identity)
            records[key][row["round"]] = row
            counts["valid_rows"] += 1
    for key, case in expected.items():
        rows = records[key]
        for number in range(1, rounds + 1):
            if number not in rows:
                errors.append(side + ": missing round %d for " % number + case_label(case))
        if case["kind"] == "encode" and rows:
            first = next(iter(rows.values()))
            for number, row in rows.items():
                for field in ("ruzstd_bytes", "c_bytes", "ruzstd_ratio", "c_ratio"):
                    equal = (row[field] == first[field] if field.endswith("bytes")
                             else same_number(row[field], first[field]))
                    check(equal, side + ": inconsistent encoding " + field + " across rounds for " + case_label(case), errors)
    return records, counts


def median(values):
    ordered = sorted(values)
    n = len(ordered)
    if not n:
        return None
    if n % 2:
        return ordered[n // 2]
    return ordered[n // 2 - 1] / 2 + ordered[n // 2] / 2


def percent(after, before):
    # Decimal avoids overflow in intermediate subtraction/division; output is
    # still ordinary JSON numbers, with impossible finite percentages rejected.
    value = float((Decimal(str(after)) / Decimal(str(before)) - 1) * 100)
    if not math.isfinite(value):
        raise ValueError("computed percent difference is outside finite range")
    return value


def side_stats(rows):
    values = list(rows.values())
    result = {"rounds_present": sorted(rows), "round_count": len(values),
              "impl_sample_count": sum(len(row["impl_samples"]) for row in values),
              "ref_sample_count": sum(len(row["ref_samples"]) for row in values)}
    for name, field in (("wall_ms", "ruzstd_ms"), ("cpu_ms", "ruzstd_cpu_ms"),
                        ("reference_wall_ms", "c_ms"), ("reference_cpu_ms", "c_cpu_ms")):
        numbers = [row[field] for row in values]
        result[name] = median(numbers)
        result["diagnostic_min_" + name] = min(numbers) if numbers else None
    if values and values[0]["kind"] == "encode":
        for field in ("ruzstd_bytes", "c_bytes", "ruzstd_ratio", "c_ratio"):
            result[field] = (values[0][field] if field.endswith("bytes")
                             else median([row[field] for row in values]))
    else:
        for field in ("ruzstd_bytes", "c_bytes", "ruzstd_ratio", "c_ratio"):
            result[field] = None
    return result


def paired_metric(before, after, field, base, base2, min_effect, inverse=False):
    rounds = sorted(set(before) & set(after))
    delta = lambda after_value, before_value: (percent(before_value, after_value) if inverse
                                             else percent(after_value, before_value))
    deltas = [{"round": number, "delta_pct": delta(after[number][field], before[number][field])}
              for number in rounds]
    aa = [{"round": number, "delta_pct": delta(base2[number][field], base[number][field])}
          for number in sorted(set(base) & set(base2))]
    noise = max((abs(row["delta_pct"]) for row in aa), default=None)
    return {"round_deltas_pct": deltas,
            "median_delta_pct": median([row["delta_pct"] for row in deltas]),
            "aa_round_deltas_pct": aa, "aa_noise_pct": noise,
            "threshold_pct": max(min_effect, noise) if noise is not None else None}


def classify(paired, ratio_degraded):
    wall, cpu = paired["wall"], paired["cpu"]
    w, c = wall["median_delta_pct"], cpu["median_delta_pct"]
    wt, ct = wall["threshold_pct"], cpu["threshold_pct"]
    if None in (w, c, wt, ct):
        return "NOT_EVALUATED"
    # Output byte counts are exact integers. Float ratio validation tolerance
    # must never become permission to hide even one extra encoded byte.
    if ratio_degraded:
        return "TRADEOFF"
    # Opposite directions are unresolved even if either is below threshold;
    # one significant metric alone never proves a simultaneous gain.
    if w * c < 0:
        return "UNRESOLVED"
    if w < -wt and c < -ct:
        return "GAIN"
    if w > wt and c > ct:
        return "REGRESSION"
    if abs(w) > wt or abs(c) > ct:
        return "UNRESOLVED"
    return "NO_CHANGE"


def compare_case(case, before, after, base, base2, min_effect, before_name="base", after_name="head"):
    paired = {}
    for name, field in (("wall", "ruzstd_ms"), ("cpu", "ruzstd_cpu_ms"),
                        ("reference_wall", "c_ms"), ("reference_cpu", "c_cpu_ms")):
        paired[name] = paired_metric(before, after, field, base, base2, min_effect)
    for name, field in (("ratio", "ruzstd_bytes"), ("reference_ratio", "c_bytes")):
        if case["kind"] == "encode":
            paired[name] = paired_metric(before, after, field, base, base2, min_effect, inverse=True)
        else:
            paired[name] = None
    ratio_degraded = case["kind"] == "encode" and any(
        after[number]["ruzstd_bytes"] > before[number]["ruzstd_bytes"]
        for number in set(before) & set(after))
    return {"case": case, "data_status": "COMPLETE", "paired": paired,
            "sides": {before_name: side_stats(before), after_name: side_stats(after),
                      **({"base2": side_stats(base2)} if before_name == "base" else {})},
            "performance_status": classify(paired, ratio_degraded)}


def primary_performance(cases):
    statuses = [case["performance_status"] for case in cases]
    if "TRADEOFF" in statuses or "UNRESOLVED" in statuses:
        return "NOT_ACCEPTED", False
    if "REGRESSION" in statuses:
        return "REGRESSION", False
    if statuses and "GAIN" in statuses and all(status in ("GAIN", "NO_CHANGE") for status in statuses):
        return "GAIN", True
    return "NO_GAIN", False


def compare(manifest_path, data_dir, min_effect_pct=1.0, *, allow_attachments=True):
    """Return a JSON report; allow_attachments=False forbids nested config suites."""
    errors, warnings = [], []
    if not finite_number(min_effect_pct) or min_effect_pct < 0:
        errors.append("min_effect_pct must be finite and >= 0")
        min_effect_pct = 1.0
    manifest, expected = load_manifest(manifest_path, errors)
    if not allow_attachments:
        declarations = manifest.get("attachments")
        nested = declarations.get("config") if isinstance(declarations, dict) else None
        if isinstance(nested, dict) and nested.get("status") == "COMPLETE":
            errors.append("nested config attachments are forbidden in an independent suite")
    rounds = manifest.get("rounds") if positive_int(manifest.get("rounds")) else 0
    builds = manifest.get("builds") if isinstance(manifest.get("builds"), dict) else {}
    report = {"schema": 1, "data_status": "INCOMPLETE", "performance_status": "NOT_EVALUATED",
              "accepted": False, "errors": errors, "warnings": warnings, "cases": [], "sides": {},
              "provenance": {key: manifest.get(key) for key in ("runner", "rounds", "harness_sha256", "lock_sha256", "builds")},
              "method": {"min_effect_pct": min_effect_pct,
                         "estimator": "median of matched-round head/base percent differences",
                         "noise": "per-case per-metric max absolute matched-round base2/base difference",
                         "threshold": "max(min_effect_pct, per-case A/A noise); strict exceedance",
                         "gate": "accept at least one GAIN, all remaining cases GAIN or NO_CHANGE; no ratio degradation",
                         "claim_scope": "same-run operational screen, not statistical significance or cross-run speedup",
                         "selection": "first minimum-wall sample; its paired CPU; minima diagnostic only",
                         "ratio": "input bytes / output bytes; lower implementation ratio is TRADEOFF"},
              "attachments": {}}
    records = {}
    for side in SIDES:
        build = builds.get(side) if isinstance(builds.get(side), dict) else {}
        records[side], report["sides"][side] = load_side(Path(data_dir) / (side + ".jsonl"), side,
                                                       rounds, build, expected, errors)
    for key, case in expected.items():
        if rounds and all(len(records[side][key]) == rounds for side in SIDES):
            try:
                entry = compare_case(case, records["base"][key], records["head"][key],
                                     records["base"][key], records["base2"][key], min_effect_pct)
            except (ValueError, OverflowError) as exc:
                errors.append(case_label(case) + ": " + str(exc))
                entry = None
        else:
            entry = None
        if entry is None:
            entry = {"case": case, "data_status": "INCOMPLETE", "performance_status": "NOT_EVALUATED",
                     "paired": None, "sides": {side: side_stats(records[side][key]) for side in SIDES}}
        report["cases"].append(entry)
    if not errors:
        report["data_status"] = "COMPLETE"
        report["performance_status"], report["accepted"] = primary_performance(report["cases"])
    else:
        # Valid subsets remain diagnostic only, never a partial pass.
        for entry in report["cases"]:
            entry["performance_status"] = "NOT_EVALUATED"
    attachments = manifest.get("attachments") if isinstance(manifest.get("attachments"), dict) else {}
    for name in ("config", "profile"):
        declaration = attachments.get(name) if isinstance(attachments.get(name), dict) else {}
        status = declaration.get("status") if declaration.get("status") in ATTACHMENT_STATES else "FAILED"
        reason = declaration.get("reason") if isinstance(declaration.get("reason"), str) else "invalid declaration"
        report["attachments"][name] = {"status": status, "reason": reason}
    config = attachments.get("config") if isinstance(attachments.get("config"), dict) else {}
    if config.get("status") == "COMPLETE" and allow_attachments:
        attachment = report["attachments"]["config"]
        attachment_errors = []
        suite = config.get("suite")
        suite_path = None
        if not nonempty_string(suite) or "\x00" in suite or Path(suite).is_absolute() or ".." in Path(suite).parts:
            attachment_errors.append("config: COMPLETE requires a relative suite path inside data-dir")
        else:
            try:
                root = Path(data_dir).resolve()
                candidate = (root / suite).resolve()
                if candidate == root or root not in candidate.parents:
                    attachment_errors.append("config: suite must be a distinct directory inside data-dir")
                else:
                    suite_path = candidate
            except (OSError, ValueError, RuntimeError) as exc:
                attachment_errors.append("config: invalid suite path: " + str(exc))
        attachment["suite"] = suite
        if suite_path is not None:
            independent = compare(suite_path / "manifest.json", suite_path, min_effect_pct,
                                  allow_attachments=False)
            attachment["report"] = independent
            attachment_errors.extend("config suite: " + error for error in independent["errors"])
            suite_builds = independent["provenance"].get("builds")
            primary_head = builds.get("head")
            if not isinstance(suite_builds, dict) or not isinstance(primary_head, dict):
                attachment_errors.append("config: valid suite builds and primary head build are required")
            else:
                for side in ("base", "base2"):
                    check(suite_builds.get(side) == primary_head,
                          "config: " + side + " must be the primary head default source AND binary", attachment_errors)
                tuned_build = suite_builds.get("head")
                check(isinstance(tuned_build, dict) and tuned_build.get("source_sha") == primary_head.get("source_sha"),
                      "config: tuned head source_sha must equal primary head", attachment_errors)
            warnings.extend("config suite: " + warning for warning in independent["warnings"])
        attachment["errors"] = attachment_errors
        if attachment_errors:
            attachment.update(status="FAILED", reason="COMPLETE independent suite failed validation")
            warnings.extend(attachment_errors)
    elif config.get("status") == "COMPLETE":
        report["attachments"]["config"].update(status="FAILED", reason="recursive config suite forbidden")
    # Profile is a declaration only: this comparator neither fabricates phase
    # budgets nor claims to validate absent production profiling probes.
    return report


def md(value):
    return html.escape(str(value), quote=False).replace("|", "&#124;").replace("\r", " ").replace("\n", " ")


def number(value, suffix=""):
    return "N/A" if value is None else "%.4g%s" % (value, suffix)


def render_cases(cases):
    lines = ["| case (full key) | status | Δ wall / CPU / ratio | A/A wall / CPU | threshold wall / CPU |",
             "|---|---|---|---|---|"]
    for entry in cases:
        paired = entry["paired"]
        if paired:
            ratio = paired["ratio"]["median_delta_pct"] if paired["ratio"] else None
            delta = " / ".join(number(value, "%") for value in
                               (paired["wall"]["median_delta_pct"], paired["cpu"]["median_delta_pct"], ratio))
            aa = " / ".join(number(paired[name]["aa_noise_pct"], "%") for name in ("wall", "cpu"))
            threshold = " / ".join(number(paired[name]["threshold_pct"], "%") for name in ("wall", "cpu"))
        else:
            delta = aa = threshold = "N/A (missing/invalid data)"
        lines.append("| %s | %s | %s | %s | %s |" % (
            md(case_label(entry["case"])), entry["performance_status"], delta, aa, threshold))
    lines.extend(["", "| case | side | rounds / impl samples / ref samples | wall / CPU ms | ratio | C wall / CPU ms | C ratio | diagnostic min wall / CPU ms |",
                  "|---|---|---|---|---|---|---|---|"])
    for entry in cases:
        for side, stats in entry["sides"].items():
            lines.append("| %s | %s | %d / %d / %d | %s / %s | %s | %s / %s | %s | %s / %s |" % (
                md(case_label(entry["case"])), side, stats["round_count"], stats["impl_sample_count"], stats["ref_sample_count"],
                number(stats["wall_ms"]), number(stats["cpu_ms"]), number(stats["ruzstd_ratio"]),
                number(stats["reference_wall_ms"]), number(stats["reference_cpu_ms"]), number(stats["c_ratio"]),
                number(stats["diagnostic_min_wall_ms"]), number(stats["diagnostic_min_cpu_ms"])))
    lines.extend(["", "| case | round | Δ wall / CPU / ratio | A/A wall / CPU | C Δ wall / CPU / ratio |",
                  "|---|---|---|---|---|---|"])
    for entry in cases:
        paired = entry["paired"]
        if not paired:
            continue
        maps = {name: {row["round"]: row["delta_pct"] for row in value["round_deltas_pct"]}
                for name, value in paired.items() if value is not None}
        aa_maps = {name: {row["round"]: row["delta_pct"] for row in paired[name]["aa_round_deltas_pct"]}
                   for name in ("wall", "cpu")}
        for round_number in maps["wall"]:
            group = lambda names: " / ".join(number(maps.get(name, {}).get(round_number), "%") for name in names)
            aa = " / ".join(number(aa_maps[name].get(round_number), "%") for name in ("wall", "cpu"))
            lines.append("| %s | %d | %s | %s | %s |" % (
                md(case_label(entry["case"])), round_number, group(("wall", "cpu", "ratio")), aa,
                group(("reference_wall", "reference_cpu", "reference_ratio"))))
    return lines


def render_markdown(report):
    lines = ["# Performance data contract", "", "Data status: **%s**" % report["data_status"],
             "Performance status: **%s**" % report["performance_status"],
             "Explicit performance gate: **%s**" % ("ACCEPTED" if report["accepted"] else "NOT_ACCEPTED"), "",
             "Exit 0 means complete primary data, not a gain. Exit 1 means incomplete data/report I/O failure. "
             "With --fail-on-regression, exit 2 means complete but NOT_ACCEPTED: regression, ratio tradeoff, "
             "unresolved direction, or no gain. Acceptance requires at least one GAIN and all other cases GAIN or NO_CHANGE.",
             "", "Wall/CPU: median of paired-round head/base changes. Each case/metric threshold is "
             "max(min_effect_pct, max absolute paired base2/base A/A fluctuation). Both metrics must strictly "
             "exceed their thresholds in the same direction. This is not statistical significance. "
             "Ratio = input/output bytes; lower implementation ratio is TRADEOFF. Cross-run absolute speedups "
             "are not inferred. Across-round minima are diagnostic only.", "", "## Provenance", ""]
    for key, value in report["provenance"].items():
        lines.append("- %s: %s" % (key, md(json.dumps(value, sort_keys=True, ensure_ascii=False))))
    for title, messages in (("Errors", report["errors"]), ("Warnings", report["warnings"])):
        lines.extend(["", "## " + title, ""])
        lines.extend(["- " + md(message) for message in messages] or ["None."])
    lines.extend(["", "## Primary A/B/A", ""])
    lines.extend(render_cases(report["cases"]))
    for name, attachment in report["attachments"].items():
        lines.extend(["", "## " + name, "", "%s: %s" % (attachment["status"], md(attachment["reason"]))])
        if name == "config" and attachment["status"] == "COMPLETE":
            independent = attachment["report"]
            lines.extend(["", "Independent default/tuned/default A/B/A (not primary-head versus later tuning).",
                          "Data: %s; performance: %s; gate: %s." % (
                              independent["data_status"], independent["performance_status"],
                              "ACCEPTED" if independent["accepted"] else "NOT_ACCEPTED"), ""])
            lines.extend(render_cases(independent["cases"]))
        elif name == "profile":
            lines.append("No phase budget inferred; profile status is a declaration, not validation of profiling artifacts.")
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--data-dir", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--summary", required=True)
    parser.add_argument("--min-effect-pct", type=float, default=1.0)
    parser.add_argument("--fail-on-regression", action="store_true")
    args = parser.parse_args(argv)
    report = compare(args.manifest, args.data_dir, args.min_effect_pct)
    write_failed = False
    for path, content in ((args.output, json.dumps(report, indent=2, ensure_ascii=False, allow_nan=False) + "\n"),
                          (args.summary, render_markdown(report))):
        try:
            target = Path(path)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")
        except OSError as exc:
            print("cannot write report %s: %s" % (path, exc), file=sys.stderr)
            write_failed = True
    print("data=%s performance=%s gate=%s errors=%d warnings=%d" % (
        report["data_status"], report["performance_status"], "ACCEPTED" if report["accepted"] else "NOT_ACCEPTED",
        len(report["errors"]), len(report["warnings"])))
    if write_failed or report["data_status"] != "COMPLETE":
        return 1
    if args.fail_on_regression and not report["accepted"]:
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
