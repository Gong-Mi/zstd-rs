#!/usr/bin/env python3
"""Generate small deterministic LZ counterexamples on the benchmark runner."""
import argparse
import hashlib
import json
import os
from pathlib import Path

SIZE = 128 * 1024
MASK = (1 << 64) - 1


def payloads():
    period = bytes(range(256)) * (SIZE // 256)
    state = 7
    prefix = bytearray()
    for _ in range(4096):
        state ^= (state << 13) & MASK
        state ^= state >> 7
        state ^= (state << 17) & MASK
        prefix.append(state & 255)
    prefix[:256] = bytes(range(256))
    token = b"the repeated tail still has long distance matches "
    needed = SIZE - len(prefix)
    tail = (token * ((needed + len(token) - 1) // len(token)))[:needed]
    return {"period-256.bin": period, "prefix-repeated-tail.bin": bytes(prefix) + tail}


def generate(output):
    output = Path(output)
    data = payloads()
    if output.is_symlink():
        raise FileExistsError("refusing a symlink output directory")
    output.mkdir(parents=True, exist_ok=True)
    directory = os.open(output, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        names = list(data) + ["policy-manifest.json"]
        for name in names:
            try:
                os.stat(name, dir_fd=directory, follow_symlinks=False)
            except FileNotFoundError:
                continue
            raise FileExistsError("refusing an existing experiment input: " + name)
        manifest = [{"name": name, "bytes": len(content),
                     "sha256": hashlib.sha256(content).hexdigest()}
                    for name, content in data.items()]
        contents = dict(data)
        contents["policy-manifest.json"] = (json.dumps(manifest, indent=2) + "\n").encode()
        for name, content in contents.items():
            descriptor = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                 0o600, dir_fd=directory)
            with os.fdopen(descriptor, "wb") as stream:
                stream.write(content)
        return manifest
    finally:
        os.close(directory)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", required=True)
    args = parser.parse_args()
    print(json.dumps(generate(args.output_dir)))
