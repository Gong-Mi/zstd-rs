//! 编码侧工作项计数（确定性计数，不是计时；性能判定仍只在 CI）。
#![cfg(feature = "encstats")]
use std::env;
use std::fs;

fn main() {
    let files: Vec<String> = env::args().skip(1).collect();
    println!(
        "{:<20} {:>9} {:>8} {:>8} {:>9} {:>9}",
        "corpus", "raw MB", "ratio", "fse_new", "fse_reuse", "reuse%",
    );
    for path in files {
        let data = fs::read(&path).expect("read");
        let _ = ruzstd::encstats::take();
        let _ = ruzstd::encstats::take_phases();
        let wall0 = std::time::Instant::now();
        let comp = ruzstd::encoding::compress_to_vec(
            &data[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let wall_ms = wall0.elapsed().as_secs_f64() * 1000.0;
        let c = ruzstd::encstats::take();
        let ph = ruzstd::encstats::take_phases();
        let tot: u64 = ph.iter().sum();
        let tot_phases = ph.iter().sum::<u64>() + ph[6];
        let ph6 = ph[6];
        let pct = |i: usize| {
            if tot > 0 {
                100.0 * ph[i] as f64 / tot as f64
            } else {
                0.0
            }
        };
        let name0 = path.rsplit('/').next().unwrap_or(&path).to_string();
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        let ratio = data.len() as f64 / comp.len() as f64;
        let per_byte = (c[0] + c[1]) as f64 / data.len() as f64;
        eprintln!(
            "  {:<20} wall {:>7.1} ms | 匹配 {:>5.1}% 字面量 {:>5.1}% 表构建 {:>5.1}% 表写入 {:>5.1}% 序列编码 {:>5.1}% 其余 {:>5.1}%（相位和 {:>7.1} ms）",
            name0, wall_ms, pct(0), pct(1), pct(2), pct(3), pct(4), pct(5),
            tot as f64 / 1e6
        );
        eprintln!(
            "  {:<20} cmp_calls {:>12} 触及字节 {:>12} 匹配字节 {:>12} 触及/匹配 {:>6.2} | 后缀构建 {:>5.1}%",
            name0,
            c[5],
            c[6],
            c[7],
            if c[7] > 0 { c[6] as f64 / c[7] as f64 } else { 0.0 },
            if tot_phases > 0 { 100.0 * ph6 as f64 / tot_phases as f64 } else { 0.0 }
        );
        let ph7 = ph[7];
        let ph8 = ph[8];
        let ph9 = ph[9];
        if ph7 > 0 {
            eprintln!(
                "  {:<20} 采样(位置内): 比较 {:>5.1}% | 哈希get {:>5.1}% | 窗口迭代与簿记 {:>5.1}%（样本总占比 {:>5.1}%）",
                name0,
                100.0 * ph8 as f64 / ph7.max(1) as f64,
                100.0 * ph9 as f64 / ph7.max(1) as f64,
                100.0 * (ph7 as f64 - ph8 as f64 - ph9 as f64) / ph7.max(1) as f64,
                100.0 * ph7 as f64 / tot_phases.max(1) as f64
            );
        }
        eprintln!(
            "  {:<20} 探测 当前块 {} / 更早块 {} | 命中 当前块 {} / 更早块 {} | 移位 {} | 第二槽非空 {}",
            name0, c[13], c[14], c[11], c[12], c[10], c[9]
        );
        let tot = c[3] + c[4];
        let reuse_pct = if tot > 0 {
            100.0 * c[4] as f64 / tot as f64
        } else {
            0.0
        };
        let _ = per_byte;
        println!(
            "{:<20} {:>9.2} {:>8.3} {:>8} {:>9} {:>8.1}%",
            name,
            data.len() as f64 / 1048576.0,
            ratio,
            c[3],
            c[4],
            reuse_pct
        );
    }
}
