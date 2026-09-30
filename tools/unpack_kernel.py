#!/usr/bin/env python3
"""从 Android boot.img / init_boot.img 里解出内核镜像（boot header v0-v4）。

支持：v0-v2（kernel_size/kernel_addr 字段）、v3/v4（kernel_size 字段 + 页对齐）。
压缩：lz4 / gzip / 未压缩。lz4 需要 `pip install lz4`（CI 里装；装不上就退回整份）。
失败一律非致命（返回非 0，由调用方决定回退到整份当语料）。

用法：python3 tools/unpack_kernel.py --in boot.img --out kernel.bin
"""
import argparse
import gzip
import struct
import sys

MAGIC = b"ANDROID!"
VENDOR_MAGIC = b"VNDRBOOT"


def parse_header(b):
    magic = b[:8]
    if magic == b"VNDRBOOT":  # pragma: no cover - placeholder
        return None
    if magic != MAGIC:
        return None
    kernel_size = struct.unpack_from("<I", b, 8)[0]
    kernel_addr = struct.unpack_from("<I", b, 12)[0]
    page_size = struct.unpack_from("<I", b, 36)[0] or 2048
    # v3/v4 的 header 版本在偏移 40（v0 没有），用 kernel_size 是否为 0 与 page_size 合法性判断
    header_version = struct.unpack_from("<I", b, 40)[0] if len(b) >= 44 else 0
    os_version = struct.unpack_from("<I", b, 44)[0] if len(b) >= 48 else 0
    return dict(kernel_size=kernel_size, kernel_addr=kernel_addr, page_size=page_size,
                header_version=header_version if header_version < 8 else 0, os_version=os_version)


def decompress(data):
    if data[:4] == b"\x04\x22\x4d\x18":  # LZ4 frame
        try:
            import lz4.frame  # type: ignore
        except Exception:
            print("lz4 not available; keeping compressed payload", file=sys.stderr)
            return None
        return lz4.frame.decompress(data)
    if data[:2] == b"\x1f\x8b":
        return gzip.decompress(data)
    return data  # 未压缩


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--in", dest="src", required=True)
    ap.add_argument("--out", dest="dst", required=True)
    a = ap.parse_args()
    b = open(a.src, "rb").read()
    h = parse_header(b)
    if not h:
        print("not an ANDROID! boot image", file=sys.stderr)
        return 1
    page = h["page_size"]
    off = page
    size = h["kernel_size"] or (len(b) - page)
    blob = b[off:off + size]
    if h["header_version"] >= 3 and size == 0:
        print("v3+ header with unknown kernel size; falling back", file=sys.stderr)
        return 2
    raw = decompress(blob)
    if raw is None:
        open(a.dst, "wb").write(blob)
        return 0
    open(a.dst, "wb").write(raw)
    print(f"kernel: {size} bytes on disk -> {len(raw)} bytes raw")
    return 0


if __name__ == "__main__":
    sys.exit(main())
