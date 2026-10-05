use crate::{
    common::MAX_BLOCK_SIZE,
    encoding::{
        block_header::BlockHeader, blocks::compress_block, frame_compressor::CompressState, Matcher,
    },
};
use alloc::vec::Vec;

/// Compresses a single block at [`crate::encoding::CompressionLevel::Fastest`].
///
/// # Parameters
/// - `state`: [`CompressState`] so the compressor can refer to data before
///   the start of this block
/// - `last_block`: Whether or not this block is going to be the last block in the frame
///   (needed because this info is written into the block header)
/// - `uncompressed_data`: A block's worth of uncompressed data, taken from the
///   larger input
/// - `output`: As `uncompressed_data` is compressed, it's appended to `output`.
#[inline]
pub fn compress_fastest<M: Matcher>(
    state: &mut CompressState<M>,
    last_block: bool,
    uncompressed_data: Vec<u8>,
    output: &mut Vec<u8>,
) {
    let block_size = uncompressed_data.len() as u32;
    // Combined fast-path: check RLE and incompressibility in a single pass
    // over a sample, avoiding the old O(n) RLE scan on every block.
    let sample_len = uncompressed_data.len().min(1024);
    let first_byte = uncompressed_data[0];
    let mut all_same = true;
    for &b in &uncompressed_data[..sample_len] {
        if b != first_byte {
            all_same = false;
        }
    }
    // For blocks larger than the sample, verify RLE on the full block
    if all_same && uncompressed_data.len() > sample_len {
        all_same = uncompressed_data[sample_len..]
            .iter()
            .all(|&x| x == first_byte);
    }

    if all_same {
        let rle_byte = uncompressed_data[0];
        state.matcher.commit_space(uncompressed_data);
        state.matcher.skip_matching();
        let header = BlockHeader {
            last_block,
            block_type: crate::blocks::block::BlockType::RLE,
            block_size,
        };
        // Write the header, then the block
        header.serialize(output);
        output.push(rle_byte);
    } else {
        // Quick incompressibility check: sample up to 1KB of the block and count
        // distinct byte values. If the data is near-maximum entropy (>= 250 distinct
        // values out of 256), hash matching will find nothing useful — skip straight
        // to a raw block. This mirrors C zstd's fast-path acceleration behaviour where
        // the step size grows until it effectively skips the whole block.
        let sample_len = uncompressed_data.len().min(1024);
        let mut seen = [false; 256];
        let mut distinct: u16 = 0;
        for &b in &uncompressed_data[..sample_len] {
            if !seen[b as usize] {
                seen[b as usize] = true;
                distinct += 1;
            }
        }
        if distinct >= 250 {
            // Data is effectively incompressible — emit raw block without matching.
            // Recycle the buffer directly without building suffix indexes.
            let header = BlockHeader {
                last_block,
                block_type: crate::blocks::block::BlockType::Raw,
                block_size,
            };
            header.serialize(output);
            output.extend_from_slice(&uncompressed_data);
            state.matcher.recycle_space(uncompressed_data);
            return;
        }

        // Compress as a standard compressed block
        let mut compressed = Vec::new();
        state.matcher.commit_space(uncompressed_data);
        let offset_hist = state.offset_hist;
        compress_block(state, &mut compressed);
        let compressed_size = compressed.len();
        // If compression does not shrink the block, store it raw instead.
        // Also preserve the format guard that compressed blocks must not
        // exceed the maximum block size.
        if compressed_size >= block_size as usize || compressed_size > MAX_BLOCK_SIZE as usize {
            // Raw blocks contain no sequences, so the decoder keeps its repeat
            // offsets. Discard history updates from the rejected trial block.
            state.offset_hist = offset_hist;
            let header = BlockHeader {
                last_block,
                block_type: crate::blocks::block::BlockType::Raw,
                block_size,
            };
            // Write the header, then the block
            header.serialize(output);
            output.extend_from_slice(state.matcher.get_last_space());
        } else {
            let header = BlockHeader {
                last_block,
                block_type: crate::blocks::block::BlockType::Compressed,
                block_size: compressed_size as u32,
            };
            // Write the header, then the block
            header.serialize(output);
            output.extend(compressed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compress_fastest;
    use crate::decoding::frame::read_frame_header;
    use crate::encoding::{
        frame_compressor::{CompressState, FseTables},
        frame_header::FrameHeader,
        MatchGeneratorDriver,
    };
    use alloc::vec::Vec;

    fn state() -> CompressState<MatchGeneratorDriver> {
        CompressState {
            matcher: MatchGeneratorDriver::new(128 * 1024, 1),
            last_huff_table: None,
            fse_tables: FseTables::new(),
            offset_hist: [1, 4, 8],
        }
    }

    #[test]
    fn raw_fallback_preserves_offset_history() {
        for history in [[1, 4, 8], [17, 9, 2]] {
            let mut state = state();
            state.offset_hist = history;
            let mut output = Vec::new();
            // The matcher finds offset 6, but the compressed block is larger
            // than the input. The decoder sees only raw bytes, no sequences.
            compress_fastest(&mut state, false, b"abcdefabcdef".to_vec(), &mut output);
            assert_eq!((output[0] >> 1) & 3, 0, "fixture must fall back to raw");
            assert_eq!(&output[3..], b"abcdefabcdef");
            assert_eq!(state.offset_hist, history, "raw block changed offsets");
        }
    }

    #[test]
    fn early_raw_and_rle_preserve_offset_history() {
        // Deterministic pseudo-random block: incompressible no matter how good
        // the matcher gets, so this keeps exercising the size-based raw fallback
        // (the previous 0..=255 cycling fixture became compressible once the
        // matcher learned to find the 256-byte repeat).
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let early_raw: Vec<u8> = (0..1024)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        let rle = alloc::vec![7; 64];
        for (input, block_type) in [(early_raw, 0), (rle, 1)] {
            let mut state = state();
            let history = [17, 9, 2];
            state.offset_hist = history;
            let mut output = Vec::new();
            compress_fastest(&mut state, true, input, &mut output);
            assert_eq!((output[0] >> 1) & 3, block_type);
            assert_eq!(state.offset_hist, history);
        }
    }

    #[test]
    fn default_compressor_after_raw_fallback_roundtrips() {
        let mut seed = 7u64;
        let mut original: Vec<u8> = (0..128 * 1024)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        // Keep the sample below the early-raw threshold while leaving the
        // full block incompressible. The short match changes the trial history.
        for byte in &mut original[..1024] {
            *byte %= 249;
        }
        original[..5].copy_from_slice(b"abcde");
        original[102..107].copy_from_slice(b"abcde");
        original.extend_from_slice(&b"ghijkl".repeat(50));
        let output = crate::encoding::compress_to_vec(
            original.as_slice(),
            crate::encoding::CompressionLevel::Fastest,
        );
        let (_, header_size) = read_frame_header(output.as_slice()).unwrap();
        let first_start = header_size as usize;
        assert_eq!((output[first_start] >> 1) & 3, 0);
        let second_start = first_start + 3 + 128 * 1024;
        assert_eq!((output[second_start] >> 1) & 3, 2);

        let decoded = zstd::decode_all(output.as_slice()).unwrap();
        assert!(
            decoded == original,
            "C decoder must reconstruct the original"
        );
        let mut decoded = Vec::with_capacity(original.len());
        crate::decoding::FrameDecoder::new()
            .decode_all_to_vec(&output, &mut decoded)
            .unwrap();
        assert!(
            decoded == original,
            "Rust decoder must reconstruct the original"
        );
    }

    #[test]
    fn compressed_after_raw_fallback_roundtrips() {
        let mut state = state();
        let mut output = Vec::new();
        FrameHeader {
            frame_content_size: None,
            single_segment: false,
            content_checksum: false,
            dictionary_id: None,
            window_size: Some(128 * 1024),
        }
        .serialize(&mut output);

        let first = b"abcdefabcdef".to_vec();
        let second = b"ghijkl".repeat(50);
        let first_start = output.len();
        compress_fastest(&mut state, false, first.clone(), &mut output);
        assert_eq!((output[first_start] >> 1) & 3, 0);
        let second_start = output.len();
        compress_fastest(&mut state, true, second.clone(), &mut output);
        assert_eq!((output[second_start] >> 1) & 3, 2);
        assert_ne!(
            state.offset_hist,
            [1, 4, 8],
            "compressed block must commit offsets"
        );

        let mut original = first;
        original.extend_from_slice(&second);
        let decoded = zstd::decode_all(output.as_slice()).unwrap();
        assert_eq!(decoded, original, "C decoder must reconstruct the original");
        let mut decoded = Vec::with_capacity(original.len());
        crate::decoding::FrameDecoder::new()
            .decode_all_to_vec(&output, &mut decoded)
            .unwrap();
        assert_eq!(decoded, original, "Rust round-trip mismatch");
    }
}
