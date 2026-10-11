//! 次选候选入围规则扫描（`encstats` 特性门控，诊断用）。
//!
//! 只跑与本片代码相关的上下游：编码输出尺寸（ratio）与自解码正确性。
//! 规则见 `encstats::cand_rule_accept`；rule 0 = 现行行为（须与未插桩构建逐字节一致）。
//!
//! 用法：`cand_rule_sweep <input>...`
#![cfg(feature = "encstats")]
use std::env;
use std::fs;
use std::io::Read;

use ruzstd::encstats as es;

const RULES: &[u8] = &[0, 1, 2, 3, 4, 5];

fn main() {
    let files: Vec<String> = env::args().skip(1).collect();
    assert!(!files.is_empty(), "cand_rule_sweep <input>...");
    print!("{:<12} {:>9}", "corpus", "raw KB");
    for r in RULES {
        print!("{:>11}", format!("rule{}", r));
    }
    println!();
    for path in files {
        let data = fs::read(&path).expect("read input");
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        print!("{:<12} {:>9.0}", name, data.len() as f64 / 1024.0);
        let mut base_bytes = 0usize;
        for rule in RULES {
            es::set_cand_rule(*rule);
            let comp = ruzstd::encoding::compress_to_vec(
                &data[..],
                ruzstd::encoding::CompressionLevel::Fastest,
            );
            let mut decoded = Vec::new();
            ruzstd::decoding::StreamingDecoder::new(&comp[..])
                .expect("decoder init")
                .read_to_end(&mut decoded)
                .expect("decode");
            assert_eq!(decoded, data, "{name} rule {rule} 自解码不一致");
            if *rule == 0 {
                base_bytes = comp.len();
            }
            let pct = if base_bytes == 0 {
                0.0
            } else {
                100.0 * (comp.len() as f64 - base_bytes as f64) / base_bytes as f64
            };
            print!("{:>8}({:+.1}%)", comp.len(), pct);
        }
        es::set_cand_rule(0);
        println!();
    }
}
