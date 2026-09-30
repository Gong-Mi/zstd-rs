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


def detect_and_decompress(data):
    """按魔数识别压缩并解压，返回 (解压后数据, 类型名)。识别不了就原样返回 raw。

    Android boot 里的内核常见：lz4 legacy（0x184C2102，MTK/高通大量使用）、
    lz4 frame、gzip、xz；少数用 zstd（没装 zstandard 库时明确报出来，不静默）。
    """
    if data[:4] == b"\x04\x22\x4d\x18":
        import lz4.frame  # type: ignore

        return lz4.frame.decompress(data), "lz4-frame"
    if data[:4] == b"\x02\x21\x4c\x18":
        # 旧版（legacy）LZ4 帧：magic + 一串 [u32 块头 + 块数据]，块头最高位=1 表示未压缩块，
        # 0 长度块表示结束。python-lz4 的 block API 只吃裸块，所以自己走帧。
        # 不能直接对整段调 block.decompress（没有长度信息，必然 corrupt input）。
        import lz4.block  # type: ignore
        import struct as _s

        out = bytearray()
        i = 4
        while i + 4 <= len(data):
            (bhead,) = _s.unpack_from("<I", data, i)
            i += 4
            blen = bhead & 0x7FFFFFFF
            if blen == 0:
                break
            blk = data[i:i + blen]
            i += blen
            if bhead & 0x80000000:
                out += blk
            else:
                # 裸块没有未压缩长度：给足空间，block.decompress 返回实际解出的字节数
                out += lz4.block.decompress(blk, uncompressed_size=max(blen * 8, 1 << 20))
        return bytes(out), "lz4-legacy"
    if data[:2] == b"\x1f\x8b":
        return gzip.decompress(data), "gzip"
    if data[:6] == b"\xfd7zXZ\x00":
        import lzma

        return lzma.decompress(data), "xz"
    if data[:4] == b"\x28\xb5\x2f\xfd":
        try:
            import zstandard  # type: ignore

            return zstandard.ZstdDecompressor().decompressobj().decompress(data), "zstd"
        except Exception:
            return None, "zstd(lib missing)"
    return data, "raw"


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
    data, kind = detect_and_decompress(blob)
    if data is None:
        open(a.dst, "wb").write(blob)
        print(f"WARNING: kernel 是 {kind} 但我们没有解压库，先原样落盘 {len(blob)} B", file=sys.stderr)
        return 0
    open(a.dst, "wb").write(data)
    print(f"kernel: {size} B on disk, 压缩={kind} -> {len(data)} B")
    if kind == "raw" and size > 1 << 20:
        print("WARNING: 未识别出压缩（原样落盘）；若内核确实是压缩的，语料代表性会变差", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
