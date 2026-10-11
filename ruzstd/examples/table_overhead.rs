//! 块内构成诊断（`encstats` 特性门控）：表开销 / literals / sequences 各占多少。
//!
//! 回答「每块 FSE 表开销（B5 余项：Repeat 模式未发）值多少」。现有编码器每个压缩块
//! 都按需重发 ll/ml/of 三张 FSE 表（`Encoded`），C 则可以在块间复用（repeat 模式）。
//! 本工具不改输出，只统计各分区占用与表模式选择次数。
//!
//! 用法：`table_overhead <input>...`
#![cfg(feature = "encstats")]
use std::env;
use std::fs;

use ruzstd::encstats as es;

fn main() {
    let files: Vec<String> = env::args().skip(1).collect();
    assert!(!files.is_empty(), "table_overhead <input>...");
    println!(
        "{:<12} {:>9} {:>7} {:>7} {:>8} {:>9} {:>9} {:>9} {:>7} {:>7} {:>7}",
        "corpus",
        "comp B",
        "blocks",
        "raw blk",
        "tbl/blk",
        "table B",
        "literals B",
        "seq B",
        "tbl %",
        "lit %",
        "seq %"
    );
    for path in files {
        let data = fs::read(&path).expect("read input");
        let _ = es::take();
        let comp = ruzstd::encoding::compress_to_vec(
            &data[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let c = es::take();
        let blocks = c[es::BLK_NUM];
        let table_b = c[es::BLK_TABLE_BITS] as f64 / 8.0;
        let lit_b = c[es::BLK_LITERALS_BITS] as f64 / 8.0;
        let seq_b = c[es::BLK_SEQ_BITS] as f64 / 8.0;
        let total = comp.len() as f64;
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        println!(
            "{:<12} {:>9} {:>7} {:>7} {:>8.0} {:>9.0} {:>9.0} {:>9.0} {:>6.2}% {:>6.1}% {:>6.1}%",
            name,
            comp.len(),
            blocks,
            c[es::BLK_RAW],
            if blocks == 0 {
                0.0
            } else {
                table_b / blocks as f64
            },
            table_b,
            lit_b,
            seq_b,
            100.0 * table_b / total,
            100.0 * lit_b / total,
            100.0 * seq_b / total
        );
        println!(
            "            表模式：Encoded {} / Predefined {}（共 {} 次，即 3×{} 块）",
            c[es::MODE_ENCODED],
            c[es::MODE_PREDEFINED],
            c[es::MODE_ENCODED] + c[es::MODE_PREDEFINED],
            blocks
        );
    }
}
