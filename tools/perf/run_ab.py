#!/usr/bin/env python3
"""Run a bounded, identity-bound A/B suite; never interpret local timings as gains.

The immutable plan is written BEFORE measurement. Results live outside the corpus
checkout so a newly written JSONL/report cannot silently change repo-sources.
Configuration experiments use their own default/tuned/default interleaved suite.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def harness_identity(directory):
    root = Path(directory)
    files = [root / "Cargo.toml", root / "build.rs", *sorted((root / "src").rglob("*.rs"))]
    if len(files) < 2:
        raise ValueError("measurement source missing")
    digest = hashlib.sha256()
    for path in files:
        digest.update(str(path.relative_to(root)).encode() + b"\0")
        digest.update(path.read_bytes() + b"\0")
    return digest.hexdigest(), sha256(root / "Cargo.lock")


def runner_fingerprint():
    cpu = "unknown"
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith(("model name", "Hardware")):
                cpu = line.partition(":")[2].strip()
                break
    except OSError:
        pass
    return (f"{platform.machine()} cpu={cpu} nproc={os.cpu_count()} "
            f"runner={os.environ.get('RUNNER_NAME', 'local-NOT-PERFORMANCE-ACCEPTANCE')}")


def positive(value):
    n = int(value)
    if n <= 0 or n > 100:
        raise argparse.ArgumentTypeError("expected bounded integer 1..100")
    return n


def argv(args):
    result = ["--iters", str(args.iters), "--size-mb", str(args.size_mb),
              "--encode-mb", str(args.encode_mb), "--repo-dir", str(args.repo_dir)]
    for path in args.corpus_file:
        result += ["--corpus-file", path]
    return result


def execute(binary, command, env, log, timeout, output=None):
    with Path(log).open("ab") as stderr:
        if output is None:
            result = subprocess.run([str(binary), *command], env=env, stdout=subprocess.PIPE,
                                    stderr=stderr, timeout=timeout, check=True)
            return result.stdout
        with Path(output).open("ab") as stdout:
            subprocess.run([str(binary), *command], env=env, stdout=stdout,
                           stderr=stderr, timeout=timeout, check=True)
    return None


def suite(args, directory, base_bin, head_bin, base_source, head_source, rounds):
    directory.mkdir(parents=True, exist_ok=False)
    builds = {
        "base": {"source_sha": base_source, "binary_sha256": sha256(base_bin)},
        "head": {"source_sha": head_source, "binary_sha256": sha256(head_bin)},
    }
    builds["base2"] = dict(builds["base"])
    bins = {"base": base_bin, "head": head_bin, "base2": base_bin}
    common = argv(args)
    env = os.environ.copy()
    harness_sha, lock_sha = harness_identity(args.harness_dir)
    identities = {}
    for side in ("base", "head"):
        identity = json.loads(execute(bins[side], ["--identity"], env,
                                      directory / "run.log", args.timeout))
        if identity.get("source_sha") != builds[side]["source_sha"]:
            raise ValueError("binary source identity differs from actual build: " + side)
        if (identity.get("harness_sha256") != harness_sha or
                identity.get("lock_sha256") != lock_sha):
            raise ValueError("binary measurement harness/dependency identity mismatch: " + side)
        identities[side] = identity
    (directory / "binary-identities.json").write_text(json.dumps(identities, indent=2) + "\n")
    plan = json.loads(execute(head_bin, [*common, "--plan"], env,
                              directory / "run.log", args.timeout))
    if plan.get("schema") != 1 or not plan.get("cases"):
        raise ValueError("harness produced no valid expected plan")
    manifest = {
        "schema": 1, "rounds": rounds, "runner": runner_fingerprint(),
        "harness_sha256": harness_sha, "lock_sha256": lock_sha,
        "builds": builds, "cases": plan["cases"],
        "attachments": {
            "config": {"status": "NOT_RUN", "reason": "not requested"},
            "profile": {"status": "NOT_RUN", "reason":
                        "current production ref has no validated instrumentation; no phase/budget claim"},
        },
    }
    (directory / "plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    (directory / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (directory / "environment.json").write_text(json.dumps({
        "runner": manifest["runner"], "python": sys.version,
        "build_env": {k: os.environ.get(k, "") for k in
                      ("RUSTFLAGS", "CARGO_PROFILE_RELEASE_LTO", "CARGO_PROFILE_RELEASE_CODEGEN_UNITS")},
        "note": "raw samples are evidence only in the CI runner/context that generated them",
    }, indent=2) + "\n")
    for side in bins:
        (directory / (side + ".jsonl")).touch()
    for round_number in range(1, rounds + 1):
        for side in ("base", "head", "base2"):
            identity = builds[side]
            if sha256(bins[side]) != identity["binary_sha256"]:
                raise ValueError("binary changed after manifest was frozen: " + side)
            env = dict(os.environ, PERF_SOURCE_SHA=identity["source_sha"],
                       PERF_BINARY_SHA256=identity["binary_sha256"])
            with (directory / "run.log").open("a") as log:
                log.write(f"round={round_number} side={side} binary={identity['binary_sha256']}\n")
            execute(bins[side], [*common, "--round", str(round_number), "--side", side],
                    env, directory / "run.log", args.timeout,
                    directory / (side + ".jsonl"))
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-bin", type=Path, required=True)
    parser.add_argument("--head-bin", type=Path, required=True)
    parser.add_argument("--base-source", required=True)
    parser.add_argument("--head-source", required=True)
    parser.add_argument("--repo-dir", type=Path, required=True)
    parser.add_argument("--harness-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--rounds", type=positive, default=3)
    parser.add_argument("--iters", type=positive, default=3)
    parser.add_argument("--size-mb", type=positive, default=1)
    parser.add_argument("--encode-mb", type=positive, default=1)
    parser.add_argument("--timeout", type=positive, default=90)
    parser.add_argument("--corpus-file", action="append", default=[])
    parser.add_argument("--tuned-bin", type=Path)
    parser.add_argument("--config-failed", default="")
    args = parser.parse_args()
    for sha in (args.base_source, args.head_source):
        if re.fullmatch("[a-f0-9]{40}", sha) is None:
            parser.error("source identities must be resolved full lowercase git SHAs")
    args.repo_dir = args.repo_dir.resolve(strict=True)
    args.output_dir = args.output_dir.resolve()
    args.base_bin = args.base_bin.resolve(strict=True)
    args.head_bin = args.head_bin.resolve(strict=True)
    args.harness_dir = args.harness_dir.resolve(strict=True)
    if args.output_dir.is_relative_to(args.repo_dir):
        parser.error("results must live outside the corpus checkout")
    if args.output_dir.exists():
        parser.error("output directory already exists; use a fresh exact-run path")
    if args.size_mb > 64 or args.encode_mb > 64:
        parser.error("64 MiB per-corpus limit")
    manifest = suite(args, args.output_dir, args.base_bin, args.head_bin,
                     args.base_source, args.head_source, args.rounds)
    if args.tuned_bin:
        try:
            suite(args, args.output_dir / "config", args.head_bin,
                  args.tuned_bin.resolve(strict=True), args.head_source,
                  args.head_source, args.rounds)
            manifest["attachments"]["config"] = {
                "status": "COMPLETE", "reason": "paired default/tuned/default suite",
                "suite": "config",
            }
        except (OSError, ValueError, subprocess.SubprocessError) as exc:
            manifest["attachments"]["config"] = {"status": "FAILED", "reason": str(exc)}
    elif args.config_failed:
        manifest["attachments"]["config"] = {"status": "FAILED", "reason": args.config_failed}
    (args.output_dir / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps({"data_dir": str(args.output_dir), "expected_cases": len(manifest["cases"]),
                      "rounds": args.rounds, "attachments": manifest["attachments"]}))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        print("A/B execution FAILED: " + str(exc), file=sys.stderr)
        raise SystemExit(1)
