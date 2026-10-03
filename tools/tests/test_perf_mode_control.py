#!/usr/bin/env python3
"""Fixture tests for binary-control validation, NOT native execution evidence."""
import importlib.util
import os
from pathlib import Path
import struct
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("control", Path(__file__).parents[1] / "perf" / "prepare_mode_control.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def fixture(flags=6):
    data = bytearray(120)
    data[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<HH", data, 54, 56, 1)
    data.extend(MODULE.PREFIX + b"\x01" + MODULE.SUFFIX)
    struct.pack_into("<IIQQQQQQ", data, 64, 1, flags, 0, 0, 0, len(data), len(data), 4096)
    return bytes(data)


class ModeControlTests(unittest.TestCase):
    def test_exactly_one_data_byte_changes(self):
        image = fixture()
        result, offset = MODULE.derive_disabled(image)
        self.assertEqual([i for i, (a, b) in enumerate(zip(image, result)) if a != b], [offset])
        self.assertEqual(image[offset], 1)
        self.assertEqual(result[offset], 0)

    def test_reject_executable_or_readonly_segment(self):
        for flags in (5, 7, 4):
            with self.assertRaises(ValueError):
                MODULE.derive_disabled(fixture(flags))

    def test_reject_missing_or_duplicate_marker(self):
        for image in (fixture().replace(MODULE.PREFIX, b"x" * 16), fixture() + MODULE.PREFIX + b"\x01" + MODULE.SUFFIX):
            with self.assertRaises(ValueError):
                MODULE.derive_disabled(image)

    def test_reject_invalid_header(self):
        for image in (b"not ELF", fixture()[:64], fixture().replace(b"\x02\x01", b"\x01\x01", 1)):
            with self.assertRaises(ValueError):
                MODULE.derive_disabled(image)

    @unittest.skipUnless(hasattr(os, "link"), "hardlink API unavailable; run this oracle on Linux CI")
    def test_hardlink_alias_is_rejected_before_writing(self):
        with tempfile.TemporaryDirectory() as directory:
            p = Path(directory); head = p / "head"; base = p / "base"
            head.write_bytes(fixture())
            os.link(head, base)
            with self.assertRaises(ValueError):
                MODULE.prepare(base, head, p)
            self.assertEqual(head.read_bytes(), fixture())

    def test_prepare_records_real_byte_difference_and_refuses_repeat(self):
        with tempfile.TemporaryDirectory() as directory:
            p = Path(directory); head = p / "head"; base = p / "base"
            head.write_bytes(fixture()); base.write_bytes(fixture())
            for side in ("base", "head"):
                (p / (side + "-source.txt")).write_text("0" * 40)
            info = MODULE.prepare(base, head, p)
            self.assertTrue(info["diagnostic_only"])
            self.assertEqual(info["changed_byte_count"], 1)
            self.assertEqual(head.read_bytes()[info["mode_offset"]], 1)
            self.assertEqual(base.read_bytes()[info["mode_offset"]], 0)
            before = base.read_bytes()
            with self.assertRaises(FileExistsError):
                MODULE.prepare(base, head, p)
            self.assertEqual(base.read_bytes(), before)

    def test_source_mismatch_is_rejected_before_writing(self):
        with tempfile.TemporaryDirectory() as directory:
            p = Path(directory); head = p / "head"; base = p / "base"
            head.write_bytes(fixture()); base.write_bytes(fixture())
            (p / "base-source.txt").write_text("0" * 40)
            (p / "head-source.txt").write_text("1" * 40)
            with self.assertRaises(ValueError):
                MODULE.prepare(base, head, p)
            self.assertEqual(base.read_bytes(), fixture())


if __name__ == "__main__":
    unittest.main()
