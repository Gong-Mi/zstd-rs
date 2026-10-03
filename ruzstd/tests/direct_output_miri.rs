//! Small, deterministic tests of the production direct-output decoder.
//!
//! This target calls no C FFI, encoder, random generator, filesystem or
//! external command. The raw/RLE/empty frames are hand-written RFC 8878
//! frames. The two compressed frames are embedded, real libzstd output; see
//! their provenance below. There is deliberately no cfg(miri) gate: ordinary
//! cargo test and Miri both execute the same nonzero set of tests.
//!
//! CI invocation: cargo +nightly miri test -p ruzstd --test direct_output_miri
//! -- --test-threads=1

use ruzstd::decoding::errors::FrameDecoderError;
use ruzstd::decoding::FrameDecoder;

// Magic, single-segment descriptor, one-byte content size, last raw block.
const RAW: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x04, 0x21, 0x00, 0x00, b'r', b'a', b'w', b'!',
];
const RAW_PLAIN: &[u8] = b"raw!";
// Same frame header, last RLE block: regenerated size 17, stored byte 7.
const RLE: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x11, 0x8b, 0x00, 0x00, 7];
const RLE_PLAIN: &[u8] = &[7; 17];
const EMPTY: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x00, 0x01, 0x00, 0x00];
// The payload deliberately resembles a frame header but must remain opaque.
const SKIP: &[u8] = &[0x50, 0x2a, 0x4d, 0x18, 4, 0, 0, 0, 0x28, 0xb5, 0x2f, 0xfd];

// Generated with libzstd 1.5.7 ZSTD_compress(..., level=1) from b"abcd" * 16.
// The full 19-byte frame was checked byte-for-byte against that encoder and
// C-decompressed to the exact 64-byte plaintext before embedding it here.
// ZSTD_generateSequences with the same level reported (offset=4, ll=4, ml=60).
// Thus decode_all must execute direct_repeat's overlap/doubling branch (4<60),
// not a raw/RLE block. Frame SHA-256:
// 7f66e84192e26a8bcf824ce5bbfbf715a9d4e606f849404f377d17963f22efbf
const OVERLAP: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x40, 0x55, 0x00, 0x00, 0x20, 0x61, 0x62, 0x63, 0x64, 0x01, 0x00,
    0x59, 0x72, 0x44,
];
const OVERLAP_PLAIN: &[u8] = b"abcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd";

// Generated and C-decompressed using the same libzstd call and level from the
// exact NONOVERLAP_PLAIN below. ZSTD_generateSequences reported
// (offset=38, ll=38, ml=16): direct_repeat's nonoverlapping fast path (38>=16).
// The complete 55-byte compressed frame, including its non-raw block header,
// was checked byte-for-byte against the encoder. Frame SHA-256:
// 96c50ad05f1df8461225557ac10b466f9d63388a916a2b31c66d79b6dd41f720
const NONOVERLAP: &[u8] = &[
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x36, 0x75, 0x01, 0x00, 0x64, 0x02, 0x30, 0x31, 0x32, 0x33, 0x34,
    0x35, 0x36, 0x37, 0x38, 0x39, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b,
    0x6c, 0x6d, 0x6e, 0x6f, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x57, 0x58, 0x59, 0x5a, 0x2d,
    0x2d, 0x01, 0x00, 0x4e, 0x88, 0x7a, 0x02,
];
const NONOVERLAP_PLAIN: &[u8] = b"0123456789abcdefghijklmnopqrstuvWXYZ--0123456789abcdef";

const GUARD: usize = 8;
const SENTINEL: u8 = 0xa5;
const PREFIX: &[u8] = b"keep:";

fn cases() -> [(&'static [u8], &'static [u8]); 4] {
    [
        (RAW, RAW_PLAIN),
        (RLE, RLE_PLAIN),
        (OVERLAP, OVERLAP_PLAIN),
        (NONOVERLAP, NONOVERLAP_PLAIN),
    ]
}

fn assert_guards(storage: &[u8], capacity: usize) {
    assert!(storage[..GUARD].iter().all(|&b| b == SENTINEL));
    assert!(storage[GUARD + capacity..].iter().all(|&b| b == SENTINEL));
}

// Box<[u8]> -> Vec guarantees an exact capacity, rather than assuming the
// allocator did not round up Vec::with_capacity. The prefix is already live.
fn output_vec(extra_capacity: usize) -> Vec<u8> {
    let mut out = vec![SENTINEL; PREFIX.len() + extra_capacity]
        .into_boxed_slice()
        .into_vec();
    assert_eq!(out.capacity(), PREFIX.len() + extra_capacity);
    out[..PREFIX.len()].copy_from_slice(PREFIX);
    out.truncate(PREFIX.len());
    out
}

fn check_success(decoder: &mut FrameDecoder, frame: &[u8], expected: &[u8]) {
    // Exact and oversized slices both preserve the guards; oversized output
    // must not overwrite its unused tail, and written counts must be exact.
    for capacity in [expected.len(), expected.len() + 3] {
        let mut storage = vec![SENTINEL; GUARD + capacity + GUARD];
        let written = decoder
            .decode_all(frame, &mut storage[GUARD..GUARD + capacity])
            .unwrap();
        assert_eq!(written, expected.len());
        assert_eq!(&storage[GUARD..GUARD + written], expected);
        assert!(storage[GUARD + written..GUARD + capacity]
            .iter()
            .all(|&b| b == SENTINEL));
        assert_guards(&storage, capacity);
    }
    let mut out = output_vec(expected.len());
    let allocation = out.as_ptr();
    let capacity = out.capacity();
    decoder.decode_all_to_vec(frame, &mut out).unwrap();
    assert_eq!(&out[..PREFIX.len()], PREFIX);
    assert_eq!(&out[PREFIX.len()..], expected);
    assert_eq!(out.len(), PREFIX.len() + expected.len());
    assert_eq!(out.capacity(), capacity);
    assert_eq!(
        out.as_ptr(),
        allocation,
        "decode_all_to_vec must not reallocate"
    );
}

#[test]
fn raw_block_exact_and_vec() {
    check_success(&mut FrameDecoder::new(), RAW, RAW_PLAIN);
}

#[test]
fn rle_block_exact_and_vec() {
    check_success(&mut FrameDecoder::new(), RLE, RLE_PLAIN);
}

#[test]
fn compressed_match_overlap_exact_and_vec() {
    check_success(&mut FrameDecoder::new(), OVERLAP, OVERLAP_PLAIN);
}

#[test]
fn compressed_match_nonoverlap_exact_and_vec() {
    check_success(&mut FrameDecoder::new(), NONOVERLAP, NONOVERLAP_PLAIN);
}

#[test]
fn short_slices_preserve_guards_and_decoder_can_be_reused() {
    let mut decoder = FrameDecoder::new();
    for (frame, expected) in [(RAW, RAW_PLAIN), (RLE, RLE_PLAIN)] {
        for capacity in [0, expected.len() - 1] {
            let mut storage = vec![SENTINEL; GUARD + capacity + GUARD];
            let result = decoder.decode_all(frame, &mut storage[GUARD..GUARD + capacity]);
            assert!(
                matches!(result, Err(FrameDecoderError::TargetTooSmall)),
                "{:?}",
                result
            );
            assert_guards(&storage, capacity);
            // The old output is gone before reuse. Miri must not observe any
            // dangling direct-output access into it during a subsequent call.
            drop(storage);
            check_success(&mut decoder, frame, expected);
        }
    }
}

#[test]
fn short_vec_keeps_prefix_length_and_allocation_after_error() {
    let mut decoder = FrameDecoder::new();
    for (frame, expected) in [(RAW, RAW_PLAIN), (RLE, RLE_PLAIN)] {
        for capacity in [0, expected.len() - 1] {
            let mut out = output_vec(capacity);
            let allocation = out.as_ptr();
            let original_capacity = out.capacity();
            let result = decoder.decode_all_to_vec(frame, &mut out);
            assert!(
                matches!(result, Err(FrameDecoderError::TargetTooSmall)),
                "{:?}",
                result
            );
            assert_eq!(out, PREFIX, "length and live prefix must survive error");
            assert_eq!(out.capacity(), original_capacity);
            assert_eq!(out.as_ptr(), allocation);
            drop(out);
            check_success(&mut decoder, frame, expected);
        }
    }
}

#[test]
fn concatenated_frames_skip_before_between_and_after() {
    let input = [SKIP, RAW, SKIP, OVERLAP, SKIP, RLE, NONOVERLAP, SKIP].concat();
    let expected = [RAW_PLAIN, OVERLAP_PLAIN, RLE_PLAIN, NONOVERLAP_PLAIN].concat();
    let mut decoder = FrameDecoder::new();
    check_success(&mut decoder, &input, &expected);

    // Finish on a raw block to exercise capacity exhaustion in a later frame.
    let short_input = [&input[..], RAW].concat();
    let capacity = expected.len() + RAW_PLAIN.len() - 1;
    let mut storage = vec![SENTINEL; GUARD + capacity + GUARD];
    let result = decoder.decode_all(&short_input, &mut storage[GUARD..GUARD + capacity]);
    assert!(
        matches!(result, Err(FrameDecoderError::TargetTooSmall)),
        "{:?}",
        result
    );
    assert_guards(&storage, capacity);
    drop(storage);
    check_success(&mut decoder, &input, &expected);
}

/// Compressed output exhaustion is a Result error, not a Seq_sum panic.
/// Exercise every short capacity, including literals that do not fit even
/// though a later match would fit in the unchanged physical output buffer.
#[test]
fn compressed_short_output_preserves_guards_vec_and_decoder_reuse() {
    let mut decoder = FrameDecoder::new();
    // A non-final compressed block followed by two raw blocks: exhaustion
    // must not continue using the physical prefix as output progress/history.
    let mut multiblock = OVERLAP.to_vec();
    multiblock[5] += (2 * RAW_PLAIN.len()) as u8;
    multiblock[6] &= !1;
    let raw_header = multiblock.len();
    multiblock.extend_from_slice(&RAW[6..]);
    multiblock[raw_header] &= !1;
    multiblock.extend_from_slice(&RAW[6..]);
    let multiblock_plain = [OVERLAP_PLAIN, RAW_PLAIN, RAW_PLAIN].concat();
    for (frame, expected) in [
        (OVERLAP, OVERLAP_PLAIN),
        (NONOVERLAP, NONOVERLAP_PLAIN),
        (&multiblock[..], &multiblock_plain[..]),
    ] {
        for capacity in 0..expected.len() {
            let mut storage = vec![SENTINEL; GUARD + capacity + GUARD];
            let result = decoder.decode_all(frame, &mut storage[GUARD..GUARD + capacity]);
            assert!(
                matches!(result, Err(FrameDecoderError::TargetTooSmall)),
                "capacity {}: {:?}",
                capacity,
                result
            );
            assert_guards(&storage, capacity);
            drop(storage);
            check_success(&mut decoder, frame, expected);

            let mut out = output_vec(capacity);
            let allocation = out.as_ptr();
            let original_capacity = out.capacity();
            let result = decoder.decode_all_to_vec(frame, &mut out);
            assert!(
                matches!(result, Err(FrameDecoderError::TargetTooSmall)),
                "capacity {}: {:?}",
                capacity,
                result
            );
            assert_eq!(out, PREFIX, "length and live prefix must survive error");
            assert_eq!(out.capacity(), original_capacity);
            assert_eq!(out.as_ptr(), allocation);
            drop(out);
            check_success(&mut decoder, frame, expected);
        }
    }
}

#[test]
fn truncated_block_and_skip_errors_do_not_poison_decoder() {
    use ruzstd::decoding::errors::{
        DecodeBlockContentError, DecodeBufferError, DecodeSequenceError, DecompressBlockError,
        ExecuteSequencesError,
    };

    let mut decoder = FrameDecoder::new();
    // Extra sequence bits are detected after literals/matches have attempted
    // output. Capacity exhaustion must not replace that corruption error.
    let mut extra_bits = OVERLAP.to_vec();
    extra_bits[6] += 8; // One additional byte in the compressed block body.
    extra_bits.insert(16, 0);
    // RFC-style raw literals + one RLE-coded sequence: ll=1, ml=3,
    // offset=(1<<3)-3=5, but only one byte of history and no dictionary.
    let invalid_offset = [
        0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x04, 0x45, 0, 0, 0x08, b'a', 1, 0x54, 1, 3, 0, 0x08,
    ];
    for (frame, is_extra_bits) in [(&extra_bits[..], true), (&invalid_offset[..], false)] {
        for capacity in [0, OVERLAP_PLAIN.len()] {
            let assert_error = |result| {
                let error = match result {
                    Err(FrameDecoderError::FailedToReadBlockBody(
                        DecodeBlockContentError::DecompressBlockError(error),
                    )) => error,
                    other => panic!("corruption must precede TargetTooSmall: {:?}", other),
                };
                if is_extra_bits {
                    assert!(matches!(
                        error,
                        DecompressBlockError::DecodeSequenceError(DecodeSequenceError::ExtraBits {
                            bits_remaining: 8
                        })
                    ));
                } else {
                    assert!(matches!(
                        error,
                        DecompressBlockError::ExecuteSequencesError(
                            ExecuteSequencesError::DecodebufferError(
                                DecodeBufferError::NotEnoughBytesInDictionary { got: 0, need: 4 }
                            )
                        )
                    ));
                }
            };
            let mut storage = vec![SENTINEL; GUARD + capacity + GUARD];
            assert_error(decoder.decode_all(frame, &mut storage[GUARD..GUARD + capacity]));
            assert_guards(&storage, capacity);
            drop(storage);
            let mut out = output_vec(capacity);
            let allocation = out.as_ptr();
            let original_capacity = out.capacity();
            assert_error(decoder.decode_all_to_vec(frame, &mut out).map(|()| 0));
            assert_eq!(out, PREFIX);
            assert_eq!(out.as_ptr(), allocation);
            assert_eq!(out.capacity(), original_capacity);
            drop(out);
            check_success(&mut decoder, OVERLAP, OVERLAP_PLAIN);
        }
    }

    // A checksum read error remains visible after compressed output overflow.
    let mut missing_checksum = OVERLAP.to_vec();
    missing_checksum[4] |= 4;
    assert!(matches!(
        decoder.decode_all(&missing_checksum, &mut []),
        Err(FrameDecoderError::FailedToReadChecksum(_))
    ));
    check_success(&mut decoder, OVERLAP, OVERLAP_PLAIN);
    for (frame, expected) in cases() {
        let truncated = &frame[..frame.len() - 1];
        let capacity = expected.len();
        let mut storage = vec![SENTINEL; GUARD + capacity + GUARD];
        let result = decoder.decode_all(truncated, &mut storage[GUARD..GUARD + capacity]);
        assert!(
            matches!(result, Err(FrameDecoderError::FailedToReadBlockBody(_))),
            "{:?}",
            result
        );
        assert_guards(&storage, capacity);
        drop(storage);

        let mut out = output_vec(capacity);
        let result = decoder.decode_all_to_vec(truncated, &mut out);
        assert!(
            matches!(result, Err(FrameDecoderError::FailedToReadBlockBody(_))),
            "{:?}",
            result
        );
        assert_eq!(out, PREFIX);
        drop(out);
        check_success(&mut decoder, frame, expected);
    }

    // The first frame has already written data when the final skip fails.
    let input = [RAW, &SKIP[..SKIP.len() - 1]].concat();
    let mut storage = [SENTINEL; GUARD + RAW_PLAIN.len() + GUARD];
    let result = decoder.decode_all(&input, &mut storage[GUARD..GUARD + RAW_PLAIN.len()]);
    assert!(
        matches!(result, Err(FrameDecoderError::FailedToSkipFrame)),
        "{:?}",
        result
    );
    assert_guards(&storage, RAW_PLAIN.len());
    let mut out = output_vec(RAW_PLAIN.len());
    let result = decoder.decode_all_to_vec(&input, &mut out);
    assert!(
        matches!(result, Err(FrameDecoderError::FailedToSkipFrame)),
        "{:?}",
        result
    );
    assert_eq!(
        out, PREFIX,
        "even a later-frame error must restore vector length"
    );
    drop(out);
    check_success(&mut decoder, OVERLAP, OVERLAP_PLAIN);
}

#[test]
fn empty_frame_empty_input_and_skip_only_write_nothing() {
    let mut decoder = FrameDecoder::new();
    for input in [&[][..], EMPTY, SKIP] {
        check_success(&mut decoder, input, &[]);
    }
}
