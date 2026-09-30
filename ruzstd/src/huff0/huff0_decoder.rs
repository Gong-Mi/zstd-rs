//! Utilities for decoding Huff0 encoded huffman data.

use crate::bit_io::BitReaderReversed;
use crate::decoding::errors::HuffmanTableError;
use crate::fse::{FSEDecoder, FSETable};
use alloc::vec::Vec;

/// The Zstandard specification limits the maximum length of a code to 11 bits.
pub(crate) const MAX_MAX_NUM_BITS: u8 = 11;

pub struct HuffmanDecoder<'table> {
    table: &'table HuffmanTable,
    /// State is used to index into the table.
    pub state: u64,
}

impl<'t> HuffmanDecoder<'t> {
    /// Create a new decoder with the provided table
    pub fn new(table: &'t HuffmanTable) -> HuffmanDecoder<'t> {
        HuffmanDecoder { table, state: 0 }
    }

    /// Decode the symbol the internal state (cursor) is pointed at and return the
    /// decoded literal.
    pub fn decode_symbol(&mut self) -> u8 {
        self.table.decode[self.state as usize].symbol
    }

    /// Initialize internal state and prepare to decode data. Then, `decode_symbol` can be called
    /// to read the byte the internal cursor is pointing at, and `next_state` can be called to advance
    /// the cursor until the max number of bits has been read.
    pub fn init_state(&mut self, br: &mut BitReaderReversed<'_>) -> u8 {
        let num_bits = self.table.max_num_bits;
        let new_bits = br.get_bits(num_bits);
        self.state = new_bits;
        num_bits
    }

    /// Advance the internal cursor to the next symbol. After this, you can call
    /// `decode_symbol` to read from the new position.
    pub fn next_state(&mut self, br: &mut BitReaderReversed<'_>) -> u8 {
        // self.state stores a small section, or a window of the bit stream. The table can be indexed via this state,
        // telling you how many bits identify the current symbol.
        let num_bits = self.table.decode[self.state as usize].num_bits;
        // New bits are read from the stream
        let new_bits = br.get_bits(num_bits);
        // Shift and mask out the bits that identify the current symbol
        self.state <<= num_bits;
        self.state &= self.table.decode.len() as u64 - 1;
        // The new bits are appended at the end of the current state.
        self.state |= new_bits;
        num_bits
    }

    /// Decode four symbols in one pass, from a single refill of the bit window,
    /// writing them to `dst` and returning the number of bits consumed.
    ///
    /// This is bit-exact with four rounds of `decode_symbol`/`next_state` as
    /// long as the caller has established that at least 48 real bits are still
    /// unread (`br.bits_remaining() >= 48`): four symbols consume at most
    /// `4 * max_num_bits <= 44` bits, and with that many real bits left the
    /// refill inside `unread_window` never has to zero-fill past the end of the
    /// stream, so the bits read are the same ones the serial path would read.
    ///
    /// # Safety
    ///
    /// `dst` must be valid for four byte writes.
    #[inline(always)]
    pub unsafe fn decode_batch4(&mut self, br: &mut BitReaderReversed<'_>, dst: *mut u8) -> u8 {
        let table = &self.table.decode;
        let mask = table.len() as u64 - 1;
        let mut window = br.unread_window();
        let mut state = self.state;
        let mut total = 0u8;

        for i in 0..4 {
            // SAFETY: `state` is masked to `table.len() - 1` after every update
            // and `init_state` reads exactly `max_num_bits` bits, which is the
            // index space of the table.
            let entry = unsafe { *table.get_unchecked(state as usize) };
            // SAFETY: caller guarantees room for four writes.
            unsafe { dst.add(i).write(entry.symbol) };
            let nb = entry.num_bits;
            // `window >> (63 - nb) >> 1` is `window >> (64 - nb)` without the
            // shift overflow at `nb == 0`; with `nb == 0` both terms are zero and
            // the state is unchanged, matching the serial path.
            state = ((state << nb) | (window >> (63 - nb as u32) >> 1)) & mask;
            window <<= nb;
            total += nb;
        }

        self.state = state;
        br.consume(total);
        total
    }

    /// Decode one or two symbols with a single table lookup.
    ///
    /// `window` must be the left-aligned unread region of the bit container
    /// (`BitReaderReversed::unread_window`), i.e. the next bit of the stream is
    /// its most significant bit. Every subsequent lookup of the same batch can
    /// reuse the same window shifted left by the bits consumed so far.
    ///
    /// Returns `(symbols written, bits consumed)`. Both are exactly what the
    /// one-symbol path would produce for the same window, because the table only
    /// stores a second symbol when its code is fully determined by the window.
    ///
    /// `self.state` is kept in sync (`state` is by construction the next
    /// `max_num_bits` bits of the stream), so a caller may switch to
    /// `decode_batch4`/`next_state` afterwards, e.g. to finish a stream with the
    /// serial path.
    ///
    /// # Safety
    ///
    /// `dst` must be valid for two byte writes.
    #[inline(always)]
    pub unsafe fn decode_x2_pair(&mut self, window: u64, dst: *mut u8) -> (u8, u8) {
        // The lookup key is the current `n`-bit look-ahead (`state`) followed by
        // the next stream bit, which is the most significant bit of the unread
        // window. `window` is the unread region (bits *after* the look-ahead), so
        // a caller doing several lookups per refill passes it shifted left by the
        // bits consumed so far.
        let idx = ((self.state as usize) << 1) | (window >> 63) as usize;
        // SAFETY: the table has `1 << dt_log` entries (dt_log <= 12) and `idx`
        // is masked to `dt_log` bits by the shift above, so it is in bounds.
        let e = unsafe { *self.table.decode_x2.get_unchecked(idx) };
        // SAFETY: caller guarantees room for two writes.
        unsafe {
            dst.write(e.seq as u8);
            if e.len == 2 {
                dst.add(1).write((e.seq >> 8) as u8);
            }
        }
        // Slide the look-ahead forward by the bits this lookup consumed: the new
        // `state` is `(state << nb) | next nb stream bits`, masked to
        // `max_num_bits` bits — the same update `next_state` performs per symbol.
        let nb = e.nb as u32;
        if nb != 0 {
            self.state = ((self.state << nb) | (window >> (64 - nb)))
                & (self.table.decode.len() as u64 - 1);
        }
        (e.len, e.nb)
    }
}

/// A Huffman decoding table contains a list of Huffman prefix codes and their associated values
pub struct HuffmanTable {
    decode: Vec<Entry>,
    /// Second-level table for two-symbol lookups ("X2"): indexed by
    /// `max_num_bits + 1` bits instead of `max_num_bits`, so one lookup can
    /// resolve two symbols when the second one's code fits in what is left of
    /// the window. Halves the number of dependent table lookups per symbol.
    decode_x2: Vec<EntryX2>,
    /// The weight of a symbol is the number of occurences in a table.
    /// This value is used in constructing a binary tree referred to as
    /// a Huffman tree. Once this tree is constructed, it can be used to build the
    /// lookup table
    weights: Vec<u8>,
    /// The maximum size in bits a prefix code in the encoded data can be.
    /// This value is used so that the decoder knows how many bits
    /// to read from the bitstream before checking the table. This
    /// value must be 11 or lower.
    pub max_num_bits: u8,
    bits: Vec<u8>,
    bit_ranks: Vec<u32>,
    rank_indexes: Vec<usize>,
    /// In some cases, the list of weights is compressed using FSE compression.
    fse_table: FSETable,
}

impl HuffmanTable {
    /// Create a new, empty table.
    pub fn new() -> HuffmanTable {
        HuffmanTable {
            decode: Vec::new(),
            decode_x2: Vec::new(),

            weights: Vec::with_capacity(256),
            max_num_bits: 0,
            bits: Vec::with_capacity(256),
            bit_ranks: Vec::with_capacity(11),
            rank_indexes: Vec::with_capacity(11),
            fse_table: FSETable::new(255),
        }
    }

    /// Completely empty the table then repopulate as a replica
    /// of `other`.
    pub fn reinit_from(&mut self, other: &Self) {
        self.reset();
        self.decode.extend_from_slice(&other.decode);
        self.weights.extend_from_slice(&other.weights);
        self.max_num_bits = other.max_num_bits;
        self.bits.extend_from_slice(&other.bits);
        self.rank_indexes.extend_from_slice(&other.rank_indexes);
        self.fse_table.reinit_from(&other.fse_table);
    }

    /// Completely empty the table of all data.
    pub fn reset(&mut self) {
        self.decode.clear();
        self.weights.clear();
        self.max_num_bits = 0;
        self.bits.clear();
        self.bit_ranks.clear();
        self.rank_indexes.clear();
        self.fse_table.reset();
    }

    /// Read from `source` and decode the input, populating the huffman decoding table.
    ///
    /// Returns the number of bytes read.
    pub fn build_decoder(&mut self, source: &[u8]) -> Result<u32, HuffmanTableError> {
        self.decode.clear();

        let bytes_used = self.read_weights(source)?;
        self.build_table_from_weights()?;
        Ok(bytes_used)
    }

    /// Read weights from the provided source.
    ///
    /// The huffman table is represented in the input data as a list of weights.
    /// After the header, weights are read, then a Huffman decoding table
    /// can be constructed using that list of weights.
    ///
    /// Returns the number of bytes read.
    fn read_weights(&mut self, source: &[u8]) -> Result<u32, HuffmanTableError> {
        use HuffmanTableError as err;

        if source.is_empty() {
            return Err(err::SourceIsEmpty);
        }
        let header = source[0];
        let mut bits_read = 8;

        match header {
            // If the header byte is less than 128, the series of weights
            // is compressed using two interleaved FSE streams that share
            // a distribution table.
            0..=127 => {
                let fse_stream = &source[1..];
                if header as usize > fse_stream.len() {
                    return Err(err::NotEnoughBytesForWeights {
                        got_bytes: fse_stream.len(),
                        expected_bytes: header,
                    });
                }
                //fse decompress weights
                let bytes_used_by_fse_header = self.fse_table.build_decoder(fse_stream, 6)?;

                if bytes_used_by_fse_header > header as usize {
                    return Err(err::FSETableUsedTooManyBytes {
                        used: bytes_used_by_fse_header,
                        available_bytes: header,
                    });
                }

                vprintln!(
                    "Building fse table for huffman weights used: {}",
                    bytes_used_by_fse_header
                );
                // Huffman headers are compressed using two interleaved
                // FSE bitstreams, where the first state (decoder) handles
                // even symbols, and the second handles odd symbols.
                let mut dec1 = FSEDecoder::new(&self.fse_table);
                let mut dec2 = FSEDecoder::new(&self.fse_table);

                let compressed_start = bytes_used_by_fse_header;
                let compressed_length = header as usize - bytes_used_by_fse_header;

                let compressed_weights = &fse_stream[compressed_start..];
                if compressed_weights.len() < compressed_length {
                    return Err(err::NotEnoughBytesToDecompressWeights {
                        have: compressed_weights.len(),
                        need: compressed_length,
                    });
                }
                let compressed_weights = &compressed_weights[..compressed_length];
                let mut br = BitReaderReversed::new(compressed_weights);

                bits_read += (bytes_used_by_fse_header + compressed_length) * 8;

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
                    return Err(err::ExtraPadding { skipped_bits });
                }

                dec1.init_state(&mut br)?;
                dec2.init_state(&mut br)?;

                self.weights.clear();

                // The two decoders take turns decoding a single symbol and updating their state.
                loop {
                    let w = dec1.decode_symbol();
                    self.weights.push(w);
                    dec1.update_state(&mut br);

                    if br.bits_remaining() <= -1 {
                        //collect final states
                        self.weights.push(dec2.decode_symbol());
                        break;
                    }

                    let w = dec2.decode_symbol();
                    self.weights.push(w);
                    dec2.update_state(&mut br);

                    if br.bits_remaining() <= -1 {
                        //collect final states
                        self.weights.push(dec1.decode_symbol());
                        break;
                    }
                    //maximum number of weights is 255 because we use u8 symbols and the last weight is inferred from the sum of all others
                    if self.weights.len() > 255 {
                        return Err(err::TooManyWeights {
                            got: self.weights.len(),
                        });
                    }
                }
            }
            // If the header byte is greater than or equal to 128,
            // weights are directly represented, where each weight is
            // encoded directly as a 4 bit field. The weights will
            // always be encoded with full bytes, meaning if there's
            // an odd number of weights, the last weight will still
            // occupy a full byte.
            _ => {
                // weights are directly encoded
                let weights_raw = &source[1..];
                let num_weights = header - 127;
                self.weights.resize(num_weights as usize, 0);

                let bytes_needed = if num_weights.is_multiple_of(2) {
                    num_weights as usize / 2
                } else {
                    (num_weights as usize / 2) + 1
                };

                if weights_raw.len() < bytes_needed {
                    return Err(err::NotEnoughBytesInSource {
                        got: weights_raw.len(),
                        need: bytes_needed,
                    });
                }

                for idx in 0..num_weights {
                    if idx % 2 == 0 {
                        self.weights[idx as usize] = weights_raw[idx as usize / 2] >> 4;
                    } else {
                        self.weights[idx as usize] = weights_raw[idx as usize / 2] & 0xF;
                    }
                    bits_read += 4;
                }
            }
        }

        let bytes_read = if bits_read % 8 == 0 {
            bits_read / 8
        } else {
            (bits_read / 8) + 1
        };
        Ok(bytes_read as u32)
    }

    /// Once the weights have been read from the data, you can decode the weights
    /// into a table, and use that table to decode the actual compressed data.
    ///
    /// This function populates the rest of the table from the series of weights.
    fn build_table_from_weights(&mut self) -> Result<(), HuffmanTableError> {
        use HuffmanTableError as err;

        self.bits.clear();
        self.bits.resize(self.weights.len() + 1, 0);

        let mut weight_sum: u32 = 0;
        for w in &self.weights {
            if *w > MAX_MAX_NUM_BITS {
                return Err(err::WeightBiggerThanMaxNumBits { got: *w });
            }
            weight_sum += if *w > 0 { 1_u32 << (*w - 1) } else { 0 };
        }

        if weight_sum == 0 {
            return Err(err::MissingWeights);
        }

        let max_bits = highest_bit_set(weight_sum) as u8;
        let left_over = (1 << max_bits) - weight_sum;

        //left_over must be power of two
        if !left_over.is_power_of_two() {
            return Err(err::LeftoverIsNotAPowerOf2 { got: left_over });
        }

        let last_weight = highest_bit_set(left_over) as u8;

        for symbol in 0..self.weights.len() {
            let bits = if self.weights[symbol] > 0 {
                max_bits + 1 - self.weights[symbol]
            } else {
                0
            };
            self.bits[symbol] = bits;
        }

        self.bits[self.weights.len()] = max_bits + 1 - last_weight;
        self.max_num_bits = max_bits;

        if max_bits > MAX_MAX_NUM_BITS {
            return Err(err::MaxBitsTooHigh { got: max_bits });
        }

        self.bit_ranks.clear();
        self.bit_ranks.resize((max_bits + 1) as usize, 0);
        for num_bits in &self.bits {
            self.bit_ranks[(*num_bits) as usize] += 1;
        }

        //fill with dummy symbols
        self.decode.resize(
            1 << self.max_num_bits,
            Entry {
                symbol: 0,
                num_bits: 0,
            },
        );

        //starting codes for each rank
        self.rank_indexes.clear();
        self.rank_indexes.resize((max_bits + 1) as usize, 0);

        self.rank_indexes[max_bits as usize] = 0;
        for bits in (1..self.rank_indexes.len() as u8).rev() {
            self.rank_indexes[bits as usize - 1] = self.rank_indexes[bits as usize]
                + self.bit_ranks[bits as usize] as usize * (1 << (max_bits - bits));
        }

        assert!(
            self.rank_indexes[0] == self.decode.len(),
            "rank_idx[0]: {} should be: {}",
            self.rank_indexes[0],
            self.decode.len()
        );

        for symbol in 0..self.bits.len() {
            let bits_for_symbol = self.bits[symbol];
            if bits_for_symbol != 0 {
                // allocate code for the symbol and set in the table
                // a code ignores all max_bits - bits[symbol] bits, so it gets
                // a range that spans all of those in the decoding table
                let base_idx = self.rank_indexes[bits_for_symbol as usize];
                let len = 1 << (max_bits - bits_for_symbol);
                self.rank_indexes[bits_for_symbol as usize] += len;
                for idx in 0..len {
                    self.decode[base_idx + idx].symbol = symbol as u8;
                    self.decode[base_idx + idx].num_bits = bits_for_symbol;
                }
            }
        }

        self.build_two_symbol_table();
        Ok(())
    }

    /// Build the two-symbol lookup table from the one-symbol table.
    ///
    /// For every `max_num_bits + 1` bit window: the first symbol comes from the
    /// one-symbol table indexed by the top `max_num_bits` bits, and a second
    /// symbol is stored when its whole code fits into the bits that are left in
    /// the window (`nb1 <= max_num_bits + 1 - nb0`). When fewer than
    /// `max_num_bits` bits are left, the window does not cover the whole lookup
    /// range of the one-symbol table, so the range is only usable when every
    /// index in it maps to the same symbol and length — which a single
    /// comparison of the two ends decides, because the table is filled in code
    /// order and equal entries are therefore contiguous.
    fn build_two_symbol_table(&mut self) {
        let n = self.max_num_bits as usize;
        let size = 1usize << n;
        let x2_size = size << 1;
        self.decode_x2.clear();
        self.decode_x2.resize(
            x2_size,
            EntryX2 {
                seq: 0,
                nb: 0,
                len: 1,
            },
        );

        for idx in 0..x2_size {
            let e0 = self.decode[idx >> 1];
            let nb0 = e0.num_bits as usize;
            if nb0 == 0 {
                // Not reachable for a fully filled table; keep the one-symbol
                // behaviour (emit the symbol, consume no bits) instead of
                // underflowing on the arithmetic below.
                self.decode_x2[idx] = EntryX2 {
                    seq: u16::from(e0.symbol),
                    nb: 0,
                    len: 1,
                };
                continue;
            }

            let room = n + 1 - nb0;
            let base = ((idx << nb0) & (x2_size - 1)) >> 1;
            let unknown = nb0 - 1;
            let second_known = if unknown == 0 {
                self.decode[base].num_bits as usize <= room
            } else {
                let last = base + (1 << unknown) - 1;
                last < size
                    && self.decode[last].symbol == self.decode[base].symbol
                    && self.decode[last].num_bits == self.decode[base].num_bits
                    && self.decode[base].num_bits as usize <= room
            };

            self.decode_x2[idx] = if second_known {
                let e1 = self.decode[base];
                EntryX2 {
                    seq: u16::from(e0.symbol) | (u16::from(e1.symbol) << 8),
                    nb: e0.num_bits + e1.num_bits,
                    len: 2,
                }
            } else {
                EntryX2 {
                    seq: u16::from(e0.symbol),
                    nb: e0.num_bits,
                    len: 1,
                }
            };
        }
    }
}

impl Default for HuffmanTable {
    fn default() -> Self {
        Self::new()
    }
}

/// A single entry in the table contains the decoded symbol/literal and the
/// size of the prefix code.
/// One entry of the two-symbol lookup table.
///
/// `seq` packs the symbols little byte first (first symbol in the low byte),
/// `nb` is the number of bits the present symbols consume together and `len` is
/// how many symbols are present (1 or 2). A second symbol is only stored when
/// its code is fully contained in the lookup window, so consuming `nb` bits and
/// emitting `len` symbols is bit-exact with `len` rounds of the one-symbol path.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EntryX2 {
    pub seq: u16,
    pub nb: u8,
    pub len: u8,
}

#[derive(Copy, Clone, Debug)]
pub struct Entry {
    /// The byte that the prefix code replaces during encoding.
    symbol: u8,
    /// The number of bits the prefix code occupies.
    num_bits: u8,
}

/// Assert that the provided value is greater than zero, and returns the
/// 32 - the number of leading zeros
fn highest_bit_set(x: u32) -> u32 {
    assert!(x > 0);
    u32::BITS - x.leading_zeros()
}

#[cfg(test)]
mod x2_two_symbol_table {
    //! The two-symbol lookup path must produce exactly the same symbols, and
    //! consume exactly the same bits, as the serial one-symbol path.

    use super::*;
    use crate::bit_io::{BitReaderReversed, BitWriter};
    use crate::huff0::huff0_encoder;
    // no_std crate: the test harness has std, but `eprintln!` is not in scope.
    use std::eprintln;

    fn skip_padding(br: &mut BitReaderReversed<'_>) -> i32 {
        let mut skipped = 0;
        loop {
            let v = br.get_bits(1);
            skipped += 1;
            if v == 1 || skipped > 8 {
                break;
            }
        }
        skipped
    }

    #[test]
    fn x2_matches_x1() {
        for seed in 0u32..32 {
            let data: Vec<u8> = (0..300usize)
                .map(|i| ((i as u32 * 7 + seed) % 5) as u8)
                .collect();
            let mut writer = BitWriter::new();
            let enc_table = huff0_encoder::HuffmanTable::build_from_data(&data);
            let mut enc = huff0_encoder::HuffmanEncoder::new(&enc_table, &mut writer);
            enc.encode(&data, true);
            let encoded = writer.dump();
            let mut table = HuffmanTable::new();
            let table_bytes = table.build_decoder(&encoded).unwrap();
            let payload = &encoded[table_bytes as usize..];
            let max_bits = table.max_num_bits as isize;

            // serial
            let mut d1 = HuffmanDecoder::new(&table);
            let mut br1 = BitReaderReversed::new(payload);
            let sk1 = skip_padding(&mut br1);
            d1.init_state(&mut br1);
            let mut out1 = Vec::new();
            while br1.bits_remaining() > -max_bits {
                out1.push(d1.decode_symbol());
                d1.next_state(&mut br1);
            }

            // two-symbol lookups, serial tail
            let mut d2 = HuffmanDecoder::new(&table);
            let mut br2 = BitReaderReversed::new(payload);
            let sk2 = skip_padding(&mut br2);
            d2.init_state(&mut br2);
            let mut out2 = Vec::new();
            let mut budget = (payload.len() * 8) as isize - sk2 as isize;
            let mut buf = [0u8; 8];
            while budget >= 48 + max_bits && out2.len() + 4 <= data.len() {
                let win = br2.unread_window();
                let (l1, n1) = unsafe { d2.decode_x2_pair(win, buf.as_mut_ptr()) };
                let (l2, n2) =
                    unsafe { d2.decode_x2_pair(win << n1, buf.as_mut_ptr().add(l1 as usize)) };
                out2.extend_from_slice(&buf[..(l1 + l2) as usize]);
                br2.consume(n1 + n2);
                budget -= (n1 + n2) as isize;
            }
            while budget > 0 && out2.len() < data.len() {
                out2.push(d2.decode_symbol());
                let nb = d2.next_state(&mut br2);
                budget -= nb as isize;
            }

            assert_eq!(sk1, sk2, "seed {seed}: padding skip differs");
            if out1 != out2 {
                let i = out1
                    .iter()
                    .zip(out2.iter())
                    .position(|(a, b)| a != b)
                    .unwrap_or_else(|| out1.len().min(out2.len()));
                panic!(
                    "seed {seed}: divergence at {i}/{}, max_bits={}, before={:?} x1={:?} x2={:?}",
                    out1.len(),
                    max_bits,
                    &out1[i.saturating_sub(4)..i],
                    &out1[i..(i + 6).min(out1.len())],
                    &out2[i..(i + 6).min(out2.len())]
                );
            }
            assert_eq!(out1.len(), data.len());
        }
    }
}
