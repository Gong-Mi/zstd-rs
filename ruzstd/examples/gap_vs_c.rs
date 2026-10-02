//! 双实现解压对比 harness（为 CI 用；本地只做构建/断言验证）。
//!
//! 同一进程内同一窗口测两侧：C zstd（dev-dependency `zstd`）与 ruzstd，
//! 每语料 best-of-`--iters`，整轮重复 `--reps` 次。
//!
//! 输出：stderr 人类可读表，stdout 每行一条 JSON（便于 CI 解析与 A/B 比较）。
//! 退出码非 0 = 有语料解压结果与原文不一致（正确性断言失败）。
//!
//! 用法：
//!   cargo run --release --example gap_vs_c -- --reps 5 --iters 7 --size-mb 8
//!
//! 注意（证据边界）：CI runner 是 x86_64，本机是 aarch64。CI 数字用于
//! *相对*比较（同轮 ruzstd/C 比值、base vs head 差值），不可当作 aarch64 绝对值。

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

fn ruzstd_decode(data: &[u8]) -> Vec<u8> {
    use ruzstd::io::Read as _;
    let mut dec = ruzstd::decoding::StreamingDecoder::new(data).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    // 用 `read` 循环而不是 std 的 `read_to_end`：feature 矩阵里 ruzstd 可能不带
    // `std`，那时没有 std 的 blanket 实现（read_to_end 不存在）。
    loop {
        let n = dec.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// best-of over `iters`; returns (wall seconds, CPU seconds).
///
/// CPU time comes from `/proc/self/stat` (utime+stime, CLK_TCK=100 on Linux).
/// It is a much more load-tolerant reference than wall time: it counts only the
/// CPU this process actually burned, so a busy runner or a noisy neighbour
/// inflates it far less than wall clock. Still a reference, not ground truth.
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

/// Process CPU time (user+sys) in seconds, at nanosecond resolution.
///
/// Uses `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` — declared here directly
/// instead of pulling in a `libc` dependency (the value is 2 on Linux for every
/// architecture we care about). This is a much more load-tolerant reference than
/// wall time: it counts only the CPU this process actually burned, so a busy
/// runner slows it far less than wall clock. Still a reference, not ground truth.
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
    // SAFETY: `ts` is a valid, properly aligned `Timespec` for the duration of
    // the call, which is what `clock_gettime` writes into.
    let rc = unsafe { clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &mut ts) };
    if rc != 0 {
        return 0.0;
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut reps, mut iters, mut size_mb) = (5u32, 7u32, 8usize);
    #[allow(unused_mut, unused_assignments)]
    let mut profile = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--reps" => reps = args.next().and_then(|v| v.parse().ok()).unwrap_or(reps),
            "--iters" => iters = args.next().and_then(|v| v.parse().ok()).unwrap_or(iters),
            "--size-mb" => size_mb = args.next().and_then(|v| v.parse().ok()).unwrap_or(size_mb),
            "--profile" => profile = true,
            other => eprintln!("ignoring unknown arg {other}"),
        }
    }
    let size = size_mb * 1024 * 1024;

    let corpora: Vec<(&str, Vec<u8>)> = vec![
        ("text", gen_text(size)),
        ("random", gen_random(size)),
        ("binary-medium", gen_medium(size)),
    ];
    // 用 C 侧编码，两侧解同一份输入（公平对照：压缩产物唯一）
    let encoded: Vec<(&str, Vec<u8>, u32)> = corpora
        .iter()
        .map(|(name, data)| {
            let comp = zstd::encode_all(&data[..], 1).unwrap();
            (*name, comp, data.len() as u32)
        })
        .collect();

    let mut failures = 0usize;
    let mut degenerate: Vec<String> = Vec::new();
    for (name, comp, orig_len) in &encoded {
        let orig = &corpora.iter().find(|(n, _)| n == name).unwrap().1;
        let got = ruzstd_decode(comp);
        if got.len() != *orig_len as usize || &got != orig {
            eprintln!("ASSERT FAIL: {name} ruzstd output != original");
            failures += 1;
        }
        let via_c = zstd::decode_all(&comp[..]).unwrap();
        if &via_c != orig {
            eprintln!("ASSERT FAIL: {name} C output != original (harness bug)");
            failures += 1;
        }
        // 语料退化检测：C 若把整块按 raw 存（比率 ≈1），解码路径根本没被跑到。
        // 阈值只抓"几乎没压缩"，中等压缩比的语料（如 binary-medium）是正常的。
        let ratio = *orig_len as f64 / comp.len().max(1) as f64;
        if ratio < 1.05 {
            degenerate.push(format!("{name}(ratio={ratio:.2})"));
        }
    }
    if !degenerate.is_empty() {
        eprintln!(
            "WARNING: 语料退化，可能未走压缩/序列路径: {}",
            degenerate.join(", ")
        );
    }

    eprintln!(
        "reps={reps} iters={iters} size={size_mb}MB corpora={}",
        encoded.len()
    );
    let ratios: Vec<String> = encoded
        .iter()
        .map(|(name, comp, orig_len)| {
            format!(
                "\"{name}\":{:.2}",
                *orig_len as f64 / comp.len().max(1) as f64
            )
        })
        .collect();
    println!(
        "{{\"kind\":\"meta\",\"reps\":{reps},\"iters\":{iters},\"size_mb\":{size_mb},\"ratios\":{{{}}},\"degenerate\":[{}]}}",
        ratios.join(","),
        degenerate
            .iter()
            .map(|d| format!("\"{d}\""))
            .collect::<Vec<_>>()
            .join(",")
    );

    for rep in 1..=reps {
        for (name, comp, _) in &encoded {
            let (c_wall, c_cpu) = best_of(iters, || {
                let _ = zstd::decode_all(&comp[..]).unwrap();
            });
            let (rs_wall, rs_cpu) = best_of(iters, || {
                let _ = ruzstd_decode(comp);
            });
            eprintln!(
                "  rep {rep} {name:<14} C {:>9.3} ms (cpu {:>8.3})   ruzstd {:>9.3} ms (cpu {:>8.3})   wall ratio {:>5.2}x  cpu ratio {:>5.2}x",
                c_wall * 1000.0,
                c_cpu * 1000.0,
                rs_wall * 1000.0,
                rs_cpu * 1000.0,
                rs_wall / c_wall,
                rs_cpu / c_cpu.max(f64::MIN_POSITIVE)
            );
            println!(
                "{{\"kind\":\"sample\",\"rep\":{rep},\"corpus\":\"{name}\",\"c_ms\":{:.4},\"ruzstd_ms\":{:.4},\"c_cpu_ms\":{:.4},\"ruzstd_cpu_ms\":{:.4}}}",
                c_wall * 1000.0,
                rs_wall * 1000.0,
                c_cpu * 1000.0,
                rs_cpu * 1000.0
            );
        }
    }

    #[cfg(feature = "prof")]
    if profile {
        for (name, comp, orig_len) in &encoded {
            let orig = &corpora.iter().find(|(n, _)| n == name).unwrap().1;
            report_profile(name, comp, *orig_len);
            let _ = orig;
        }
    }
    #[cfg(not(feature = "prof"))]
    if profile {
        eprintln!(
            "--profile needs the `prof` feature: cargo run --features prof --example gap_vs_c"
        );
    }

    if failures > 0 {
        eprintln!("{failures} assertion failure(s)");
        std::process::exit(1);
    }
}

/// One instrumented decode per corpus; prints one JSON line with the block-level
/// split (literals vs sequence loop), the literal composition and the sampled
/// in-loop phase shares. Ratios only — the instrumented build is slower than a
/// clean one, so its wall time is not comparable to anything.
#[cfg(feature = "prof")]
fn report_profile(label: &str, comp: &[u8], orig_len: u32) {
    use ruzstd::decoding::prof as p;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    fn get(c: &AtomicU64) -> f64 {
        c.load(Ordering::Relaxed) as f64
    }

    p::reset();
    let out = ruzstd_decode(comp);
    assert_eq!(
        out.len(),
        orig_len as usize,
        "profile pass decoded a wrong length"
    );

    let lit = get(&p::LITERALS_TICKS);
    let seq = get(&p::SEQ_LOOP_TICKS);
    let phased = (lit + seq).max(1.0);
    let samples = get(&p::SAMPLES).max(1.0);
    let inloop =
        (get(&p::FSE_TICKS) + get(&p::LIT_COPY_TICKS) + get(&p::MATCH_COPY_TICKS)).max(1.0);

    println!(
        "{{\"kind\":\"profile\",\"corpus\":\"{label}\",\"literals_pct\":{:.1},\"seq_loop_pct\":{:.1},\"lit_raw_mb\":{:.2},\"lit_rle_mb\":{:.2},\"lit_huf_mb\":{:.2},\"huf4_sections\":{},\"huf1_sections\":{},\"lit_blocks_raw\":{},\"lit_blocks_rle\":{},\"lit_blocks_huf\":{},\"seqs\":{},\"samples\":{},\"inloop_fse_pct\":{:.1},\"inloop_litcopy_pct\":{:.1},\"inloop_matchcopy_pct\":{:.1}}}",
        get(&p::LITERALS_TICKS) * 100.0 / phased,
        get(&p::SEQ_LOOP_TICKS) * 100.0 / phased,
        get(&p::LIT_RAW_BYTES) / 1048576.0,
        get(&p::LIT_RLE_BYTES) / 1048576.0,
        get(&p::LIT_HUF_BYTES) / 1048576.0,
        get(&p::HUF4_SECTIONS) as u64,
        get(&p::HUF1_SECTIONS) as u64,
        get(&p::LIT_BLOCKS_RAW) as u64,
        get(&p::LIT_BLOCKS_RLE) as u64,
        get(&p::LIT_BLOCKS_HUF) as u64,
        get(&p::SEQS) as u64,
        get(&p::SAMPLES) as u64,
        get(&p::FSE_TICKS) * 100.0 / inloop,
        get(&p::LIT_COPY_TICKS) * 100.0 / inloop,
        get(&p::MATCH_COPY_TICKS) * 100.0 / inloop,
    );
}
