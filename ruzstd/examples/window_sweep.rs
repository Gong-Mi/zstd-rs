//! 窗口大小扫描（对齐 C 的 `windowLog` 口径的对照实验）。
//!
//! 本库默认窗口 = `MatchGeneratorDriver::new(128KiB, 1)`，即 128KiB；C 的 `ZSTD_compress`
//! 在 level 1 用 windowLog 19（512KiB）。本工具只改窗口、不改其他任何编码决策，
//! 用来量「参考范围」这一项对压缩率的贡献。
//!
//! 注意：`slice_size` 同时是块大小（格式上限 128KiB），所以扩大窗口只能靠增加片数。
//!
//! 用法：`window_sweep <input>...`
use std::env;
use std::fs;

use ruzstd::encoding::{CompressionLevel, FrameCompressor, MatchGeneratorDriver};

const SLICE: usize = 128 * 1024;
const CONFIGS: &[usize] = &[1, 2, 4, 8, 16, 32];

fn main() {
    let files: Vec<String> = env::args().skip(1).collect();
    assert!(!files.is_empty(), "window_sweep <input>...");
    print!("{:<12} {:>9}", "corpus", "raw KB");
    for s in CONFIGS {
        print!("{:>11}", format!("{}K", SLICE * s / 1024));
    }
    println!();
    for path in files {
        let data = fs::read(&path).expect("read input");
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        print!("{:<12} {:>9.0}", name, data.len() as f64 / 1024.0);
        for slices in CONFIGS {
            let mut comp = FrameCompressor::new_with_matcher(
                MatchGeneratorDriver::new(SLICE, *slices),
                CompressionLevel::Fastest,
            );
            comp.set_source(data.as_slice());
            let mut out = Vec::new();
            comp.set_drain(&mut out);
            comp.compress();
            print!("{:>11}", out.len());
        }
        println!();
    }
}
