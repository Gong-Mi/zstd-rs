//! This module contains the decompress_literals function, used to take a
//! parsed literals header and a source and decompress it.

use super::super::blocks::literals_section::{LiteralsSection, LiteralsSectionType};
use super::scratch::HuffmanScratch;
use crate::bit_io::BitReaderReversed;
use crate::decoding::errors::DecompressLiteralsError;
use crate::huff0::{HuffmanDecoder, HuffmanTable};
use alloc::vec::Vec;

/// Decode and decompress the provided literals section into `target`, returning the number of bytes read.
pub fn decode_literals(
    section: &LiteralsSection,
    scratch: &mut HuffmanScratch,
    source: &[u8],
    target: &mut Vec<u8>,
) -> Result<u32, DecompressLiteralsError> {
    match section.ls_type {
        LiteralsSectionType::Raw => {
            target.extend(&source[0..section.regenerated_size as usize]);
            Ok(section.regenerated_size)
        }
        LiteralsSectionType::RLE => {
            target.resize(target.len() + section.regenerated_size as usize, source[0]);
            Ok(1)
        }
        LiteralsSectionType::Compressed | LiteralsSectionType::Treeless => {
            let bytes_read = decompress_literals(section, scratch, source, target)?;

            //return sum of used bytes
            Ok(bytes_read)
        }
    }
}

/// Decompress the provided literals section and source into the provided `target`.
/// This function is used when the literals section is `Compressed` or `Treeless`
///
/// Returns the number of bytes read.
fn decompress_literals(
    section: &LiteralsSection,
    scratch: &mut HuffmanScratch,
    source: &[u8],
    target: &mut Vec<u8>,
) -> Result<u32, DecompressLiteralsError> {
    use DecompressLiteralsError as err;

    let compressed_size = section.compressed_size.ok_or(err::MissingCompressedSize)? as usize;
    let num_streams = section.num_streams.ok_or(err::MissingNumStreams)?;

    target.reserve(section.regenerated_size as usize);
    let source = &source[0..compressed_size];
    let mut bytes_read = 0;

    match section.ls_type {
        LiteralsSectionType::Compressed => {
            //read Huffman tree description
            bytes_read += scratch.table.build_decoder(source)?;
            vprintln!("Built huffman table using {} bytes", bytes_read);
        }
        LiteralsSectionType::Treeless if scratch.table.max_num_bits == 0 => {
            return Err(err::UninitializedHuffmanTable);
        }

        _ => { /* nothing to do, huffman tree has been provided by previous block */ }
    }

    let source = &source[bytes_read as usize..];

    if num_streams == 4 {
        //build jumptable
        if source.len() < 6 {
            return Err(err::MissingBytesForJumpHeader { got: source.len() });
        }
        let jump1 = source[0] as usize + ((source[1] as usize) << 8);
        let jump2 = jump1 + source[2] as usize + ((source[3] as usize) << 8);
        let jump3 = jump2 + source[4] as usize + ((source[5] as usize) << 8);
        bytes_read += 6;
        let source = &source[6..];

        if source.len() < jump3 {
            return Err(err::MissingBytesForLiterals {
                got: source.len(),
                needed: jump3,
            });
        }

        //decode 4 streams
        let stream1 = &source[..jump1];
        let stream2 = &source[jump1..jump2];
        let stream3 = &source[jump2..jump3];
        let stream4 = &source[jump3..];

        // The four streams are independent and their output is *not*
        // interleaved: the first three streams produce exactly `segment_size`
        // bytes each and the fourth one the remainder, so every stream can write
        // straight into its own slice of the output. That is what makes it
        // possible to decode all four at the same time: a single huffman
        // symbol's state update is a short dependency chain, and one stream
        // alone cannot keep the pipeline busy with it (the reference
        // implementation takes four symbols from each stream per iteration for
        // the same reason).
        //
        // Per stream: raw-pointer writes into the pre-reserved `target`, a local
        // bit budget instead of a per-symbol `bits_remaining()` recomputation
        // (`budget == bits_remaining() + max_bits`), and four symbols per refill.
        let regen = section.regenerated_size as usize;
        let base = target.as_mut_ptr();
        let segment_size = (regen + 3) / 4;
        // For valid input the first three streams fill `segment_size` bytes
        // exactly and the fourth one the remainder; the `min`s only keep a
        // malformed header from desynchronising the slices below.
        let len1 = segment_size.min(regen);
        let len2 = segment_size.min(regen - len1);
        let len3 = segment_size.min(regen - len1 - len2);
        let len4 = regen - len1 - len2 - len3;
        let seg = [0usize, len1, len1 + len2, len1 + len2 + len3];
        let seg_len = [len1, len2, len3, len4];

        let max_bits = scratch.table.max_num_bits as isize;
        let (mut dec1, mut br1, mut b1) = init_huffman_stream(&scratch.table, stream1)?;
        let (mut dec2, mut br2, mut b2) = init_huffman_stream(&scratch.table, stream2)?;
        let (mut dec3, mut br3, mut b3) = init_huffman_stream(&scratch.table, stream3)?;
        let (mut dec4, mut br4, mut b4) = init_huffman_stream(&scratch.table, stream4)?;
        let (mut w1, mut w2, mut w3, mut w4) = (0usize, 0usize, 0usize, 0usize);

        // Bulk: while all four streams can take another four symbols, take four
        // from each — 16 symbols per iteration, four independent chains in
        // flight. `budget >= 48 + max_bits` is `bits_remaining() >= 48`, the
        // precondition that makes `decode_batch4` bit-exact with the serial
        // rounds in the tails below.
        while b1 >= 48 + max_bits
            && b2 >= 48 + max_bits
            && b3 >= 48 + max_bits
            && b4 >= 48 + max_bits
            && w1 + 4 <= seg_len[0]
            && w2 + 4 <= seg_len[1]
            && w3 + 4 <= seg_len[2]
            && w4 + 4 <= seg_len[3]
        {
            // SAFETY: every `w + 4 <= len` holds and the four segments are
            // disjoint ranges inside the `regen` bytes reserved above; nothing
            // else writes to `target` while `base` is live.
            b1 -= unsafe { dec1.decode_batch4(&mut br1, base.add(seg[0] + w1)) } as isize;
            w1 += 4;
            b2 -= unsafe { dec2.decode_batch4(&mut br2, base.add(seg[1] + w2)) } as isize;
            w2 += 4;
            b3 -= unsafe { dec3.decode_batch4(&mut br3, base.add(seg[2] + w3)) } as isize;
            w3 += 4;
            b4 -= unsafe { dec4.decode_batch4(&mut br4, base.add(seg[3] + w4)) } as isize;
            w4 += 4;
        }

        // Tails: a stream can run out of bits before the others, so each stream
        // is finished on its own — batch fast path first, then serial rounds,
        // then the same termination and fill checks as before.
        macro_rules! finish_stream {
            ($dec:expr, $br:expr, $w:expr, $b:expr, $off:expr, $len:expr) => {
                while $b >= 48 + max_bits && $w + 4 <= $len {
                    // SAFETY: `$w + 4 <= $len` and the segment lies inside the
                    // reserved `regen` bytes; nothing else writes to `target`.
                    $b -= unsafe { $dec.decode_batch4(&mut $br, base.add($off + $w)) } as isize;
                    $w += 4;
                }
                while $b > 0 {
                    if $w >= $len {
                        return Err(DecompressLiteralsError::DecodedLiteralCountMismatch {
                            decoded: $off + $w + 1,
                            expected: regen,
                        });
                    }
                    // SAFETY: `$w < $len`, the segment lies inside the reserved
                    // `regen` bytes; nothing else writes to `target`.
                    unsafe { base.add($off + $w).write($dec.decode_symbol()) };
                    $w += 1;
                    $b -= $dec.next_state(&mut $br) as isize;
                }
                if $br.bits_remaining() != -max_bits {
                    return Err(DecompressLiteralsError::BitstreamReadMismatch {
                        read_til: $br.bits_remaining(),
                        expected: -max_bits,
                    });
                }
                if $w != $len {
                    return Err(DecompressLiteralsError::DecodedLiteralCountMismatch {
                        decoded: $off + $w,
                        expected: regen,
                    });
                }
            };
        }
        finish_stream!(dec1, br1, w1, b1, seg[0], seg_len[0]);
        finish_stream!(dec2, br2, w2, b2, seg[1], seg_len[1]);
        finish_stream!(dec3, br3, w3, b3, seg[2], seg_len[2]);
        finish_stream!(dec4, br4, w4, b4, seg[3], seg_len[3]);

        // The fill checks above guarantee that every byte of `regen` was written
        // by exactly one stream, so the length can be published.
        unsafe { target.set_len(regen) };

        bytes_read += source.len() as u32;
    } else {
        //just decode the one stream
        assert!(num_streams == 1);
        let (mut decoder, mut br, mut budget) = init_huffman_stream(&scratch.table, source)?;

        // Same shape as the four-stream path: raw-pointer writes into the
        // pre-reserved `target`, a local bit budget instead of a per-symbol
        // `bits_remaining()` recomputation, and a four-symbol batch fast path.
        // `budget == bits_remaining() + max_bits` here as well.
        let regen = section.regenerated_size as usize;
        let base = target.as_mut_ptr();
        let mut written = 0usize;
        let max_bits = scratch.table.max_num_bits as isize;

        while budget >= 48 + max_bits && written + 4 <= regen {
            // SAFETY: `written + 4 <= regen` and `target` is reserved for `regen`
            // bytes; nothing else touches `target` while `base` is live.
            let consumed = unsafe { decoder.decode_batch4(&mut br, base.add(written)) };
            written += 4;
            budget -= consumed as isize;
        }

        while budget > 0 {
            if written >= regen {
                return Err(DecompressLiteralsError::DecodedLiteralCountMismatch {
                    decoded: written + 1,
                    expected: regen,
                });
            }
            // SAFETY: written < regen, and target was reserved for regen bytes;
            // nothing else touches target while `base` is live.
            unsafe { base.add(written).write(decoder.decode_symbol()) };
            written += 1;
            let nb = decoder.next_state(&mut br);
            budget -= nb as isize;
        }
        // `budget <= 0` is where the previous `bits_remaining() > -max_bits` loop
        // stopped as well; an underproduction is still caught by the length check
        // after this function.
        unsafe { target.set_len(written) };

        bytes_read += source.len() as u32;
    }

    if target.len() != section.regenerated_size as usize {
        return Err(DecompressLiteralsError::DecodedLiteralCountMismatch {
            decoded: target.len(),
            expected: section.regenerated_size as usize,
        });
    }

    Ok(bytes_read)
}

/// Create the huffman decoder + reversed bit reader for one literal stream,
/// position it on the first symbol (skip the zero padding at the end of the
/// stream and throw away the first 1 bit, then read the first state) and report
/// the stream's initial bit budget.
///
/// `budget == bits_remaining() + max_num_bits`, which right after the padding
/// skip and `init_state` is `stream_len * 8 - skipped_bits`; the callers track
/// it incrementally from there (subtracting the bits each symbol consumes)
/// instead of recomputing `bits_remaining()` per symbol.
fn init_huffman_stream<'t, 's>(
    table: &'t HuffmanTable,
    stream: &'s [u8],
) -> Result<(HuffmanDecoder<'t>, BitReaderReversed<'s>, isize), DecompressLiteralsError> {
    let mut br = BitReaderReversed::new(stream);
    //skip the 0 padding at the end of the last byte of the bit stream and throw away the first 1 found
    let mut skipped_bits = 0;
    loop {
        let val = br.get_bits(1);
        skipped_bits += 1;
        if val == 1 || skipped_bits > 8 {
            break;
        }
    }
    if skipped_bits > 8 {
        //if more than 7 bits are 0, this is not the correct end of the bitstream. Either a bug or corrupted data
        return Err(DecompressLiteralsError::ExtraPadding { skipped_bits });
    }
    let mut decoder = HuffmanDecoder::new(table);
    decoder.init_state(&mut br);

    let budget = (stream.len() * 8) as isize - skipped_bits as isize;
    Ok((decoder, br, budget))
}
