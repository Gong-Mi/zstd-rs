//! 编码侧工作项计数（确定性计数，不是计时；性能判定仍只在 CI）。
#![cfg(feature = "encstats")]
use std::env;
use std::fs;

fn main() {
    let files: Vec<String> = env::args().skip(1).collect();
    println!(
        "{:<20} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "corpus", "raw MB", "ratio", "inserts", "probes", "hits", "hashes", "hash/(p+i)"
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
        // 单进程确定性门：同一位置的探针与插入共用一次哈希 ⇒ hashes < probes + inserts。
        // 注意：encstats 是进程级全局计数器，该不变量只在"本进程内没有并发压缩"时成立，
        // 所以它放在 example 里（单进程），不进会在同一进程并行跑整套用例的 lib 测试。
        let (inserts, probes, hashes) = (c[0], c[1], c[3]);
        assert!(
            hashes < probes + inserts,
            "同位置哈希未共用：hashes={} probes={} inserts={}（重构前取等号）",
            hashes,
            probes,
            inserts
        );
        println!(
            "{:<20} {:>9.2} {:>9.3} {:>9} {:>9} {:>9} {:>9} {:>9.3}",
            name,
            data.len() as f64 / 1048576.0,
            ratio,
            c[0],
            c[1],
            c[2],
            c[3],
            c[3] as f64 / (c[1] + c[0]) as f64
        );
    }
}
