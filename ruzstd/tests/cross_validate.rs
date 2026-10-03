//! Cross-validation test-suite: ruzstd vs the reference C implementation.
//!
//! Every test is deterministic (fixed seeds, fixed generators) and runs the
//! same triple check used during the optimization campaign:
//!
//! 1. C zstd encode  -> ruzstd known-capacity AND streaming decode == original
//! 2. ruzstd encode  -> ruzstd known-capacity AND streaming decode == original
//! 3. ruzstd encode  -> C decode                                  == original
//!
//! The known-capacity leg calls FrameDecoder::decode_all (the direct-output
//! path); the streaming leg independently consumes one frame through Read.
//! Concatenated/skippable streams are decoded as streams, never recompressed
//! as plaintext. StreamingDecoder is recreated for each data frame, as its
//! documented contract requires.
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

fn check_known(name: &str, compressed: &[u8], original: &[u8]) -> Result<(), String> {
    let mut dec = ruzstd::decoding::FrameDecoder::new();
    let mut out = vec![0u8; original.len()];
    let written = dec
        .decode_all(compressed, &mut out)
        .map_err(|e| format!("{name}: known decode_all failed: {e:?}"))?;
    if written != original.len() || out != original {
        return Err(format!(
            "{name}: known decode_all mismatch ({written} bytes)"
        ));
    }
    Ok(())
}

/// A separate oracle for the ring-buffer/Read consumer, not decode_all.
fn check_single_stream(name: &str, compressed: &[u8], original: &[u8]) -> Result<(), String> {
    let mut dec = ruzstd::decoding::StreamingDecoder::new(compressed)
        .map_err(|e| format!("{name}: streaming init failed: {e:?}"))?;
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| format!("{name}: streaming read failed: {e:?}"))?;
    if out != original || !dec.into_inner().is_empty() {
        return Err(format!(
            "{name}: single-frame streaming mismatch or trailing input"
        ));
    }
    Ok(())
}

/// Follow StreamingDecoder's single-frame contract: recreate the consumer
/// after EOF and skip only the declared payload after a SkipFrame header.
fn read_frame_stream(mut input: &[u8]) -> Result<(Vec<u8>, usize, usize), String> {
    use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
    use ruzstd::decoding::StreamingDecoder;

    let mut out = Vec::new();
    let mut frames = 0;
    let mut skips = 0;
    while !input.is_empty() {
        let before = input.len();
        match StreamingDecoder::new(&mut input) {
            Ok(mut dec) => {
                dec.read_to_end(&mut out)
                    .map_err(|e| format!("multi-frame streaming read failed: {e:?}"))?;
                frames += 1;
            }
            Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                length,
                ..
            })) => {
                input = input
                    .get(length as usize..)
                    .ok_or_else(|| "truncated skippable payload".to_owned())?;
                skips += 1;
            }
            Err(e) => return Err(format!("multi-frame streaming init failed: {e:?}")),
        }
        if input.len() >= before {
            return Err("multi-frame streaming made no progress".to_owned());
        }
    }
    Ok((out, frames, skips))
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
    let c_leg = format!("{name}: C-encode -> Rust");
    check_known(&c_leg, &c_comp, original)?;
    check_single_stream(&c_leg, &c_comp, original)?;
    let rs_comp = rs_encode(original);
    let rs_leg = format!("{name}: Rust-encode -> Rust");
    check_known(&rs_leg, &rs_comp, original)?;
    check_single_stream(&rs_leg, &rs_comp, original)?;
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
    for (name, data) in [
        ("empty", &empty[..]),
        ("one-byte", &one[..]),
        ("rle", &rle[..]),
        ("exact-block", &exact_block[..]),
    ] {
        triple_check(name, data).unwrap();
    }

    let original_a = b"first frame payload";
    let original_b = b"second frame payload";
    let expected = [original_a.as_slice(), original_b.as_slice()].concat();
    // A valid skip header with four opaque payload bytes. It is part of the
    // compressed input, not plaintext passed to an encoder.
    let skip = [0x50, 0x2A, 0x4D, 0x18, 4, 0, 0, 0, 0xDE, 0xAD, 0xBE, 0xEF];
    for (encoder, encode) in [
        ("C", c_encode as fn(&[u8]) -> Vec<u8>),
        ("Rust", rs_encode as fn(&[u8]) -> Vec<u8>),
    ] {
        let frame_a = encode(original_a);
        let frame_b = encode(original_b);
        check_single_stream(encoder, &frame_a, original_a).unwrap();
        check_single_stream(encoder, &frame_b, original_b).unwrap();
        for (shape, input, skip_count) in [
            ("multi", [&frame_a[..], &frame_b[..]].concat(), 0),
            ("skip-before", [&skip[..], &frame_a, &frame_b].concat(), 1),
            ("skip-between", [&frame_a[..], &skip, &frame_b].concat(), 1),
            ("skip-after", [&frame_a[..], &frame_b, &skip].concat(), 1),
            (
                "skip-before-between-after",
                [&skip[..], &frame_a, &skip, &frame_b, &skip].concat(),
                3,
            ),
        ] {
            let name = format!("{encoder}-{shape}");
            check_known(&name, &input, &expected).unwrap();
            let mut out = Vec::with_capacity(expected.len());
            ruzstd::decoding::FrameDecoder::new()
                .decode_all_to_vec(&input, &mut out)
                .unwrap();
            assert_eq!(out, expected, "{name}: known decode_all_to_vec mismatch");
            let (streamed, frames, skips) = read_frame_stream(&input).unwrap();
            assert_eq!(streamed, expected, "{name}: rebuilt stream mismatch");
            assert_eq!(frames, 2, "{name}: must consume both data frames");
            assert_eq!(skips, skip_count, "{name}: must consume each skip frame");
            assert_eq!(c_decode(&input), expected, "{name}: C decode mismatch");
        }
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

/// Train a real dictionary in memory from fixed samples; no downloaded or
/// generated-on-disk fixture and no external zstd CLI are needed.
#[test]
fn cross_c_dict() {
    use ruzstd::decoding::errors::FrameDecoderError;
    use ruzstd::decoding::{Dictionary, FrameDecoder, StreamingDecoder};

    let samples: Vec<Vec<u8>> = (0..256)
        .map(|i| {
            format!(
                "request route=/api/items/{} method=GET status=200 content-type=application/json \
                 account={} region={} response={{\"enabled\":true,\"items\":[1,2,3],\"message\":\"dictionary oracle\"}}\n",
                i % 16,
                i,
                i % 8,
            )
            .into_bytes()
        })
        .collect();
    let dict_bytes = zstd::dict::from_samples(&samples, 2048).unwrap();
    assert_eq!(&dict_bytes[..4], &[0x37, 0xA4, 0x30, 0xEC]);
    let dict_id = Dictionary::decode_dict(&dict_bytes).unwrap().id;
    assert_ne!(dict_id, 0);
    // A short training-shaped record benefits from dictionary references,
    // rather than hiding dictionary use under huge intra-frame repetitions.
    let payload = &samples[37];
    let mut encoder = zstd::bulk::Compressor::with_dictionary(1, &dict_bytes).unwrap();
    let compressed = encoder.compress(payload).unwrap();
    assert_eq!(
        zstd::zstd_safe::get_dict_id_from_frame(&compressed).map(|id| id.get()),
        Some(dict_id),
        "C encoder must emit the trained dictionary ID",
    );
    let without_dict = zstd::bulk::compress(payload, 1).unwrap();
    assert!(
        compressed.len() < without_dict.len(),
        "dictionary must actually help"
    );
    let mut c_decoder = zstd::bulk::Decompressor::with_dictionary(&dict_bytes).unwrap();
    assert_eq!(
        c_decoder.decompress(&compressed, payload.len()).unwrap(),
        *payload
    );
    assert!(zstd::bulk::decompress(&compressed, payload.len()).is_err());

    let mut out = vec![0u8; payload.len()];
    assert!(matches!(
        FrameDecoder::new().decode_all(&compressed, &mut out),
        Err(FrameDecoderError::DictNotProvided { dict_id: id }) if id == dict_id
    ));

    let make_decoder = || {
        let mut dec = FrameDecoder::new();
        dec.add_dict(Dictionary::decode_dict(&dict_bytes).unwrap())
            .unwrap();
        dec
    };
    let mut known = make_decoder();
    let written = known.decode_all(&compressed, &mut out).unwrap();
    assert_eq!(written, payload.len());
    assert_eq!(out, *payload, "C-dict -> Rust known mismatch");
    let mut vec_out = Vec::with_capacity(payload.len());
    known.decode_all_to_vec(&compressed, &mut vec_out).unwrap();
    assert_eq!(vec_out, *payload, "C-dict -> Rust known vec mismatch");

    let mut stream = StreamingDecoder::new_with_decoder(&compressed[..], make_decoder()).unwrap();
    let mut streamed = Vec::new();
    stream.read_to_end(&mut streamed).unwrap();
    assert_eq!(streamed, *payload, "C-dict -> Rust streaming mismatch");
    assert!(stream.into_inner().is_empty());
}

/// Streaming decode in small chunks (worst-case drain cadence: 1KB reads
/// through the decode buffer) must be byte-identical to one-shot decoding.
#[test]
fn cross_streaming_small_reads() {
    let data = gen_text(1024 * 1024);
    for (encoder, compressed) in [("C", c_encode(&data)), ("Rust", rs_encode(&data))] {
        check_known(encoder, &compressed, &data).unwrap();
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
        assert_eq!(out, data, "{encoder}: streaming small-read decode mismatch");
        assert!(dec.into_inner().is_empty());
    }
}
