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
        let entropy_updates = compress_block(state, &mut compressed);
        let compressed_size = compressed.len();
        // If compression does not shrink the block, store it raw instead.
        // Also preserve the format guard that compressed blocks must not
        // exceed the maximum block size.
        if compressed_size >= block_size as usize || compressed_size > MAX_BLOCK_SIZE as usize {
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
