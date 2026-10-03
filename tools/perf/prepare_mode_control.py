#!/usr/bin/env python3
"""Diagnostic only: OFF/ON binaries from ONE linked image, one byte apart.
Source identity must match; record discarded base build and changed offset.
Not a source-only production acceptance run.
"""
import argparse
import hashlib
import json
import os
import re
import struct
from pathlib import Path

PREFIX = b"RUZSTD_HUFFBOUND"
SUFFIX = b"_MODE_CONTROL_AB"


def writable_nonexecuting_data(image, offset):
    if len(image) < 64 or image[:6] != b"\x7fELF\x02\x01":
        raise ValueError("diagnostic requires a 64-bit little-endian ELF")
    phoff = struct.unpack_from("<Q", image, 32)[0]
    entry_size, count = struct.unpack_from("<HH", image, 54)
    if entry_size != 56 or phoff + entry_size * count > len(image):
        raise ValueError("invalid program header table")
    for index in range(count):
        fields = struct.unpack_from("<IIQQQQQQ", image, phoff + entry_size * index)
        kind, flags, start, _, _, size, _, _ = fields
        if kind == 1 and flags & 2 and not flags & 1 and start <= offset < start + size:
            return
    raise ValueError("mode byte is not in writable non-executable PT_LOAD data")


def derive_disabled(image):
    if not image.startswith(b"\x7fELF"):
        raise ValueError("not ELF")
    marker = PREFIX + b"\x01" + SUFFIX
    if image.count(marker) != 1:
        raise ValueError("expected exactly one enabled diagnostic control record")
    offset = image.index(marker) + len(PREFIX)
    writable_nonexecuting_data(image, offset)
    changed = bytearray(image)
    changed[offset] = 0
    disabled = bytes(changed)
    assert len(disabled) == len(image)
    assert image[:offset] == disabled[:offset] and image[offset + 1:] == disabled[offset + 1:]
    return disabled, offset


def prepare(base, head, records):
    if os.path.samefile(base, head):
        raise ValueError("base/head alias")
    base_source = (records / "base-source.txt").read_text().strip()
    head_source = (records / "head-source.txt").read_text().strip()
    if re.fullmatch("[a-f0-9]{40}", base_source) is None or re.fullmatch("[a-f0-9]{40}", head_source) is None:
        raise ValueError("source identity is not a full lowercase git SHA")
    if base_source != head_source:
        raise ValueError("same-image diagnostic requires identical source SHA")
    if (records / "mode-control.json").exists():
        raise FileExistsError("existing diagnostic evidence; refuse a second write")
    original_base = base.read_bytes()
    image = head.read_bytes()
    disabled, offset = derive_disabled(image)
    info = {
        "diagnostic_only": True,
        "source_sha": head_source,
        "one_linked_image": True,
        "mode_offset": offset,
        "base_mode": 0,
        "head_mode": 1,
        "changed_byte_count": 1,
        "discarded_base_build_sha256": hashlib.sha256(original_base).hexdigest(),
        "head_shared_image_sha256": hashlib.sha256(image).hexdigest(),
        "base_derived_image_sha256": hashlib.sha256(disabled).hexdigest(),
        "note": "No instruction/code placement change between modes; one mutable data byte only.",
    }
    base.write_bytes(disabled)
    assert base.read_bytes() == disabled and head.read_bytes() == image
    (records / "mode-control.json").write_text(json.dumps(info, indent=2) + "\n")
    return info


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--head", type=Path, required=True)
    parser.add_argument("--records", type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(prepare(args.base, args.head, args.records)))


if __name__ == "__main__":
    main()
