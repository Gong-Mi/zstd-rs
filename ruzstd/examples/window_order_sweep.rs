//! 窗口条目走查顺序 × 次选入围规则 × 窗口大小 的压缩率扫描（`encstats` 特性门控，诊断用）。
//!
//! 回答「窗口变大为什么在 src 上反而更差」。只跑与本片代码相关的上下游：编码输出尺寸 + 自解码。
//! 默认组合（oldest-first × rule0）= 生产行为，尺寸必须与未插桩构建逐一相同。
//!
//! 用法：`window_order_sweep <input>...`
#![cfg(feature = "encstats")]
use std::env;
use std::fs;
use std::io::Read;

use ruzstd::encstats as es;
use ruzstd::encoding::{CompressionLevel, FrameCompressor, MatchGeneratorDriver};

const SLICE: usize = 128 * 1024;
/// (窗口片数, 走查顺序 newest-first?, 入围规则)
const COMBOS: &[(usize, bool, u8)] = &[
    (1, false, 0), // 生产默认 = 128KiB 窗口
    (1, true, 0),  // 只改顺序
    (4, false, 0), // 512KiB 窗口，现行顺序（已知在 src 上变差）
    (4, true, 0),  // 512KiB + 最新先走
    (4, true, 4),  // 512KiB + 最新先走 + 代价感知入围
    (8, false, 0), // 1MiB 窗口，现行顺序
    (8, true, 4),  // 1MiB + 最新先走 + 代价感知
];

fn main() {
    let files: Vec<String> = env::args().skip(1).collect();
    assert!(!files.is_empty(), "window_order_sweep <input>...");
    print!("{:<12} {:>9}", "corpus", "raw KB");
    for (slices, nf, rule) in COMBOS {
        print!(
            "{:>16}",
            format!("{}K{}{}", SLICE * slices / 1024, if *nf { "-nf" } else { "" }, *rule)
        );
    }
    println!();
    for path in files {
        let data = fs::read(&path).expect("read input");
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        print!("{:<12} {:>9.0}", name, data.len() as f64 / 1024.0);
        let mut base = 0usize;
        for (slices, newest_first, rule) in COMBOS {
            es::set_window_order_newest_first(*newest_first);
            es::set_cand_rule(*rule);
            let mut comp = FrameCompressor::new_with_matcher(
                MatchGeneratorDriver::new(SLICE, *slices),
                CompressionLevel::Fastest,
            );
            comp.set_source(data.as_slice());
            let mut out = Vec::new();
            comp.set_drain(&mut out);
            comp.compress();
            let mut decoded = Vec::new();
            ruzstd::decoding::StreamingDecoder::new(&out[..])
                .expect("decoder init")
                .read_to_end(&mut decoded)
                .expect("decode");
            assert_eq!(decoded, data, "{name} combo {slices}/{newest_first}/{rule} 自解码不一致");
            if base == 0 {
                base = out.len();
            }
            let pct = 100.0 * (out.len() as f64 - base as f64) / base as f64;
            print!("{:>10}({:+.1}%)", out.len(), pct);
        }
        es::set_window_order_newest_first(false);
        es::set_cand_rule(0);
        println!();
    }
}
