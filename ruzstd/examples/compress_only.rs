//! Compress-only timing probe (scratch).
use std::fs;
use std::time::Instant;
use ruzstd::encoding::{compress_to_vec, CompressionLevel};
fn main() {
    let input = std::env::args().nth(1).expect("usage: compress_only <in>");
    let data = fs::read(&input).unwrap();
    let t = Instant::now();
    let c = compress_to_vec(data.as_slice(), CompressionLevel::Fastest);
    let enc_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("{} enc={:.1}ms out={}", input, enc_ms, c.len());
}
