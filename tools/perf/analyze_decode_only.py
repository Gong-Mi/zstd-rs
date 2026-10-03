#!/usr/bin/env python3
"""Decode-only DIAGNOSTIC adapter; production compare.py is unchanged.

Require the complete mixed manifest with identical binary/input identities.
The only allowed topology difference is removal of encoding cases. Never approve
production, even if the diagnostic classification is GAIN.
"""
import argparse
import json
from pathlib import Path
import compare as core


def sample_key(case):
    return (case["corpus"], case["leg"], case["bytes"], case["sha256"], case["compressed_sha256"])


def index_sample_params(cases, errors, label):
    result = {}
    for case in cases:
        if case["kind"] != "sample":
            continue
        key = sample_key(case)
        if key in result:
            errors.append("ambiguous diagnostic input identity: " + label)
        else:
            result[key] = {k: v for k, v in case["params"].items()
                           if k != "experiment_context"}
    return result


def analyze(manifest, mixed_manifest, data_dir):
    report = core.compare(manifest, data_dir, 1.0, allow_attachments=False)
    report["accepted"] = False
    report["diagnostic_only"] = True
    report["method"]["gate"] = "DIAGNOSTIC ONLY; production approval is forbidden"
    # Do not suppress unrelated corruption, missing samples or identity failures.
    if report["errors"] != ["manifest: required encode case is absent"]:
        report["errors"].append("decode-only contract: expected exactly the missing-encode topology error")
        report["data_status"] = "INCOMPLETE"
        return report
    errors = []
    isolated, expected = core.load_manifest(manifest, errors)
    if errors != ["manifest: required encode case is absent"]:
        return report
    errors = []
    mixed, _ = core.load_manifest(mixed_manifest, errors)
    if errors:
        report["errors"] = ["mixed context: " + e for e in errors]
        report["data_status"] = "INCOMPLETE"
        return report
    builds = isolated["builds"]
    core.check(len({builds[s]["source_sha"] for s in core.SIDES}) == 1,
               "same-image diagnostic must use one source SHA", errors)
    for key in ("runner", "rounds", "builds", "harness_sha256", "lock_sha256"):
        core.check(isolated.get(key) == mixed.get(key), "diagnostic context identity differs: " + key, errors)
    cases = list(expected.values())
    core.check(all(c["kind"] == "sample" and c["params"].get("experiment_context") ==
                   "decode-only/no-rust-encoding" for c in cases), "diagnostic requires decode-only sample cases", errors)
    wanted = index_sample_params(mixed["cases"], errors, "mixed")
    actual = index_sample_params(cases, errors, "isolated")
    core.check(actual.keys() == wanted.keys(), "diagnostic input set differs from mixed samples", errors)
    for key in actual.keys() & wanted.keys():
        core.check(actual[key] == wanted[key], "diagnostic sample params differ between contexts", errors)
    core.check(all(c["params"].get("experiment_context") == "mixed/decode-encode"
                   for c in mixed.get("cases", [])), "mixed manifest lacks explicit mixed context", errors)
    # Explicitly prove no Rust encoding was invoked in ANY isolated row.
    for side, mode in (("base", 0), ("head", 1), ("base2", 0)):
        try:
            for line in (Path(data_dir) / (side + ".jsonl")).read_text().splitlines():
                if not line.strip():
                    continue
                row = core.strict_json(line)
                if not isinstance(row, dict):
                    raise ValueError("diagnostic row must be object")
                if row.get("kind") == "meta":
                    continue
                core.check(type(row.get("rust_encode_calls_so_far")) is int and
                           row["rust_encode_calls_so_far"] == 0,
                           "diagnostic Rust encoding counter nonzero/missing: " + side, errors)
                core.check(type(row.get("diagnostic_mode")) is int and row["diagnostic_mode"] == mode,
                           "diagnostic runtime mode mismatch: " + side, errors)
        except (OSError, ValueError, RecursionError) as exc:
            errors.append("diagnostic reread failed: " + side + ": " + str(exc))
    report["errors"] = errors
    if errors:
        report["data_status"] = "INCOMPLETE"
        report["performance_status"] = "NOT_EVALUATED"
    else:
        report["data_status"] = "COMPLETE"
        for entry in report["cases"]:
            entry["performance_status"] = core.classify(entry["paired"], False)
        report["performance_status"] = core.primary_performance(report["cases"])[0]
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--mixed-manifest", required=True)
    parser.add_argument("--data-dir", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    report = analyze(args.manifest, args.mixed_manifest, args.data_dir)
    Path(args.output).write_text(json.dumps(report, indent=2) + "\n")
    print("DIAGNOSTIC data=" + report["data_status"] + " performance=" + report["performance_status"] + " approval=FORBIDDEN")
    raise SystemExit(0 if report["data_status"] == "COMPLETE" else 1)


if __name__ == "__main__":
    main()
