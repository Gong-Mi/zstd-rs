//! Cross-validation test-suite: ruzstd vs the reference C implementation.
//!
//! Every test is deterministic (fixed seeds, fixed generators) and runs the
//! same triple check used during the optimization campaign:
//!
//! 1. C zstd encode  -> ruzstd decode  == original
//! 2. ruzstd encode  -> ruzstd decode  == original
//! 3. ruzstd encode  -> C decode       == original
//!
//! This is the stabilized form of the ad-hoc `verify` binary used to gate
//! PRs #2/#3/#4; it lives in-tree so CI (and any future optimization)
//! re-runs it on every commit without external tooling.
//!
//! Requires the `std` feature and the dev-dependency `zstd` (C binding).

#![cfg(all(feature = "std", feature = "hash", feature = "dict_builder"))]

use std::io::Read;

// ── deterministic data generators (mirrors profiler/src/main.rs) ──

fn gen_text(size: usize) -> Vec<u8> {
    use rand::{Rng, SeedableRng};
    let words: [&[u8]; 16] = [
        b"the ", b"of ", b"and ", b"to ", b"in ", b"that ", b"he ", b"was ", b"it ", b"his ",
        b"with ", b"is ", b"for ", b"as ", b"had ", b"be ",
    ];
    let mut rng = rand::rngs::SmallRng::seed_from_u64(42);
    let mut buf = Vec::with_capacity(size);
    while buf.len() < size {
        buf.extend_from_slice(words[rng.gen_range(0..words.len())]);
    }
    buf.truncate(size);
    buf
}

fn gen_random(size: usize) -> Vec<u8> {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::SmallRng::seed_from_u64(7);
    let mut buf = vec![0u8; size];
    rng.fill(&mut buf[..]);
    buf
}

fn gen_binary_medium(size: usize) -> Vec<u8> {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::SmallRng::seed_from_u64(99);
    let mut buf = Vec::with_capacity(size);
    while buf.len() < size {
        let tag: u32 = rng.gen_range(0..64);
        let len: u32 = rng.gen_range(8..256);
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        for _ in 0..len {
            if rng.gen_bool(0.6) {
                buf.push(rng.gen_range(0..16));
            } else {
                buf.push(rng.gen());
            }
        }
    }
    buf.truncate(size);
    buf
}

/// Low-entropy long-period run data: exercises warmup-boundary offsets in
/// the windowed match copy (offset 1..=17 all appear here).
fn gen_runs(size: usize) -> Vec<u8> {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::SmallRng::seed_from_u64(1234);
    let mut xor: u64 = 0xDEAD_BEEF;
    let mut buf = Vec::with_capacity(size);
    while buf.len() < size {
        let run = rng.gen_range(1..=4096);
        let byte = (next(&mut xor) % 256) as u8;
        buf.extend(std::iter::repeat(byte).take(run));
        // short non-repeat gap keeps runs from merging into one RLE block
        let gap = rng.gen_range(1..32);
        for _ in 0..gap {
            buf.push((next(&mut xor) % 256) as u8);
        }
    }
    buf.truncate(size);
    buf
}

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

// ── helpers ──

fn rs_decode(data: &[u8]) -> Vec<u8> {
    let mut dec = ruzstd::decoding::StreamingDecoder::new(data).unwrap();
    let mut out = Vec::new();
    dec.read_to_end(&mut out).unwrap();
    out
}

fn rs_encode(data: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
}

fn c_encode(data: &[u8]) -> Vec<u8> {
    zstd::encode_all(data, 1).unwrap()
}

fn c_decode(data: &[u8]) -> Vec<u8> {
    zstd::decode_all(data).unwrap()
}

/// The full triple check; returns a description of the failing leg.
fn triple_check(name: &str, original: &[u8]) -> Result<(), String> {
    let c_comp = c_encode(original);
    if rs_decode(&c_comp) != original {
        return Err(format!("{name}: C-encode -> rs-decode mismatch"));
    }
    let rs_comp = rs_encode(original);
    if rs_decode(&rs_comp) != original {
        return Err(format!("{name}: rs-encode -> rs-decode mismatch"));
    }
    if c_decode(&rs_comp) != original {
        return Err(format!("{name}: rs-encode -> C-decode mismatch"));
    }
    Ok(())
}

// ── the suite ──

macro_rules! corpus_case {
    ($fn_name:ident, $gen:expr, $size:expr) => {
        #[test]
        fn $fn_name() {
            let data = $gen($size);
            triple_check(stringify!($fn_name), &data).unwrap();
        }
    };
}

corpus_case!(cross_text_64k, gen_text, 64 * 1024);
corpus_case!(cross_text_1m, gen_text, 1024 * 1024);
corpus_case!(cross_random_64k, gen_random, 64 * 1024);
corpus_case!(cross_random_1m, gen_random, 1024 * 1024);
corpus_case!(cross_binary_64k, gen_binary_medium, 64 * 1024);
corpus_case!(cross_binary_1m, gen_binary_medium, 1024 * 1024);
corpus_case!(cross_runs_64k, gen_runs, 64 * 1024);
corpus_case!(cross_runs_1m, gen_runs, 1024 * 1024);

/// Edge shapes: empty, single byte, RLE block, multi-frame streams, frame
/// boundary not on a 4-byte multiple.
#[test]
fn cross_edge_shapes() {
    let empty: Vec<u8> = Vec::new();
    let one = vec![42u8];
    let rle = vec![7u8; 3000];
    // 128KB exactly (block boundary) with markers so it is not RLE
    let mut exact_block = vec![0u8; 128 * 1024];
    exact_block[0] = 1;
    exact_block[1] = 2;
    // multi-frame: two independent frames concatenated
    let frame_a = rs_encode(b"first frame payload");
    let frame_b = rs_encode(b"second frame payload");
    let mut multi = frame_a.clone();
    multi.extend_from_slice(&frame_b);
    // skippable frame sandwiched between two data frames
    let mut skip = vec![0x50, 0x2A, 0x4D, 0x18, 4, 0, 0, 0, 0xDE, 0xAD, 0xBE, 0xEF];
    skip.extend_from_slice(&frame_a);
    skip.extend_from_slice(&frame_b);

    for (name, data) in [
        ("empty", &empty[..]),
        ("one-byte", &one[..]),
        ("rle", &rle[..]),
        ("exact-block", &exact_block[..]),
        ("multi-frame", &multi[..]),
        ("skippable+multi", &skip[..]),
    ] {
        triple_check(name, data).unwrap();
    }
}

/// 真正消费拼接多帧流（上面的 multi-frame 用例只是把多帧字节当原文再压一遍，
/// 走的是单帧路径）。分块/多进程压缩的产物就是这种形状。
#[test]
fn cross_concatenated_frames_consumption() {
    use ruzstd::decoding::MultiFrameDecoder;

    let parts: Vec<Vec<u8>> = (0..3).map(|i| gen_text(70_000 + i * 1_237)).collect();
    let mut expected = Vec::new();
    for p in &parts {
        expected.extend_from_slice(p);
    }

    // C 编的多个帧拼接 → ruzstd 一次读完
    let mut c_stream = Vec::new();
    for p in &parts {
        c_stream.extend_from_slice(&c_encode(p));
    }
    let mut dec = MultiFrameDecoder::new(&c_stream[..]);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).unwrap();
    assert_eq!(out, expected, "C multi-frame -> rs multi-frame mismatch");

    // ruzstd 编的多个帧拼接 → C 解（C 原生支持多帧）
    let mut rs_stream = Vec::new();
    for p in &parts {
        rs_stream.extend_from_slice(&rs_encode(p));
    }
    assert_eq!(
        c_decode(&rs_stream),
        expected,
        "rs multi-frame -> C mismatch"
    );

    // 每一帧仍可单独解（随机访问）
    for p in &parts {
        assert_eq!(rs_decode(&rs_encode(p)), *p);
    }
}

/// Regenerated-size values that straddle the 4-stream literals boundaries:
/// data lengths chosen so literal sections are not multiples of 4 and hit
/// the serial-tail path of the batch decoder.
#[test]
fn cross_odd_sizes() {
    for size in [
        1023usize, 1025, 4095, 4097, 65535, 65537, 65539, 131071, 131075,
    ] {
        let data = gen_text(size);
        triple_check(&format!("text-{size}"), &data).unwrap();
    }
}

/// Dictionary round-trip through the C encoder (ruzstd decode side).
///
/// IGNORED for now: ruzstd's `Dictionary::decode_dict` expects a fully
/// trained zstd dictionary (entropy tables + content), which requires the C
/// CLI's `--train` step to produce a fixture. Synthesizing magic+id+content
/// is NOT a valid dictionary — the parser reads the entropy-table section
/// right after the 8-byte header. To enable:
///   1. generate `tests/fixtures/dict.zstd` via `zstd --train`
///   2. encode a payload with `zstd::bulk::Compressor::with_dictionary`
///   3. decode via FrameDecoder::add_dict and compare.
/// Tracked as the dictionary-support gap of the decode side.
#[test]
#[ignore = "needs a trained dictionary fixture (see doc-comment)"]
fn cross_c_dict() {
    use ruzstd::decoding::Dictionary;
    use ruzstd::decoding::FrameDecoder;

    let dict_bytes = std::fs::read("tests/fixtures/dict.zstd").unwrap();
    let payload = gen_text(64 * 1024);
    let mut encoder = zstd::bulk::Compressor::with_dictionary(1, &dict_bytes).unwrap();
    let compressed = encoder.compress(&payload).unwrap();

    let dictionary = Dictionary::decode_dict(&dict_bytes).unwrap();
    let mut dec = FrameDecoder::new();
    dec.add_dict(dictionary).unwrap();
    let mut out = Vec::new();
    dec.decode_all_to_vec(&compressed, &mut out).unwrap();
    assert_eq!(out, payload, "C-dict-encode -> rs-decode mismatch");
}

/// Streaming decode in small chunks (worst-case drain cadence: 1KB reads
/// through the decode buffer) must be byte-identical to one-shot decoding.
#[test]
fn cross_streaming_small_reads() {
    let data = gen_text(1024 * 1024);
    let compressed = c_encode(&data);

    let mut dec = ruzstd::decoding::StreamingDecoder::new(&compressed[..]).unwrap();
    let mut out = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = dec.read(&mut chunk).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(out, data, "streaming small-read decode mismatch");
}
