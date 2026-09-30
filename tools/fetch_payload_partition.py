#!/usr/bin/env python3
"""从远端 OTA zip 里只取一个分区（默认 boot），全程 HTTP Range，不整包下载。

链路：miotaV3 解析地址（复用 query_xiaomi_rom.py）→ 远端 zip 用 Range 当随机读文件
（zipfile 只读中央目录）→ 取出 payload.bin 头部（读 manifest 用的那段）→ 手解
update_metadata protobuf 到"某分区的 operations"→ 逐 op Range 取数据、按类型
（REPLACE / REPLACE_XZ / REPLACE_BZ / ZERO）解压并写到 dst 偏移。

这是"验证过就算数"的工具：所有 Range 读都打印 offset/length，便于核对没有整包下载。

用法：
  python3 tools/fetch_payload_partition.py --device violin --os OS2.0.203.0.VOTCNXM \
      --android 15.0 --partition boot --out corpus-cache/violin-boot.img
  # 也可 --url <直链> 跳过 API
"""
import argparse
import io
import json
import lzma
import os
import struct
import subprocess
import sys
import urllib.request
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
UA = {"User-Agent": "curl/8"}


class HttpRangeFile(io.RawIOBase):
    """只读文件对象：seek/read 都翻译成 HTTP Range 请求。

    复用同一条 HTTPS 连接（keep-alive），否则每次小 Range 都要重做 TLS 往返，
    在慢链路上 4KB 一次会慢到分钟级。
    """

    def __init__(self, url, verbose=True, max_bytes=32 << 20, log=None):
        self.url = url
        self.pos = 0
        self.verbose = verbose
        self._len = None
        self._reads = 0
        # 硬预算：整包 7.6GB，任何"读到底"都必须被拦住；超预算直接抛错
        self.max_bytes = max_bytes
        self.transferred = 0
        self.log = log
        from urllib.parse import urlsplit
        u = urlsplit(url)
        self._host = u.hostname
        self._path = u.path + (("?" + u.query) if u.query else "")
        self._conn = None

    def _connection(self):
        import http.client
        if self._conn is None:
            self._conn = http.client.HTTPSConnection(self._host, timeout=60)
        return self._conn

    def _get(self, extra_headers=None):
        import http.client
        headers = {"User-Agent": "curl/8", "Connection": "keep-alive"}
        if extra_headers:
            headers.update(extra_headers)
        for attempt in range(3):
            try:
                conn = self._connection()
                conn.request("GET", self._path, headers=headers)
                resp = conn.getresponse()
                return resp
            except Exception:
                # 连接断了就重建再试
                self._conn = None
        raise RuntimeError("range request failed after retries")

    def _size(self):
        if self._len is None:
            resp = self._get({"Range": "bytes=0-0"})
            crange = resp.getheader("Content-Range") or ""
            resp.read()
            if "/" in crange:
                self._len = int(crange.rsplit("/", 1)[1])
            else:
                self._len = int(resp.getheader("Content-Length") or 0)
            if self.verbose:
                print(f"  [range] remote size = {self._len/1048576:.1f} MB", file=sys.stderr)
        return self._len

    def seekable(self):
        return True

    def readable(self):
        return True

    def seek(self, off, whence=0):
        if whence == 0:
            self.pos = off
        elif whence == 1:
            self.pos += off
        else:
            self.pos = self._size() + off
        return self.pos

    def tell(self):
        return self.pos

    def readinto(self, b):
        data = self.read(len(b))
        b[:len(data)] = data
        return len(data)

    def read(self, n=-1):
        size = self._size()
        if self.pos >= size:
            return b""
        if n is None or n < 0:
            # 无长度读：只放行"剩余很小"的尾读（zipfile 读 EOCD 就会这样），
            # 剩余很大时一律拒绝——否则 7.6GB 远端上那就是整包下载。
            remaining = size - self.pos
            if remaining > (1 << 20):
                raise RuntimeError(
                    f"unbounded read refused (pos={self.pos}, remaining={remaining}); 用显式长度读"
                )
            n = remaining
        n = min(n, size - self.pos)
        end = self.pos + n - 1
        resp = self._get({"Range": f"bytes={self.pos}-{end}"})
        data = resp.read()
        self._reads += 1
        self.transferred += len(data)
        line = f"[range] #{self._reads} {self.pos}..{end} ({len(data)} B, 累计 {self.transferred/1048576:.2f} MB)"
        if self.log:
            self.log.write(line + "\n")
            self.log.flush()
        if self.verbose and (self._reads <= 20 or self._reads % 50 == 0):
            print("  " + line, file=sys.stderr)
        if self.transferred > self.max_bytes:
            raise RuntimeError(
                f"byte budget exceeded: {self.transferred} > {self.max_bytes} B"
            )
        self.pos += len(data)
        return data


# ── 最小 protobuf 走查（只取本工具需要的字段） ──────────────────────────────
def read_varint(buf, i):
    shift = 0
    val = 0
    while True:
        b = buf[i]
        i += 1
        val |= (b & 0x7F) << shift
        if not b & 0x80:
            return val, i
        shift += 7


def iter_fields(buf):
    i = 0
    n = len(buf)
    while i < n:
        key, i = read_varint(buf, i)
        field, wire = key >> 3, key & 7
        if wire == 0:
            val, i = read_varint(buf, i)
            yield field, wire, val
        elif wire == 2:
            ln, i = read_varint(buf, i)
            yield field, wire, buf[i:i + ln]
            i += ln
        elif wire == 5:
            yield field, wire, buf[i:i + 4]
            i += 4
        elif wire == 1:
            yield field, wire, buf[i:i + 8]
            i += 8
        else:
            raise ValueError(f"unsupported wire type {wire}")


def parse_manifest(mf):
    """DeltaArchiveManifest: partitions(13) -> {name(1), operations(8)};
    InstallOperation: type(1), data_offset(2), data_length(3), dst_extents(6: Extent{start(1),num_blocks(2)})"""
    parts = []
    for field, wire, val in iter_fields(mf):
        if field == 13 and wire == 2:
            name = None
            ops = []
            for f2, w2, v2 in iter_fields(val):
                if f2 == 1 and w2 == 2:
                    name = v2.decode()
                elif f2 == 8 and w2 == 2:
                    op = {"type": 0, "data_offset": 0, "data_length": 0, "dst": []}
                    for f3, w3, v3 in iter_fields(v2):
                        if f3 == 1 and w3 == 0:
                            op["type"] = v3
                        elif f3 == 2 and w3 == 0:
                            op["data_offset"] = v3
                        elif f3 == 3 and w3 == 0:
                            op["data_length"] = v3
                        elif f3 == 6 and w3 == 2:
                            ext = {"start": 0, "num_blocks": 0}
                            for f4, w4, v4 in iter_fields(v3):
                                if f4 == 1 and w4 == 0:
                                    ext["start"] = v4
                                elif f4 == 2 and w4 == 0:
                                    ext["num_blocks"] = v4
                            op["dst"].append(ext)
                    ops.append(op)
            if name:
                parts.append({"name": name, "ops": ops})
    return parts


def resolve_url(device, os_ver, android):
    out = subprocess.run(
        [sys.executable, os.path.join(HERE, "query_xiaomi_rom.py"), "--device", device, "--os", os_ver, "--android", android],
        capture_output=True, text=True, timeout=180,
    )
    if out.returncode != 0:
        raise SystemExit(f"query failed: {out.stderr[:400]}")
    data = json.loads(out.stdout)

    def find(obj, acc):
        if isinstance(obj, str):
            if obj.startswith("http"):
                acc.append(obj)
        elif isinstance(obj, dict):
            for v in obj.values():
                find(v, acc)
        elif isinstance(obj, list):
            for v in obj:
                find(v, acc)
        return acc

    for rom in data.get("roms", []):
        urls = find(rom, [])
        if urls:
            def rank(u):
                full = 0 if "ota_full" in u else 1
                # 镜像偏好：aliyun OSS > superota > ultimateota > 其它（cdnorg 常最慢）
                if "oss" in u or "aliyun" in u:
                    mir = 0
                elif "superota" in u:
                    mir = 1
                elif "ultimateota" in u:
                    mir = 2
                else:
                    mir = 3
                return (full, mir, len(u))

            urls.sort(key=rank)
            print(f"  {len(urls)} 个镜像候选，选: {urls[0][:100]}")
            return urls[0], rom.get("filename")
    raise SystemExit("no downloadable rom resolved")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--device")
    ap.add_argument("--os")
    ap.add_argument("--android")
    ap.add_argument("--url")
    ap.add_argument("--manifest", default=None, help="从 corpus/manifest.txt 里按 --name 取条目")
    ap.add_argument("--name", default=None, help="清单里的语料名（如 kernel-real）")
    ap.add_argument("--partition", default="boot")
    ap.add_argument("--out", required=True)
    ap.add_argument("--quiet", action="store_true")
    ap.add_argument("--dry-run", action="store_true", help="只解析到分区/op 列表，不下载数据")
    ap.add_argument("--max-mb", type=int, default=32, help="本次允许的 Range 传输总量上限（MB）")
    ap.add_argument("--log", default=None, help="Range 记账写到此文件（默认 stderr 摘要）")
    a = ap.parse_args()

    if a.manifest and not a.url:
        for line in open(a.manifest):
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            f = [x.strip() for x in line.split("|")]
            if len(f) >= 8 and (a.name is None or f[0] == a.name):
                a.url, a.device, a.os, a.android = (f[5] if f[1] == "url" else None), f[2], f[3], f[4]
                print(f"manifest entry: {f[0]} mode={f[1]} device={f[2]} os={f[3]} android={f[4]}")
                break
        else:
            raise SystemExit(f"manifest 里没有 {a.name} 的可用条目")

    url = a.url
    if not url:
        url, fname = resolve_url(a.device, a.os, a.android)
        print(f"resolved: {fname}\n  url: {url[:120]}...")
    logf = open(a.log, "w") if a.log else None
    rf = HttpRangeFile(url, verbose=not a.quiet, max_bytes=a.max_mb * 1024 * 1024, log=logf)

    zf = zipfile.ZipFile(rf)
    names = zf.namelist()
    print(f"  zip entries: {len(names)}; payload={'payload.bin' in names}")
    if "payload.bin" not in names:
        print("  没有 payload.bin（可能是 .tgz fastboot 包）", file=sys.stderr)
        return 2

    # 不走 ZipExtFile（它按 4KB 缓冲，取 100MB 分区要上万次 Range）：自算 payload.bin
    # 的数据起点，然后按 op 的 [data_offset, data_length] 一次性大块 Range 读。
    info = zf.getinfo("payload.bin")
    rf.seek(info.header_offset)
    lh = rf.read(30)
    nlen = struct.unpack_from("<H", lh, 26)[0]
    elen = struct.unpack_from("<H", lh, 28)[0]
    data_start = info.header_offset + 30 + nlen + elen
    print(f"  payload.bin: local header @{info.header_offset}, data @{data_start}, "
          f"size={info.file_size/1048576:.1f} MB")

    def read_at(off, ln):
        rf.seek(data_start + off)
        return rf.read(ln)

    magic, _ver, msize = struct.unpack("<4sQQ", read_at(0, 20))
    assert magic == b"CrAU", f"payload magic = {magic!r}"
    print(f"  manifest size = {msize/1024:.0f} KiB")
    # 分块读 manifest：一次要几十 MB 时某些 CDN 会长时间不返回，分块＋逐块进度能把
    # "卡在哪一块"看出来（上一轮就是整块读 manifest 卡了 20 分钟直到超时）
    CHUNK = 256 * 1024
    mf = bytearray()
    off = 20
    while len(mf) < msize:
        n = min(CHUNK, msize - len(mf))
        mf += read_at(off, n)
        off += n
        print(f"    manifest {len(mf)/1024:.0f}/{msize/1024:.0f} KiB, "
              f"Range 累计 {rf.transferred/1048576:.2f} MB")
    mf = bytes(mf)
    parts = parse_manifest(mf)
    target = next((p for p in parts if p["name"] == a.partition), None)
    if not target:
        print(f"  没有分区 {a.partition}；可选: {sorted(p['name'] for p in parts)}", file=sys.stderr)
        return 2
    ops = target["ops"]
    total = sum(o["data_length"] for o in ops)
    print(f"  {a.partition}: {len(ops)} ops, 数据合计 {total/1048576:.1f} MB（每 op 一次大块 Range）")

    out_size = 0
    for o in ops:
        for e in o["dst"]:
            out_size = max(out_size, e["start"] + e["num_blocks"])
    out_size *= 4096
    buf = bytearray(out_size)
    if a.dry_run:
        print("  --dry-run：停在这里（未下载任何 op 数据）")
        print("  分区列表:", sorted(p["name"] for p in parts))
        print("  boot ops 前 3:", json.dumps(ops[:3]))
        return 0
    import time as _t
    t0 = _t.time()
    for idx, o in enumerate(ops):
        if o["data_length"] == 0:
            continue
        raw = read_at(o["data_offset"], o["data_length"])
        t = o["type"]
        if t == 8:      # REPLACE_XZ
            data = lzma.decompress(raw)
        elif t == 1:    # REPLACE_BZ
            data = __import__("bz2").decompress(raw)
        else:           # REPLACE / 其它按原样
            data = raw
        pos = 0
        for e in o["dst"]:
            s0 = e["start"] * 4096
            length = e["num_blocks"] * 4096
            buf[s0:s0 + length] = data[pos:pos + length]
            pos += length
        if idx % 5 == 0 or idx == len(ops) - 1:
            print(f"    op {idx+1}/{len(ops)} type={t} len={o['data_length']} "
                  f"累计传输 {rf.transferred/1048576:.1f} MB, {_t.time()-t0:.0f}s")

    os.makedirs(os.path.dirname(os.path.abspath(a.out)), exist_ok=True)
    open(a.out, "wb").write(bytes(buf))
    print(f"wrote {a.out} ({out_size/1048576:.1f} MB)")
    print(f"Range 记账：{rf._reads} 次请求，累计传输 {rf.transferred/1048576:.2f} MB（上限 {a.max_mb} MB）")
    if logf:
        logf.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
