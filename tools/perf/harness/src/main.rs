//! One locked measurement program, compiled against each subject ref.
//! Local runs prove correctness/schema only; performance acceptance belongs to CI.
mod corpus;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::hint::black_box;
use std::io::Read;
use std::time::Instant;

struct Args {
    size_mb: usize,
    encode_mb: usize,
    iters: usize,
    round: usize,
    side: String,
    source_sha: String,
    binary_sha256: String,
    repo_dir: String,
    corpus_files: Vec<(String, String)>,
    plan: bool,
}

fn positive(value: Option<String>, name: &str) -> usize {
    let value = value.unwrap_or_else(|| panic!("missing {name}"));
    let n: usize = value.parse().unwrap_or_else(|_| panic!("invalid {name}"));
    assert!(n > 0, "{name} must be positive");
    n
}

fn args() -> Args {
    let mut out = Args {
        size_mb: 1,
        encode_mb: 1,
        iters: 3,
        round: 1,
        side: "head".into(),
        source_sha: std::env::var("PERF_SOURCE_SHA").unwrap_or_default(),
        binary_sha256: std::env::var("PERF_BINARY_SHA256").unwrap_or_default(),
        repo_dir: ".".into(),
        corpus_files: Vec::new(),
        plan: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(name) = args.next() {
        match name.as_str() {
            "--size-mb" => out.size_mb = positive(args.next(), &name),
            "--encode-mb" => out.encode_mb = positive(args.next(), &name),
            "--iters" => out.iters = positive(args.next(), &name),
            "--round" => out.round = positive(args.next(), &name),
            "--side" => out.side = args.next().expect("missing --side"),
            "--repo-dir" => out.repo_dir = args.next().expect("missing --repo-dir"),
            "--plan" => out.plan = true,
            "--corpus-file" => {
                let path = args.next().expect("missing --corpus-file");
                let (path, label) = match path.rsplit_once(':') {
                    Some((path, label)) if !label.contains('/') => {
                        (path.to_string(), label.to_string())
                    }
                    _ => {
                        let label = std::path::Path::new(&path)
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .expect("corpus path has no name")
                            .to_string();
                        (path, label)
                    }
                };
                out.corpus_files.push((path, label));
            }
            _ => panic!("unknown argument {name}"),
        }
    }
    assert!(out.size_mb <= 64 && out.encode_mb <= 64, "64 MiB per-corpus limit");
    assert!(out.iters <= 100 && out.round <= 100, "bounded experiment required");
    if !out.plan {
        assert!(matches!(out.side.as_str(), "base" | "head" | "base2" | "head_tuned"));
        assert!(hex_identity(&out.source_sha, 40), "missing/invalid source identity");
        assert_eq!(out.source_sha, env!("PERF_BUILD_SOURCE_SHA"), "runtime source identity differs from actual build");
        assert!(hex_identity(&out.binary_sha256, 64), "missing/invalid binary identity");
    }
    out
}

fn hex_identity(value: &str, size: usize) -> bool {
    value.len() == size && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn sha256(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn load_corpora(args: &Args) -> Vec<(String, Vec<u8>)> {
    let size = args.size_mb * 1024 * 1024;
    let mut corpora = vec![
        ("text".into(), corpus::gen_text(size)),
        ("random".into(), corpus::gen_random(size)),
        ("binary-medium".into(), corpus::gen_medium(size)),
        ("src-like".into(), corpus::gen_src_like(size)),
        ("bin-like".into(), corpus::gen_bin_like(size)),
        ("repo-sources".into(), corpus::gen_repo_sources(&args.repo_dir, size)),
    ];
    for (path, name) in &args.corpus_files {
        let mut data = std::fs::read(path).expect("required corpus file unreadable");
        data.truncate(size);
        corpora.push((name.clone(), data));
    }
    let mut names = BTreeSet::new();
    for (name, data) in &corpora {
        assert!(!name.is_empty() && names.insert(name.clone()), "duplicate/empty corpus name");
        assert!(!data.is_empty(), "required corpus {name} is empty");
    }
    corpora
}

fn params(kind: &str, leg: &str, args: &Args) -> Value {
    let (impl_api, ref_api) = match leg {
        "known" => ("FrameDecoder::decode_all", "bulk::decompress_to_buffer"),
        "stream" => ("StreamingDecoder/read_64k", "decode_all"),
        "fastest" => ("compress_to_vec", "encode_all"),
        _ => unreachable!(),
    };
    let mut params = json!({
        "impl_api": impl_api, "ref_api": ref_api, "ref_level": 1,
        "ref_version": zstd::zstd_safe::version_string(),
        "features": ["hash", "std"], "iters": args.iters,
        "generator": "fixed-seed-corpus-v1",
    });
    if kind == "encode" {
        params["impl_level"] = json!("Fastest");
    }
    params
}

fn descriptor(kind: &str, leg: &str, name: &str, data: &[u8], args: &Args) -> Value {
    json!({"kind": kind, "leg": leg, "corpus": name, "bytes": data.len(),
           "sha256": sha256(data), "params": params(kind, leg, args)})
}

fn decode_stream(data: &[u8]) -> Vec<u8> {
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(data).unwrap();
    let mut output = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut chunk).unwrap();
        if n == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..n]);
    }
    output
}

fn decode_known(data: &[u8], output: &mut [u8]) -> usize {
    ruzstd::decoding::FrameDecoder::new().decode_all(data, output).unwrap()
}

fn verify(encoded: &[u8], original: &[u8]) {
    assert_eq!(decode_stream(encoded), original, "streaming byte mismatch");
    let mut output = vec![0; original.len()];
    let n = decode_known(encoded, &mut output);
    assert_eq!(n, original.len());
    assert_eq!(output, original, "known byte mismatch");
    assert_eq!(zstd::decode_all(encoded).unwrap(), original, "C byte mismatch");
}

fn cpu_seconds() -> f64 {
    #[repr(C)]
    struct Timespec {
        tv_sec: i64,
        tv_nsec: i64,
    }
    extern "C" {
        fn clock_gettime(clock: i32, result: *mut Timespec) -> i32;
    }
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    // Linux/Android 64-bit CI only. A clock failure is fatal, never a zero sample.
    assert_eq!(unsafe { clock_gettime(2, &mut ts) }, 0, "process CPU clock failed");
    ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
}

fn timed(action: &mut impl FnMut()) -> Value {
    let cpu = cpu_seconds();
    let wall = Instant::now();
    action();
    let wall_ms = wall.elapsed().as_secs_f64() * 1000.0;
    let cpu_ms = (cpu_seconds() - cpu) * 1000.0;
    assert!(wall_ms > 0.0 && cpu_ms > 0.0);
    json!({"wall_ms": wall_ms, "cpu_ms": cpu_ms})
}

fn measure_pair(iters: usize, mut reference: impl FnMut(), mut subject: impl FnMut()) -> (Vec<Value>, Vec<Value>) {
    reference();
    subject();
    let mut ref_samples = Vec::with_capacity(iters);
    let mut impl_samples = Vec::with_capacity(iters);
    for i in 0..iters {
        // Balanced order inside each invocation; retain every real sample.
        if i % 2 == 0 {
            ref_samples.push(timed(&mut reference));
            impl_samples.push(timed(&mut subject));
        } else {
            impl_samples.push(timed(&mut subject));
            ref_samples.push(timed(&mut reference));
        }
    }
    (ref_samples, impl_samples)
}

fn best(samples: &[Value]) -> &Value {
    samples.iter().min_by(|a, b| {
        a["wall_ms"].as_f64().unwrap().total_cmp(&b["wall_ms"].as_f64().unwrap())
    }).unwrap()
}

fn output(mut row: Value, args: &Args, reference: Vec<Value>, subject: Vec<Value>) {
    let ref_best = best(&reference);
    let impl_best = best(&subject);
    row["schema"] = json!(1);
    row["side"] = json!(args.side);
    row["round"] = json!(args.round);
    row["source_sha"] = json!(args.source_sha);
    row["binary_sha256"] = json!(args.binary_sha256);
    row["c_ms"] = ref_best["wall_ms"].clone();
    row["c_cpu_ms"] = ref_best["cpu_ms"].clone();
    row["ruzstd_ms"] = impl_best["wall_ms"].clone();
    row["ruzstd_cpu_ms"] = impl_best["cpu_ms"].clone();
    row["ref_samples"] = json!(reference);
    row["impl_samples"] = json!(subject);
    println!("{row}");
}

fn build_identity() -> Value {
    json!({"source_sha": env!("PERF_BUILD_SOURCE_SHA"),
           "harness_sha256": env!("PERF_BUILD_HARNESS_SHA256"),
           "lock_sha256": env!("PERF_BUILD_LOCK_SHA256")})
}

fn main() {
    if std::env::args().skip(1).eq(["--identity"]) {
        println!("{}", build_identity());
        return;
    }
    let args = args();
    let corpora = load_corpora(&args);
    let mut cases = Vec::new();
    for (name, data) in corpora {
        let compressed = zstd::encode_all(&data[..], 1).unwrap();
        let compressed_sha = sha256(&compressed);
        for leg in ["stream", "known"] {
            let mut row = descriptor("sample", leg, &name, &data, &args);
            row["compressed_sha256"] = json!(compressed_sha);
            if args.plan {
                cases.push(row);
                continue;
            }
            verify(&compressed, &data);
            let (reference, subject) = if leg == "stream" {
                measure_pair(args.iters,
                    || { black_box(zstd::decode_all(&compressed[..]).unwrap()); },
                    || { black_box(decode_stream(&compressed)); })
            } else {
                // Both consumers supply and reuse equal-capacity buffers; neither
                // result Vec allocation is charged to only one implementation.
                let mut ref_output = vec![0u8; data.len()];
                let mut impl_output = vec![0u8; data.len()];
                let result = measure_pair(args.iters,
                    || { black_box(zstd::bulk::decompress_to_buffer(&compressed, &mut ref_output).unwrap()); },
                    || { black_box(decode_known(&compressed, &mut impl_output)); });
                assert_eq!(ref_output, data);
                assert_eq!(impl_output, data);
                result
            };
            if data.len() as f64 / (compressed.len() as f64) < 1.05 {
                eprintln!("WARNING: {name}/{leg} reference ratio near 1; entropy path may not dominate");
            }
            output(row, &args, reference, subject);
        }
        let data = &data[..data.len().min(args.encode_mb * 1024 * 1024)];
        let mut row = descriptor("encode", "fastest", &name, data, &args);
        if args.plan {
            cases.push(row);
            continue;
        }
        let c_compressed = zstd::encode_all(data, 1).unwrap();
        let rs_compressed = ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest);
        verify(&c_compressed, data);
        verify(&rs_compressed, data);
        row["c_bytes"] = json!(c_compressed.len());
        row["ruzstd_bytes"] = json!(rs_compressed.len());
        row["c_ratio"] = json!(data.len() as f64 / c_compressed.len() as f64);
        row["ruzstd_ratio"] = json!(data.len() as f64 / rs_compressed.len() as f64);
        let (reference, subject) = measure_pair(args.iters,
            || { black_box(zstd::encode_all(data, 1).unwrap()); },
            || { black_box(ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)); });
        output(row, &args, reference, subject);
    }
    if args.plan {
        println!("{}", json!({"schema": 1, "cases": cases}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_public_vectors() {
        assert_eq!(sha256(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn encode_measurement_honors_each_iteration_and_one_warmup() {
        let mut reference_calls = 0;
        let mut subject_calls = 0;
        let (reference, subject) = measure_pair(3,
            || { reference_calls += 1; black_box(vec![0u8; 4096]); },
            || { subject_calls += 1; black_box(vec![1u8; 4096]); });
        assert_eq!((reference_calls, subject_calls), (4, 4));
        assert_eq!((reference.len(), subject.len()), (3, 3));
    }

    #[test]
    fn cpu_metric_belongs_to_wall_selected_iteration() {
        let values = vec![json!({"wall_ms": 2.0, "cpu_ms": 1.9}), json!({"wall_ms": 3.0, "cpu_ms": 1.0})];
        assert_eq!(best(&values)["cpu_ms"], json!(1.9));
    }
}
