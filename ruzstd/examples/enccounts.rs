//! 编码侧工作项计数（确定性计数，不是计时；性能判定仍只在 CI）。
#![cfg(feature = "encstats")]
use std::env;
use std::fs;

fn main() {
    let files: Vec<String> = env::args().skip(1).collect();
    println!(
        "{:<20} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "corpus", "raw MB", "ratio", "inserts", "probes", "hits", "ins+B/byte"
    );
    for path in files {
        let data = fs::read(&path).expect("read");
        let _ = ruzstd::encstats::take();
        let comp = ruzstd::encoding::compress_to_vec(
            &data[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let c = ruzstd::encstats::take();
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        let ratio = data.len() as f64 / comp.len() as f64;
        let per_byte = (c[0] + c[1]) as f64 / data.len() as f64;
        println!(
            "{:<20} {:>9.2} {:>9.3} {:>9} {:>9} {:>9} {:>9.3}",
            name,
            data.len() as f64 / 1048576.0,
            ratio,
            c[0],
            c[1],
            c[2],
            per_byte
        );
    }
}
