//! CI perf harness：每语料 × 每 API 腿（streaming / known-size）× 两侧（C zstd / ruzstd），
//! 另加一条编码腿（时间 + ratio）。
//!
//! 为什么分腿（issue #11 §3）：ruzstd 的 StreamingDecoder 要 64KB 读循环 + 未知尺寸增长，
//! 而 C 侧 decode_all 直写调用方容量；混在一行会把"API 契约成本"算进实现差距。
//! known-size 腿走 ruzstd 自带的 `FrameDecoder::decode_all(src, &mut [u8])`。
//!
//! 真实语料腿（issue #11 §5）：把 checkout 里按路径排序的源文件拼起来，是对冲"语料全合成"
//! 的最小手段（真实熵分布、确定、无网络依赖）。
//!
//! 证据边界：CI runner 是 x86_64、本机是 aarch64，比值随机器变（issue #11 §1/§6），
//! 所以只比同一 run 内的差值，且报告必须带 runner 指纹。

use rand::{Rng, SeedableRng};
use std::time::Instant;

fn gen_text(size: usize) -> Vec<u8> {
    let words: [&[u8]; 16] = [
        b"the ", b"of ", b"and ", b"to ", b"in ", b"that ", b"he ", b"was ", b"it ", b"his ",
        b"with ", b"is ", b"for ", b"as ", b"had ", b"be ",
    ];
    let mut rng = rand::rngs::SmallRng::seed_from_u64(42);
    let mut buf = Vec::with_capacity(size);
    while buf.len() < size {
        buf.extend_from_slice(words[rng.gen_range(0..words.len())]);
    }
    buf.truncate(size);
    buf
}

fn gen_random(size: usize) -> Vec<u8> {
    let mut rng = rand::rngs::SmallRng::seed_from_u64(7);
    let mut buf = vec![0u8; size];
    rng.fill(&mut buf[..]);
    buf
}

fn gen_medium(size: usize) -> Vec<u8> {
    let mut rng = rand::rngs::SmallRng::seed_from_u64(99);
    let mut buf = Vec::with_capacity(size);
    while buf.len() < size {
        let tag: u32 = rng.gen_range(0..64);
        let len: u32 = rng.gen_range(8..256);
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        for _ in 0..len {
            if rng.gen_bool(0.6) {
                buf.push(rng.gen_range(0..16));
            } else {
                buf.push(rng.gen());
            }
        }
    }
    buf.truncate(size);
    buf
}

/// 仿真内核风格源码（pseudo-random，确定种子）：关键字/标识符/缩进/注释 +
/// 重复行，token 频率明显偏斜——比 16 词表的 text 语料接近真实 C 代码的熵结构。
/// 不是真数据，是"结构仿造"，用来替代"必须下载内核源码"这类重依赖语料。
fn gen_src_like(size: usize) -> Vec<u8> {
    const KWS: [&[u8]; 22] = [
        b"static ", b"int ", b"struct ", b"const ", b"unsigned long ", b"void ", b"return ",
        b"if (", b") {\n", b"}\n", b"for (", b";\n", b" = ", b"->", b"/* ", b" */", b"\n\t",
        b"EXPORT_SYMBOL(", b"__init", b"#define ", b"#include <linux/", b">\n",
    ];
    const IDS: [&[u8]; 18] = [
        b"page", b"sk_buff", b"alloc", b"mutex_lock", b"vmalloc", b"inode", b"task_struct",
        b"list_head", b"spinlock", b"kmalloc", b"rcu_read_lock", b"dma_addr_t", b"workqueue",
        b"kmem_cache", b"jiffies", b"refcount", b"hlist_node", b"bio",
    ];
    let mut rng = rand::rngs::SmallRng::seed_from_u64(0xC0FFEE);
    let mut buf = Vec::with_capacity(size + 4096);
    let mut line_start = 0usize;
    while buf.len() < size {
        let r: u32 = rng.gen_range(0..100);
        if r < 45 {
            let k = KWS[rng.gen_range(0..KWS.len())];
            buf.extend_from_slice(k);
        } else if r < 80 {
            let i = IDS[rng.gen_range(0..IDS.len())];
            buf.extend_from_slice(i);
        } else if r < 92 {
            // 重复上一行（真实代码里大量重复的调用/声明）
            let l = buf.len().min(buf.len() - line_start);
            if l > 0 {
                let from = buf.len() - l;
                let take = l.min(120);
                buf.extend_from_within(from..from + take);
            }
        } else {
            buf.push(rng.gen_range(b'a'..=b'z'));
        }
        if buf.len() - line_start > 80 && rng.gen_bool(0.5) {
            buf.push(b'\n');
            line_start = buf.len();
        }
    }
    buf.truncate(size);
    buf
}

/// 仿真 ELF 二进制（pseudo-random，确定种子）：ELF 头 + 代码段字节分布
/// （x86-64 常见 opcode/ModRM 前缀）+ 零填充 + 字符串表 + 指针数组。
fn gen_bin_like(size: usize) -> Vec<u8> {
    const OPS: [u8; 20] = [
        0x48, 0x8b, 0x89, 0xe8, 0x0f, 0x1f, 0x00, 0x48, 0x89, 0xc7, 0xeb, 0x74, 0x75, 0x31,
        0xc0, 0xff, 0x25, 0xc3, 0x55, 0x5d,
    ];
    let mut rng = rand::rngs::SmallRng::seed_from_u64(0xBADC0DE);
    let mut buf = Vec::with_capacity(size + 4096);
    buf.extend_from_slice(b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00");
    buf.extend_from_slice(&[0x02, 0x00, 0x3e, 0x00]); // ET_EXEC, x86-64
    while buf.len() < size {
        let r: u32 = rng.gen_range(0..100);
        if r < 40 {
            // 代码段：opcode 偏斜
            let n = rng.gen_range(4..24);
            for _ in 0..n {
                buf.push(OPS[rng.gen_range(0..OPS.len())]);
            }
        } else if r < 55 {
            // 对齐/填充：零串
            let n = rng.gen_range(8..64);
            buf.extend_from_slice(&vec![0u8; n]);
        } else if r < 70 {
            // 指针/地址数组：重复的高位字节 + 低熵尾巴
            let n = rng.gen_range(4..16);
            for _ in 0..n {
                buf.extend_from_slice(&[0x00, 0x00, 0x40, 0x00]);
                buf.extend_from_slice(&(rng.gen_range(0u32..0x0040_0000u32)).to_le_bytes());
            }
        } else if r < 88 {
            // 字符串表：短符号名
            let n = rng.gen_range(3..10);
            for _ in 0..n {
                buf.push(rng.gen_range(b'a'..=b'z'));
            }
            buf.push(0);
        } else {
            let n = rng.gen_range(1..40);
            for _ in 0..n {
                buf.push(rng.gen());
            }
        }
    }
    buf.truncate(size);
    buf
}

/// 从目录收集文件拼成语料（按路径排序，确定）。`kind` 决定收哪些文件：
/// `src` = 源码/文本类扩展名；`bin` = ELF 二进制（按魔数判断，不看扩展名）。
/// 用于真实语料：Linux 内核源码/头文件、系统里的真实二进制。
fn collect_from_dir(dir: &str, kind: &str, size: usize) -> Vec<u8> {
    fn walk(dir: &std::path::Path, kind: &str, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == ".git" || name == "target" {
                    continue;
                }
                if p.is_dir() {
                    walk(&p, kind, out);
                } else if kind == "bin" {
                    // ELF 魔数，避免把脚本/文本混进二进制语料
                    if let Ok(mut f) = std::fs::File::open(&p) {
                        let mut magic = [0u8; 4];
                        if std::io::Read::read_exact(&mut f, &mut magic).is_ok()
                            && &magic == b"\x7fELF"
                            && p.metadata().map(|m| m.len() >= 4096).unwrap_or(false)
                        {
                            out.push(p);
                        }
                    }
                } else if let Some(ext) = p.extension().and_then(|x| x.to_str()) {
                    if matches!(
                        ext,
                        "rs" | "c" | "h" | "md" | "toml" | "json" | "yml" | "yaml" | "py" | "txt"
                    ) {
                        out.push(p);
                    }
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(std::path::Path::new(dir), kind, &mut files);
    files.sort();
    let mut buf = Vec::with_capacity(size + (1 << 16));
    for f in files {
        if buf.len() >= size {
            break;
        }
        if let Ok(data) = std::fs::read(&f) {
            buf.extend_from_slice(&data);
        }
    }
    if buf.is_empty() {
        eprintln!("WARNING: no files collected from {dir} (kind={kind})");
    }
    buf.truncate(size);
    buf
}

/// 真实数据腿：checkout 内按路径排序拼接源文件（确定、无网络）。
fn gen_repo_sources(dir: &str, size: usize) -> Vec<u8> {
    collect_from_dir(dir, "src", size)
}

fn ruzstd_decode_stream(data: &[u8]) -> Vec<u8> {
    use ruzstd::io::Read as _;
    let mut dec = ruzstd::decoding::StreamingDecoder::new(data).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = dec.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// known-size 腿：调用方先给容量，直接写进 &mut [u8]。
fn ruzstd_decode_known(data: &[u8], out: &mut [u8]) -> usize {
    let mut dec = ruzstd::decoding::FrameDecoder::new();
    dec.decode_all(data, out).unwrap()
}

/// best-of over `iters`; returns (wall seconds, CPU seconds).
fn best_of<F: FnMut()>(iters: u32, mut f: F) -> (f64, f64) {
    f(); // warmup
    let mut best_wall = f64::MAX;
    let mut best_cpu = f64::MAX;
    for _ in 0..iters {
        let cpu0 = cpu_seconds();
        let t = Instant::now();
        f();
        let wall = t.elapsed().as_secs_f64();
        let cpu = (cpu_seconds() - cpu0).max(0.0);
        best_wall = best_wall.min(wall);
        best_cpu = best_cpu.min(cpu);
    }
    (best_wall, best_cpu)
}

/// Process CPU time (user+sys), nanosecond resolution.
fn cpu_seconds() -> f64 {
    const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    extern "C" {
        fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32;
    }
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, aligned Timespec for the duration of the call.
    let rc = unsafe { clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    if rc != 0 {
        return 0.0;
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

/// Runner 指纹：比值随机器变，报告必须带上（issue #11 §1/§6）。
fn runner_fingerprint() -> String {
    let mut cpu = String::from("unknown");
    if let Ok(s) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in s.lines() {
            if line.starts_with("model name") || line.starts_with("Hardware") {
                if let Some(v) = line.split(':').nth(1) {
                    cpu = v.trim().to_string();
                    break;
                }
            }
        }
    }
    let nproc = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    format!(
        "{} nproc={} cpu={} runner={}",
        std::env::consts::ARCH,
        nproc,
        cpu,
        std::env::var("RUNNER_NAME").unwrap_or_else(|_| "-".into())
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut reps, mut iters, mut size_mb, mut encode_mb) = (5u32, 7u32, 8usize, 4usize);
    let mut repo_dir = String::from(".");
    let mut profile = false;
    let mut corpus_files: Vec<(String, String)> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--reps" => reps = args.next().and_then(|v| v.parse().ok()).unwrap_or(reps),
            "--iters" => iters = args.next().and_then(|v| v.parse().ok()).unwrap_or(iters),
            "--size-mb" => size_mb = args.next().and_then(|v| v.parse().ok()).unwrap_or(size_mb),
            "--encode-mb" => {
                encode_mb = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(encode_mb)
            }
            "--repo-dir" => repo_dir = args.next().unwrap_or(repo_dir),
            // --corpus-file PATH[:NAME]：把外部取来的真实语料（内核镜像等）加进语料表
            "--corpus-file" => {
                if let Some(spec) = args.next() {
                    let (path, name) = match spec.rsplit_once(':') {
                        Some((p, n)) if !n.contains('/') => (p.to_string(), n.to_string()),
                        _ => {
                            let n = std::path::Path::new(&spec)
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("external")
                                .to_string();
                            (spec.clone(), n)
                        }
                    };
                    corpus_files.push((path, name));
                }
            }
            "--profile" => profile = true,
            other => eprintln!("ignoring unknown arg {other}"),
        }
    }
    let size = size_mb * 1024 * 1024;

    let mut corpora: Vec<(String, Vec<u8>)> = vec![
        ("text".into(), gen_text(size)),
        ("random".into(), gen_random(size)),
        ("binary-medium".into(), gen_medium(size)),
        ("src-like".into(), gen_src_like(size)),
        ("bin-like".into(), gen_bin_like(size)),
        ("repo-sources".into(), gen_repo_sources(&repo_dir, size)),
    ];
    for (path, name) in &corpus_files {
        match std::fs::read(path) {
            Ok(mut data) => {
                data.truncate(size);
                eprintln!("external corpus {name}: {} bytes from {path}", data.len());
                corpora.push((name.clone(), data));
            }
            Err(e) => eprintln!("WARNING: cannot read external corpus {path}: {e}"),
        }
    }
    let corpora: Vec<(&str, Vec<u8>)> = corpora
        .iter()
        .map(|(n, d)| (n.as_str(), d.clone()))
        .collect();
    let encoded: Vec<(&str, Vec<u8>, u32)> = corpora
        .iter()
        .map(|(name, data)| {
            let comp = zstd::encode_all(&data[..], 1).unwrap();
            (*name, comp, data.len() as u32)
        })
        .collect();

    let fp = runner_fingerprint();
    eprintln!("=== runner: {fp} ===");
    println!(
        "{{\"kind\":\"meta\",\"reps\":{reps},\"iters\":{iters},\"size_mb\":{size_mb},\"encode_mb\":{encode_mb},\"runner\":\"{}\"}}",
        fp.replace('"', "'")
    );

    // ── 正确性断言：两条腿都必须与原文逐字节一致 ──
    let mut failures = 0usize;
    let mut known_buf: Vec<u8> = Vec::new();
    for (name, comp, orig_len) in &encoded {
        let orig = &corpora.iter().find(|(n, _)| n == name).unwrap().1;
        if &ruzstd_decode_stream(comp) != orig {
            eprintln!("ASSERT FAIL: {name} streaming output != original");
            failures += 1;
        }
        known_buf.resize(*orig_len as usize, 0);
        let n = ruzstd_decode_known(comp, &mut known_buf);
        if n != *orig_len as usize || &known_buf[..n] != &orig[..] {
            eprintln!("ASSERT FAIL: {name} known-size output != original");
            failures += 1;
        }
        if &zstd::decode_all(&comp[..]).unwrap() != orig {
            eprintln!("ASSERT FAIL: {name} C output != original");
            failures += 1;
        }
    }

    for rep in 1..=reps {
        for (name, comp, orig_len) in &encoded {
            let mut known = vec![0u8; *orig_len as usize];

            let (c_wall, c_cpu) = best_of(iters, || {
                let _ = zstd::decode_all(&comp[..]).unwrap();
            });
            let (rs_wall, rs_cpu) = best_of(iters, || {
                let _ = ruzstd_decode_stream(comp);
            });
            eprintln!(
                "  rep {rep} {:<14} stream     C {:>8.3} ms   ruzstd {:>8.3} ms   ratio {:>5.2}x",
                name,
                c_wall * 1000.0,
                rs_wall * 1000.0,
                rs_wall / c_wall
            );
            println!(
                "{{\"kind\":\"sample\",\"rep\":{rep},\"leg\":\"stream\",\"corpus\":\"{name}\",\"c_ms\":{:.4},\"ruzstd_ms\":{:.4},\"c_cpu_ms\":{:.4},\"ruzstd_cpu_ms\":{:.4}}}",
                c_wall * 1000.0,
                rs_wall * 1000.0,
                c_cpu * 1000.0,
                rs_cpu * 1000.0
            );

            let (ck_wall, ck_cpu) = best_of(iters, || {
                let _ = zstd::bulk::decompress(&comp[..], *orig_len as usize).unwrap();
            });
            let (rk_wall, rk_cpu) = best_of(iters, || {
                let _ = ruzstd_decode_known(comp, &mut known);
            });
            eprintln!(
                "  rep {rep} {:<14} known-size C {:>8.3} ms   ruzstd {:>8.3} ms   ratio {:>5.2}x",
                name,
                ck_wall * 1000.0,
                rk_wall * 1000.0,
                rk_wall / ck_wall
            );
            println!(
                "{{\"kind\":\"sample\",\"rep\":{rep},\"leg\":\"known\",\"corpus\":\"{name}\",\"c_ms\":{:.4},\"ruzstd_ms\":{:.4},\"c_cpu_ms\":{:.4},\"ruzstd_cpu_ms\":{:.4}}}",
                ck_wall * 1000.0,
                rk_wall * 1000.0,
                ck_cpu * 1000.0,
                rk_cpu * 1000.0
            );
        }
    }

    // ── 编码腿：时间 + ratio（短语料、单轮，控制时长） ──
    let enc_size = encode_mb * 1024 * 1024;
    for (name, data) in &corpora {
        let src = &data[..enc_size.min(data.len())];
        let c_comp = zstd::encode_all(src, 1).unwrap();
        let (c_wall, c_cpu) = best_of(1, || {
            let _ = zstd::encode_all(src, 1).unwrap();
        });
        let mut rs_out = Vec::new();
        ruzstd::encoding::compress(
            src,
            &mut rs_out,
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let (rs_wall, rs_cpu) = best_of(1, || {
            let mut out = Vec::new();
            ruzstd::encoding::compress(src, &mut out, ruzstd::encoding::CompressionLevel::Fastest);
        });
        let ratio_c = src.len() as f64 / c_comp.len().max(1) as f64;
        let ratio_rs = src.len() as f64 / rs_out.len().max(1) as f64;
        eprintln!(
            "  encode {name:<14} C {:>8.3} ms (ratio {:.2})   ruzstd {:>8.3} ms (ratio {:.2})",
            c_wall * 1000.0,
            ratio_c,
            rs_wall * 1000.0,
            ratio_rs
        );
        println!(
            "{{\"kind\":\"encode\",\"corpus\":\"{name}\",\"bytes\":{},\"c_ms\":{:.4},\"ruzstd_ms\":{:.4},\"c_cpu_ms\":{:.4},\"ruzstd_cpu_ms\":{:.4},\"c_ratio\":{:.3},\"ruzstd_ratio\":{:.3}}}",
            src.len(),
            c_wall * 1000.0,
            rs_wall * 1000.0,
            c_cpu * 1000.0,
            rs_cpu * 1000.0,
            ratio_c,
            ratio_rs
        );
    }

    #[cfg(feature = "prof")]
    if profile {
        for (name, comp, _orig_len) in &encoded {
            report_profile(name, comp);
        }
    }
    #[cfg(not(feature = "prof"))]
    if profile {
        eprintln!(
            "--profile needs the `prof` feature: cargo run --features prof --example perf_ab"
        );
    }

    if failures > 0 {
        eprintln!("{failures} assertion failure(s)");
        std::process::exit(1);
    }
}

/// 每个语料跑一次带插桩的解码，输出一行 JSON：块级拆分（字面量 vs 序列循环）
/// 与循环内采样相位占比。只看占比——插桩版墙钟没有意义。
#[cfg(feature = "prof")]
fn report_profile(label: &str, comp: &[u8]) {
    use ruzstd::decoding::prof as p;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    fn get(c: &AtomicU64) -> f64 {
        c.load(Ordering::Relaxed) as f64
    }

    p::reset();
    let _ = ruzstd_decode_stream(comp);
    let lit = get(&p::LITERALS_TICKS);
    let seq = get(&p::SEQ_LOOP_TICKS);
    let phased = (lit + seq).max(1.0);
    let inloop =
        (get(&p::FSE_TICKS) + get(&p::LIT_COPY_TICKS) + get(&p::MATCH_COPY_TICKS)).max(1.0);
    println!(
        "{{\"kind\":\"profile\",\"corpus\":\"{label}\",\"literals_pct\":{:.1},\"seq_loop_pct\":{:.1},\"lit_raw_mb\":{:.2},\"lit_rle_mb\":{:.2},\"lit_huf_mb\":{:.2},\"huf4_sections\":{},\"huf1_sections\":{},\"seqs\":{},\"samples\":{},\"inloop_fse_pct\":{:.1},\"inloop_litcopy_pct\":{:.1},\"inloop_matchcopy_pct\":{:.1}}}",
        lit * 100.0 / phased,
        seq * 100.0 / phased,
        get(&p::LIT_RAW_BYTES) / 1048576.0,
        get(&p::LIT_RLE_BYTES) / 1048576.0,
        get(&p::LIT_HUF_BYTES) / 1048576.0,
        get(&p::HUF4_SECTIONS) as u64,
        get(&p::HUF1_SECTIONS) as u64,
        get(&p::SEQS) as u64,
        get(&p::SAMPLES) as u64,
        get(&p::FSE_TICKS) * 100.0 / inloop,
        get(&p::LIT_COPY_TICKS) * 100.0 / inloop,
        get(&p::MATCH_COPY_TICKS) * 100.0 / inloop,
    );
}
