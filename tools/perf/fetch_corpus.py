#!/usr/bin/env python3
"""Optional CI-only acquisition of an existing corpus-prepare artifact.

No OTA download/extraction here. A failure is explicit and independent of the
required synthetic/source A/B legs. Only bounded .bin inputs are extracted.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import stat
import subprocess
import sys
import zipfile


LIMIT = 256 * 1024 * 1024


def gh(*args):
    result = subprocess.run(["gh", *args], check=True, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=90)
    return json.loads(result.stdout)


def acquire(root, repository):
    runs = gh("run", "list", "--repo", repository, "--workflow", "corpus prepare",
              "--status", "success", "--limit", "20", "--json", "databaseId,createdAt")
    now = datetime.now(timezone.utc)
    fresh = [r for r in runs if 0 <= (now - datetime.fromisoformat(
        r["createdAt"].replace("Z", "+00:00"))).total_seconds() < 259200]
    if not fresh:
        return {"status": "NOT_RUN", "reason": "no successful prepare within three days"}
    run = max(fresh, key=lambda r: r["createdAt"])
    artifacts = gh("api", f"repos/{repository}/actions/runs/{run['databaseId']}/artifacts?per_page=100")
    if artifacts.get("total_count", 0) != len(artifacts.get("artifacts", [])):
        raise ValueError("artifact enumeration incomplete")
    candidates = [a for a in artifacts["artifacts"]
                  if a["name"] == "corpus-cache" and not a["expired"]]
    if not candidates:
        return {"status": "NOT_RUN", "reason": "prepare has no current corpus-cache artifact",
                "prepare_run": run["databaseId"]}
    artifact = max(candidates, key=lambda a: a["id"])
    if not 0 < artifact["size_in_bytes"] <= LIMIT:
        raise ValueError("artifact outside 256 MiB acquisition budget")
    archive = root / "corpus.zip"
    with archive.open("wb") as output:
        subprocess.run(["gh", "api", f"repos/{repository}/actions/artifacts/{artifact['id']}/zip"],
                       stdout=output, stderr=subprocess.PIPE, timeout=90, check=True)
    if archive.stat().st_size > LIMIT:
        raise ValueError("download exceeds acquisition budget")
    files = []
    with zipfile.ZipFile(archive) as zipped:
        members = [m for m in zipped.infolist() if m.filename.endswith(".bin")]
        if not members or sum(m.file_size for m in members) > LIMIT:
            raise ValueError("no binary corpus or expanded inputs exceed budget")
        names = set()
        for member in members:
            name = PurePosixPath(member.filename)
            if (name.is_absolute() or ".." in name.parts or "\\" in member.filename
                    or stat.S_ISLNK(member.external_attr >> 16) or name.name in names):
                raise ValueError("unsafe/duplicate corpus member")
            names.add(name.name)
        # Publish .bin files only after every member was read and CRC-verified.
        for member in members:
            name = PurePosixPath(member.filename).name
            data = zipped.read(member)
            temporary = root / (name + ".partial")
            temporary.write_bytes(data)
            files.append({"name": name, "bytes": len(data),
                          "sha256": hashlib.sha256(data).hexdigest()})
        for file in files:
            (root / (file["name"] + ".partial")).replace(root / file["name"])
    return {"status": "COMPLETE", "reason": "exact artifact ID, ZIP CRC and input hashes recorded",
            "prepare_run": run["databaseId"], "artifact_id": artifact["id"],
            "files": files}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--enabled", choices=("true", "false"), default="false")
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
    args = parser.parse_args()
    root = args.output_dir.resolve()
    root.mkdir(parents=True, exist_ok=False)
    if args.enabled == "false":
        status = {"status": "NOT_RUN", "reason": "not requested; no network access"}
    else:
        try:
            if not args.repo:
                raise ValueError("repository identity missing")
            status = acquire(root, args.repo)
        except (OSError, ValueError, KeyError, TypeError, zipfile.BadZipFile,
                subprocess.SubprocessError) as exc:
            status = {"status": "FAILED", "reason": str(exc)}
    (root / "status.json").write_text(json.dumps(status, indent=2) + "\n")
    print(json.dumps(status))
    if args.enabled == "true" and status["status"] != "COMPLETE":
        print("::warning::real corpus " + status["status"] + ": " + status["reason"])
    # Optional acquisition is not the required A/B failure domain.
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
