//! 每序列工作项计数（确定性计数，不是计时；性能判定仍只在 CI）。
use std::env;
use std::fs;

fn main() {
    let files: Vec<String> = env::args().skip(1).collect();
    println!("{:<20} {:>9} {:>9} {:>10} {:>9} {:>10} {:>10} {:>9} {:>10}",
             "corpus", "seqs", "triple", "upd_state", "bits_rem", "lit_calls", "lit_bytes", "match_calls", "match_bytes");
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
        println!("{:<20} {:>9} {:>9} {:>10} {:>9} {:>10} {:>10} {:>9} {:>10}",
                 name, c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]);
    }
}
