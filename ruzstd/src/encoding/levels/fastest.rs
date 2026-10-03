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
    // Check the RLE sample before scanning the rest of a potentially RLE block.
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
        // Byte diversity does not rule out LZ matches elsewhere in the block.
        // Try compression and retain the existing size-based raw fallback.

        // Compress as a standard compressed block
        let mut compressed = Vec::new();
        state.matcher.commit_space(uncompressed_data);
        let offset_hist = state.offset_hist;
        let entropy_updates = compress_block(state, &mut compressed);
        let compressed_size = compressed.len();
        // If compression does not shrink the block, store it raw instead.
        // Also preserve the format guard that compressed blocks must not
        // exceed the maximum block size.
        if compressed_size >= block_size as usize || compressed_size > MAX_BLOCK_SIZE as usize {
            // Raw blocks do not contribute to the repeat-offset history.
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
            entropy_updates.commit(state);
        }
    }
}

#[cfg(test)]
mod entropy_tests {
    use super::compress_fastest;
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
    fn huffman_trial_updates_are_deferred_until_commit() {
        use crate::encoding::Matcher;
        let mut seed = 19u64;
        let data: Vec<u8> = (0..8192)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed % 16) as u8
            })
            .collect();
        for commit in [false, true] {
            let mut state = state();
            state.matcher.commit_space(data.clone());
            let mut compressed = Vec::new();
            let updates = crate::encoding::blocks::compress_block(&mut state, &mut compressed);
            assert_eq!(compressed[0] & 3, 2, "fixture must encode a Huffman table");
            assert!(state.last_huff_table.is_none());
            if commit {
                updates.commit(&mut state);
                assert!(state.last_huff_table.is_some());
            } else {
                drop(updates);
                assert!(state.last_huff_table.is_none());
                assert!(state.fse_tables.ll_previous.is_none());
                assert!(state.fse_tables.ml_previous.is_none());
                assert!(state.fse_tables.of_previous.is_none());
            }
        }
    }

    #[test]
    fn raw_trial_does_not_install_fse_tables() {
        let mut state = state();
        let mut output = Vec::new();
        compress_fastest(&mut state, false, b"abcdefabcdef".to_vec(), &mut output);
        assert_eq!((output[0] >> 1) & 3, 0);
        assert!(state.fse_tables.ll_previous.is_none());
        assert!(state.fse_tables.ml_previous.is_none());
        assert!(state.fse_tables.of_previous.is_none());
    }

    fn fse_tables_bytes(state: &CompressState<MatchGeneratorDriver>) -> [Vec<u8>; 3] {
        let tables = &state.fse_tables;
        [
            &tables.ll_previous,
            &tables.ml_previous,
            &tables.of_previous,
        ]
        .map(|table| {
            let mut writer = crate::bit_io::BitWriter::new();
            table.as_ref().unwrap().write_table(&mut writer);
            writer.flush();
            writer.dump()
        })
    }

    #[test]
    fn raw_trial_preserves_previously_emitted_tables() {
        let mut state = state();
        let mut output = Vec::new();
        let data = b"the quick brown fox jumps over the lazy dog 0123456789 ".repeat(900);
        compress_fastest(&mut state, false, data, &mut output);
        assert_eq!((output[0] >> 1) & 3, 2);
        let previous = fse_tables_bytes(&state);
        let start = output.len();
        compress_fastest(&mut state, false, b"abcdefabcdef".to_vec(), &mut output);
        assert_eq!((output[start] >> 1) & 3, 0);
        assert_eq!(fse_tables_bytes(&state), previous);
    }

    #[test]
    fn compressed_raw_rle_compressed_preserves_entropy_history() {
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
        let text = b"the quick brown fox jumps over the lazy dog 0123456789 ".repeat(900);
        let inputs = [
            text.clone(),
            b"abcdefabcdef".to_vec(),
            alloc::vec![7; 40],
            text,
        ];
        let mut original = Vec::new();
        let mut previous = None;
        for (index, input) in IntoIterator::into_iter(inputs).enumerate() {
            original.extend_from_slice(&input);
            let start = output.len();
            compress_fastest(&mut state, index == 3, input, &mut output);
            let block_type = (output[start] >> 1) & 3;
            assert_eq!(block_type, [2, 0, 1, 2][index]);
            if index == 0 {
                previous = Some(fse_tables_bytes(&state));
            } else if index < 3 {
                assert_eq!(fse_tables_bytes(&state), *previous.as_ref().unwrap());
            }
        }
        assert_eq!(zstd::decode_all(output.as_slice()).unwrap(), original);
        let mut decoded = Vec::with_capacity(original.len());
        crate::decoding::FrameDecoder::new()
            .decode_all_to_vec(&output, &mut decoded)
            .unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn compressed_after_raw_trial_decodes_with_fresh_entropy_state() {
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
        let mut original = first;
        original.extend_from_slice(&second);
        assert_eq!(zstd::decode_all(output.as_slice()).unwrap(), original);
        let mut decoded = Vec::with_capacity(original.len());
        crate::decoding::FrameDecoder::new()
            .decode_all_to_vec(&output, &mut decoded)
            .unwrap();
        assert_eq!(decoded, original);
    }
}

#[cfg(test)]
mod repcode_tests {
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
    fn raw_fallback_and_rle_preserve_offset_history() {
        let mut seed = 7u64;
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
