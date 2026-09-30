#!/usr/bin/env python3
"""按 corpus/manifest.txt 取真实语料（下载 + 落盘 + 记哈希）。

- mode=url：直接下载 file_url（支持 HTTP Range 分段，边下边写，避免整包进内存）
- mode=api：调 tools/query_xiaomi_rom.py 按 device/os/android 现查地址，取第一条可用 URL
- 下载后若给了 sha256 则校验；没给就打出来供回填
- extract=kernel 时调用 tools/unpack_kernel.py（boot.img → 内核镜像，失败不致命，退回整份）
- 输出目录固定为 --out（workflow 用 corpus-cache/），文件名 <name>.<bin>

用法：python3 tools/fetch_corpus.py --manifest corpus/manifest.txt --out corpus-cache [--only kernel-real]
"""
import argparse
import hashlib
import json
import os
import subprocess
import sys
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))


def parse_manifest(path):
    entries = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            parts = [p.strip() for p in line.split("|")]
            if len(parts) < 8:
                print(f"WARN: skipping malformed manifest line: {line}", file=sys.stderr)
                continue
            entries.append(
                dict(zip(["name", "mode", "device", "os", "android", "file_url", "sha256", "extract"], parts))
            )
    return entries


def resolve_api(device, os_ver, android):
    """用本地 updater-kmp 脚本查地址，返回 (url, filename, size, md5)。"""
    out = subprocess.run(
        [sys.executable, os.path.join(HERE, "query_xiaomi_rom.py"), "--device", device, "--os", os_ver, "--android", android],
        capture_output=True,
        text=True,
        timeout=120,
    )
    if out.returncode != 0:
        raise RuntimeError(f"query_xiaomi_rom failed: {out.stderr[:300]}")
    data = json.loads(out.stdout)

    def find_urls(obj, acc):
        """递归收集任何 http(s) 字符串，避免依赖 API 的字段结构。"""
        if isinstance(obj, str):
            if obj.startswith("http://") or obj.startswith("https://"):
                acc.append(obj)
        elif isinstance(obj, dict):
            for v in obj.values():
                find_urls(v, acc)
        elif isinstance(obj, list):
            for v in obj:
                find_urls(v, acc)
        return acc

    for rom in data.get("roms", []):
        urls = find_urls(rom, [])
        if urls:
            # 优先整包（ota_full），其次任何可达 URL
            urls.sort(key=lambda u: (0 if "ota_full" in u else 1, len(u)))
            return urls[0], rom.get("filename", "rom.zip"), rom.get("size"), rom.get("md5")
    raise RuntimeError(f"no downloadable rom in api response for {device} {os_ver} {android}")


def download(url, dst, sha256_expected=None, chunk=1 << 20):
    h = hashlib.sha256()
    total = 0
    with urllib.request.urlopen(url, timeout=120) as r, open(dst, "wb") as f:
        while True:
            buf = r.read(chunk)
            if not buf:
                break
            f.write(buf)
            h.update(buf)
            total += len(buf)
    digest = h.hexdigest()
    print(f"  downloaded {total} bytes -> {dst}")
    print(f"  sha256 = {digest}")
    if sha256_expected and sha256_expected not in ("-", "") and digest != sha256_expected:
        raise SystemExit(f"sha256 mismatch for {dst}: expected {sha256_expected}, got {digest}")
    return digest


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--manifest", default="corpus/manifest.txt")
    ap.add_argument("--out", default="corpus-cache")
    ap.add_argument("--only", default=None)
    ap.add_argument("--max-mb", type=int, default=512, help="单文件下载上限（MB）；超限跳过")
    ap.add_argument("--allow-large", action="store_true", help="显式允许超限下载")
    args = ap.parse_args()

    os.makedirs(args.out, exist_ok=True)
    entries = parse_manifest(args.manifest)
    if not entries:
        print("manifest 里没有启用的条目（全是注释）——跳过真实语料腿", file=sys.stderr)
        return 0

    for e in entries:
        if args.only and e["name"] != args.only:
            continue
        dst = os.path.join(args.out, f"{e['name']}.bin")
        if os.path.exists(dst) and os.path.getsize(dst) > 0:
            print(f"[{e['name']}] 命中缓存：{dst}")
            continue
        print(f"[{e['name']}] mode={e['mode']} extract={e['extract']}")
        url = e["file_url"]
        if e["mode"] == "api":
            url, fname, size, md5 = resolve_api(e["device"], e["os"], e["android"])
            print(f"  api resolved: {fname} size={size} md5={md5}")
        elif url in ("-", ""):
            print(f"  SKIP: mode=url 但没给 file_url", file=sys.stderr)
            continue
        # 大小护栏：整包 OTA 是 GB 级，CI 里必须先确认体量（payload Range 只取 boot 分区
        # 的工具到位之前，不在这里悄悄下 GB）
        try:
            req = urllib.request.Request(url, method="HEAD")
            with urllib.request.urlopen(req, timeout=60) as r:
                clen = int(r.headers.get("Content-Length") or 0)
        except Exception as ex:
            print(f"  WARN: HEAD failed ({ex}); 无法确认大小，按超限处理（用 --allow-large 强制）", file=sys.stderr)
            clen = (args.max_mb + 1) * 1024 * 1024
        if clen > args.max_mb * 1024 * 1024 and not args.allow_large:
            print(f"  SKIP: {clen/1048576:.1f} MB > 上限 {args.max_mb} MB（整包 OTA 需先做 payload Range 抽取）",
                  file=sys.stderr)
            continue
        tmp = dst + ".part"
        download(url, tmp, e["sha256"])
        os.replace(tmp, dst)
        if e["extract"] == "kernel":
            subprocess.run(
                [sys.executable, os.path.join(HERE, "unpack_kernel.py"), "--in", dst, "--out", dst + ".kernel"],
                check=False,
            )
            if os.path.exists(dst + ".kernel"):
                os.replace(dst + ".kernel", dst)
                print(f"  extracted kernel -> {dst}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
