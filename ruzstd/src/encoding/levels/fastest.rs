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
        compress_block(state, &mut compressed);
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
        }
    }
}
