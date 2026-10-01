//! 每序列工作项计数（确定性计数，不是计时；性能判定仍只在 CI）。
use std::env;
use std::fs;

fn main() {
    let files: Vec<String> = env::args().skip(1).collect();
    println!(
        "{:<20} {:>9} {:>10} {:>11} {:>12} {:>9}",
        "corpus", "produced", "drained", "drain_calls", "out/2 = 覆盖次数", "比值"
    );
    for path in files {
        let data = fs::read(&path).expect("read");
        let comp = zstd::bulk::compress(&data, 1).expect("encode");
        let mut out = vec![0u8; data.len()];
        let _ = ruzstd::seqstats::take();
        let mut dec = ruzstd::decoding::FrameDecoder::new();
        dec.decode_all(&comp[..], &mut out).expect("decode");
        assert!(out == data, "{}: 解码结果与原文不一致", path);
        let c = ruzstd::seqstats::take();
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        let ratio = if c[8] > 0 {
            format!("{:.2}", c[9] as f64 / c[8] as f64)
        } else {
            "-".into()
        };
        println!("{:<20} {:>9} {:>10} {:>11} {:>12} {:>9}",
                 name, c[8], c[9], c[10], format!("{} B", c[8] / 2), ratio);
    }
}
