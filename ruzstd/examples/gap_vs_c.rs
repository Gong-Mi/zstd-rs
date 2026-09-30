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
use std::io::Read;
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
    let mut dec = ruzstd::decoding::StreamingDecoder::new(data).unwrap();
    let mut out = Vec::new();
    dec.read_to_end(&mut out).unwrap();
    out
}

/// best-of over `iters`; returns seconds.
fn best_of<F: FnMut()>(iters: u32, mut f: F) -> f64 {
    f(); // warmup
    let mut best = f64::MAX;
    for _ in 0..iters {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed().as_secs_f64());
    }
    best
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut reps, mut iters, mut size_mb) = (5u32, 7u32, 8usize);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--reps" => reps = args.next().and_then(|v| v.parse().ok()).unwrap_or(reps),
            "--iters" => iters = args.next().and_then(|v| v.parse().ok()).unwrap_or(iters),
            "--size-mb" => size_mb = args.next().and_then(|v| v.parse().ok()).unwrap_or(size_mb),
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

    eprintln!("reps={reps} iters={iters} size={size_mb}MB corpora={}", encoded.len());
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
            let c = best_of(iters, || {
                let _ = zstd::decode_all(&comp[..]).unwrap();
            });
            let rs = best_of(iters, || {
                let _ = ruzstd_decode(comp);
            });
            eprintln!(
                "  rep {rep} {name:<14} C {:>9.3} ms   ruzstd {:>9.3} ms   ruzstd/C {:>5.2}x",
                c * 1000.0,
                rs * 1000.0,
                rs / c
            );
            println!(
                "{{\"kind\":\"sample\",\"rep\":{rep},\"corpus\":\"{name}\",\"c_ms\":{:.4},\"ruzstd_ms\":{:.4}}}",
                c * 1000.0,
                rs * 1000.0
            );
        }
    }

    if failures > 0 {
        eprintln!("{failures} assertion failure(s)");
        std::process::exit(1);
    }
}
