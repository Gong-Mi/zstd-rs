//! repcode 潜在收益诊断（`encstats` 特性门控）。
//!
//! 现有编码器把每条序列的 offset 都当作真实偏移写（`of = offset + 3`，见
//! `blocks/compressed.rs` 的 `// TODO make use of the offset history`）。
//! 本工具不改输出，只按 zstd 的 rep 语义（起始 1/4/8，命中时前进历史）逐序列观察：
//! 有多少条序列的 offset 命中 rep1/2/3，以及它们当前多付的 offset 附加位总量。
//!
//! 该"多付位数"是**上界**：只算 offset 附加位，不含改用 repcode 后 FSE 符号本身的代价差异。
//!
//! 用法：`repcode_potential <input>...`
#![cfg(feature = "encstats")]
use std::env;
use std::fs;
use std::io::Read;

fn main() {
    let files: Vec<String> = env::args().skip(1).collect();
    assert!(!files.is_empty(), "repcode_potential <input>...");
    println!(
        "{:<12} {:>9} {:>9} {:>9} {:>10} {:>9} {:>9} {:>9} {:>9}",
        "corpus",
        "raw KB",
        "comp B",
        "ratio",
        "sequences",
        "rep hits",
        "hit %",
        "extra B",
        "extra %"
    );
    for path in files {
        let data = fs::read(&path).expect("read input");
        let _ = ruzstd::encstats::take();
        let comp = ruzstd::encoding::compress_to_vec(
            &data[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let c = ruzstd::encstats::take();

        // 输出必须仍能自解码（诊断不改行为的旁证）
        let mut decoded = Vec::new();
        ruzstd::decoding::StreamingDecoder::new(&comp[..])
            .expect("decoder init")
            .read_to_end(&mut decoded)
            .expect("decode");
        assert_eq!(decoded, data, "{path} 自解码与原文不一致");

        let seqs = c[ruzstd::encstats::SEQ_TOTAL];
        let hits = c[ruzstd::encstats::SEQ_REP_HITS];
        let extra_bits = c[ruzstd::encstats::SEQ_REP_EXTRA_BITS];
        let extra_bytes = extra_bits as f64 / 8.0;
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        println!(
            "{:<12} {:>9.0} {:>9} {:>9.3} {:>10} {:>9} {:>8.1}% {:>9.1} {:>8.2}%",
            name,
            data.len() as f64 / 1024.0,
            comp.len(),
            data.len() as f64 / comp.len() as f64,
            seqs,
            hits,
            if seqs == 0 {
                0.0
            } else {
                100.0 * hits as f64 / seqs as f64
            },
            extra_bytes,
            100.0 * extra_bytes / comp.len() as f64
        );
    }
}
