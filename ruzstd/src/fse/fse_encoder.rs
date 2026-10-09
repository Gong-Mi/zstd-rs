use crate::bit_io::BitWriter;
use alloc::vec::Vec;

pub(crate) struct FSEEncoder<'output, V: AsMut<Vec<u8>>> {
    pub(super) table: FSETable,
    writer: &'output mut BitWriter<V>,
}

impl<V: AsMut<Vec<u8>>> FSEEncoder<'_, V> {
    pub fn new(table: FSETable, writer: &mut BitWriter<V>) -> FSEEncoder<'_, V> {
        FSEEncoder { table, writer }
    }

    #[cfg(any(test, feature = "fuzz_exports"))]
    pub fn into_table(self) -> FSETable {
        self.table
    }

    /// Encodes the data using the provided table
    /// Writes
    /// * Table description
    /// * Encoded data
    /// * Last state index
    /// * Padding bits to fill up last byte
    #[cfg(any(test, feature = "fuzz_exports"))]
    pub fn encode(&mut self, data: &[u8]) {
        self.write_table();

        let mut state = self.table.start_state(data[data.len() - 1]);
        for x in data[0..data.len() - 1].iter().rev().copied() {
            let next = self.table.next_state(x, state.index);
            let diff = state.index - next.baseline;
            self.writer.write_bits(diff as u64, next.num_bits as usize);
            state = next;
        }
        self.writer
            .write_bits(state.index as u64, self.acc_log() as usize);

        let bits_to_fill = self.writer.misaligned();
        if bits_to_fill == 0 {
            self.writer.write_bits(1u32, 8);
        } else {
            self.writer.write_bits(1u32, bits_to_fill);
        }
    }

    /// Encodes the data using the provided table but with two interleaved streams
    /// Writes
    /// * Table description
    /// * Encoded data with two interleaved states
    /// * Both Last state indexes
    /// * Padding bits to fill up last byte
    pub fn encode_interleaved(&mut self, data: &[u8]) {
        self.write_table();

        let mut state_1 = self.table.start_state(data[data.len() - 1]);
        let mut state_2 = self.table.start_state(data[data.len() - 2]);

        // The first two symbols are represented by the start states
        // Then encode the state transitions for two symbols at a time
        let mut idx = data.len() - 4;
        loop {
            {
                let state = state_1;
                let x = data[idx + 1];
                let next = self.table.next_state(x, state.index);
                let diff = state.index - next.baseline;
                self.writer.write_bits(diff as u64, next.num_bits as usize);
                state_1 = next;
            }
            {
                let state = state_2;
                let x = data[idx];
                let next = self.table.next_state(x, state.index);
                let diff = state.index - next.baseline;
                self.writer.write_bits(diff as u64, next.num_bits as usize);
                state_2 = next;
            }

            if idx < 2 {
                break;
            }
            idx -= 2;
        }

        // Determine if we have an even or odd number of symbols to encode
        // If odd we need to encode the last states transition and encode the final states in the flipped order
        if idx == 1 {
            let state = state_1;
            let x = data[0];
            let next = self.table.next_state(x, state.index);
            let diff = state.index - next.baseline;
            self.writer.write_bits(diff as u64, next.num_bits as usize);
            state_1 = next;

            self.writer
                .write_bits(state_2.index as u64, self.acc_log() as usize);
            self.writer
                .write_bits(state_1.index as u64, self.acc_log() as usize);
        } else {
            self.writer
                .write_bits(state_1.index as u64, self.acc_log() as usize);
            self.writer
                .write_bits(state_2.index as u64, self.acc_log() as usize);
        }

        let bits_to_fill = self.writer.misaligned();
        if bits_to_fill == 0 {
            self.writer.write_bits(1u32, 8);
        } else {
            self.writer.write_bits(1u32, bits_to_fill);
        }
    }

    fn write_table(&mut self) {
        self.table.write_table(self.writer);
    }

    pub(super) fn acc_log(&self) -> u8 {
        self.table.acc_log()
    }
}

#[derive(Debug, Clone)]
pub struct FSETable {
    /// Indexed by symbol
    pub(super) states: [SymbolStates; 256],
    /// Sum of all states.states.len()
    pub(crate) table_size: usize,
}

impl FSETable {
    pub(crate) fn next_state(&self, symbol: u8, idx: usize) -> &State {
        let states = &self.states[symbol as usize];
        states.get(idx, self.table_size)
    }

    pub(crate) fn start_state(&self, symbol: u8) -> &State {
        let states = &self.states[symbol as usize];
        &states.states[0]
    }

    pub fn acc_log(&self) -> u8 {
        self.table_size.ilog2() as u8
    }

    pub(crate) fn write_table<V: AsMut<Vec<u8>>>(&self, writer: &mut BitWriter<V>) {
        // Byte-exact port of C's FSE_writeNCount_generic (lib/compress/fse_compress.c,
        // zstd v1.5.7): the table description is an LSB-first byte stream, so it is
        // assembled with a local accumulator and appended whole (the frame cursor is
        // byte aligned here). Matching the reference writer byte for byte keeps our
        // streams canonical: the previous bit-by-bit writer produced different (for
        // some shapes non-round-trippable) descriptions for the same probabilities.
        let table_log = self.acc_log();
        let table_size: u32 = 1 << table_log;
        let alphabet = self.states.len();
        let mut out: Vec<u8> = Vec::new();
        let mut bit_stream: u32 = 0;
        let mut bit_count: i32 = 0;
        bit_stream = bit_stream.wrapping_add(((table_log - 5) as u32) << bit_count);
        bit_count += 4;
        let mut remaining: i32 = table_size as i32 + 1;
        let mut threshold: i32 = table_size as i32;
        let mut nb_bits: i32 = table_log as i32 + 1;
        let mut symbol = 0usize;
        let mut previous_is0 = false;
        while symbol < alphabet && remaining > 1 {
            if previous_is0 {
                let start = symbol;
                while symbol < alphabet && self.states[symbol].probability == 0 {
                    symbol += 1;
                }
                if symbol == alphabet {
                    break;
                }
                let mut st = start;
                while symbol >= st + 24 {
                    st += 24;
                    bit_stream = bit_stream.wrapping_add(0xFFFFu32 << bit_count);
                    out.push(bit_stream as u8);
                    out.push((bit_stream >> 8) as u8);
                    bit_stream >>= 16;
                }
                while symbol >= st + 3 {
                    st += 3;
                    bit_stream = bit_stream.wrapping_add(3u32 << bit_count);
                    bit_count += 2;
                }
                bit_stream = bit_stream.wrapping_add(((symbol - st) as u32) << bit_count);
                bit_count += 2;
                if bit_count > 16 {
                    out.push(bit_stream as u8);
                    out.push((bit_stream >> 8) as u8);
                    bit_stream >>= 16;
                    bit_count -= 16;
                }
            }
            {
                let mut count = self.states[symbol].probability;
                symbol += 1;
                let max = (2 * threshold - 1) - remaining;
                remaining -= if count < 0 { -count } else { count };
                count += 1;
                if count >= threshold {
                    count += max;
                }
                bit_stream = bit_stream.wrapping_add((count as u32) << bit_count);
                bit_count += nb_bits;
                if count < max {
                    bit_count -= 1;
                }
                previous_is0 = count == 1;
                if remaining < 1 {
                    panic!("writeNCount: incorrect distribution");
                }
                while remaining < threshold {
                    nb_bits -= 1;
                    threshold >>= 1;
                }
            }
            if bit_count > 16 {
                out.push(bit_stream as u8);
                out.push((bit_stream >> 8) as u8);
                bit_stream >>= 16;
                bit_count -= 16;
            }
        }
        if remaining != 1 {
            panic!("writeNCount: incorrect normalized distribution");
        }
        out.push(bit_stream as u8);
        out.push((bit_stream >> 8) as u8);
        let keep = ((bit_count + 7) / 8) as usize;
        out.truncate(out.len() - 2 + keep);
        writer.append_bytes(&out);
    }
}

#[derive(Debug, Clone)]
pub(super) struct SymbolStates {
    /// Sorted by baseline to allow easy lookup using an index
    pub(super) states: Vec<State>,
    pub(super) probability: i32,
}

impl SymbolStates {
    fn get(&self, idx: usize, max_idx: usize) -> &State {
        let start_search_at = (idx * self.states.len()) / max_idx;
        self.states[start_search_at..]
            .iter()
            .find(|state| state.contains(idx))
            .unwrap()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct State {
    /// How many bits the range of this state needs to be encoded as
    pub(crate) num_bits: u8,
    /// The first index targeted by this state
    pub(crate) baseline: usize,
    /// The last index targeted by this state (baseline + the maximum number with numbits bits allows)
    pub(crate) last_index: usize,
    /// Index of this state in the decoding table
    pub(crate) index: usize,
}

impl State {
    fn contains(&self, idx: usize) -> bool {
        self.baseline <= idx && self.last_index >= idx
    }
}

pub fn build_table_from_data(
    data: impl Iterator<Item = u8>,
    max_log: u8,
    avoid_0_numbit: bool,
) -> FSETable {
    let mut counts = [0; 256];
    let mut max_symbol = 0;
    for x in data {
        counts[x as usize] += 1;
    }
    for (idx, count) in counts.iter().copied().enumerate() {
        if count > 0 {
            max_symbol = idx;
        }
    }
    // Avoiding zero-bit states requires a second symbol to receive probability.
    // A zero-only alphabet otherwise has no slot for the redistributed mass.
    if avoid_0_numbit {
        max_symbol = max_symbol.max(1);
    }
    build_table_from_counts(&counts[..=max_symbol], max_log, avoid_0_numbit)
}

fn build_table_from_counts(counts: &[usize], max_log: u8, avoid_0_numbit: bool) -> FSETable {
    // Deterministic proportional normalization at a fixed table log with
    // largest-remainder rounding.
    //
    // The previous normalization derived the table log from the scaled count sum
    // (often T = 2^6 for literal lengths instead of the format maximum 2^9), did
    // coarse integer scaling with the whole deficit dumped into the largest
    // symbol, and only then capped the top probability. All three distortions
    // cost real bytes: on a 4 MiB text corpus the tables sit ~28 KiB above what
    // proportional normalization at the format's maximum table log produces.
    let n = counts.len();
    let total: u64 = counts.iter().map(|c| *c as u64).sum();
    assert!(total > 0);
    let acc_log = max_log.clamp(5, 12);
    let t = 1usize << acc_log;
    let mut probs = alloc::vec![0usize; n];
    let mut assigned = 0usize;
    let mut fracs: alloc::vec::Vec<(u64, usize)> = alloc::vec::Vec::new();
    for (s, &c) in counts.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let scaled = (c as u64) * (t as u64);
        let base = ((scaled / total) as usize).max(1);
        probs[s] = base;
        assigned += base;
        fracs.push((scaled % total, s));
    }
    if assigned < t {
        fracs.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        let mut i = 0usize;
        while assigned < t {
            let s = fracs[i % fracs.len()].1;
            probs[s] += 1;
            assigned += 1;
            i += 1;
        }
    } else if assigned > t {
        while assigned > t {
            let mut best = usize::MAX;
            for (s, &p) in probs.iter().enumerate() {
                if p > 1 && (best == usize::MAX || p > probs[best]) {
                    best = s;
                }
            }
            if best == usize::MAX {
                break;
            }
            probs[best] -= 1;
            assigned -= 1;
        }
    }

    // Honor avoid_0_numbit: keep every symbol's probability at or below T/2 so
    // no symbol owns num_bits == 0 states. The original motivation for the cap
    // is load-bearing: the Huffman weights table is decoded by a
    // bit-exhaustion-terminated interleaved FSE pair (no symbol count), and
    // runs of 0-bit states make the decoder overshoot, corrupting the
    // reconstructed tree (observed as LeftoverIsNotAPowerOf2 / off-by-one top
    // symbols). Beyond correctness, mixed-width ranges also lengthen the
    // encoder's state lookup compared to uniform ones, so the cap is kept for
    // every call site.
    //
    // Single-symbol alphabets: build_table_from_data reserves a second slot
    // when avoid_0_numbit is set, so a receiver for the redistributed mass
    // exists; the fallback below still guards deeper callers.
    if avoid_0_numbit {
        let t2 = t / 2;
        let mut max_idx = 0usize;
        let mut max_val = 0usize;
        for (s, &p) in probs.iter().enumerate() {
            if p > max_val {
                max_val = p;
                max_idx = s;
            }
        }
        if max_val > t2 {
            let redist = max_val - t2;
            let mut si = usize::MAX;
            for (s, &p) in probs.iter().enumerate() {
                if s != max_idx && p > 0 && (si == usize::MAX || p > probs[si]) {
                    si = s;
                }
            }
            if si == usize::MAX {
                for (s, &p) in probs.iter().enumerate() {
                    if s != max_idx && p == 0 {
                        si = s;
                        break;
                    }
                }
            }
            if si != usize::MAX {
                probs[max_idx] = t2;
                probs[si] += redist;
            }
            // If no slot exists at all the alphabet has a single symbol; that
            // table has a single state and nothing to redistribute to.
        }
    }

    let probs: alloc::vec::Vec<i32> = probs.iter().map(|p| *p as i32).collect();
    build_table_from_probabilities(&probs, acc_log)
}

pub(super) fn build_table_from_probabilities(probs: &[i32], acc_log: u8) -> FSETable {
    let mut states = core::array::from_fn::<SymbolStates, 256, _>(|_| SymbolStates {
        states: Vec::new(),
        probability: 0,
    });

    // distribute -1 symbols
    let mut negative_idx = (1 << acc_log) - 1;
    for (symbol, _prob) in probs
        .iter()
        .copied()
        .enumerate()
        .filter(|prob| prob.1 == -1)
    {
        states[symbol].states.push(State {
            num_bits: acc_log,
            baseline: 0,
            last_index: (1 << acc_log) - 1,
            index: negative_idx,
        });
        states[symbol].probability = -1;
        negative_idx -= 1;
    }

    // distribute other symbols

    // Setup all needed states per symbol with their respective index
    let mut idx = 0;
    for (symbol, prob) in probs.iter().copied().enumerate() {
        if prob <= 0 {
            continue;
        }
        states[symbol].probability = prob;
        let states = &mut states[symbol].states;
        for _ in 0..prob {
            states.push(State {
                num_bits: 0,
                baseline: 0,
                last_index: 0,
                index: idx,
            });

            idx = next_position(idx, 1 << acc_log);
            while idx > negative_idx {
                idx = next_position(idx, 1 << acc_log);
            }
        }
        assert_eq!(states.len(), prob as usize);
    }

    // After all states know their index we can determine the numbits and baselines
    for (symbol, prob) in probs.iter().copied().enumerate() {
        if prob <= 0 {
            continue;
        }
        let prob = prob as u32;
        let state = &mut states[symbol];

        // We process the states in their order in the table
        state.states.sort_by_key(|l| l.index);

        let prob_log = if prob.is_power_of_two() {
            prob.ilog2()
        } else {
            prob.ilog2() + 1
        };
        let rounded_up = 1u32 << prob_log;

        // The lower states target double the amount of indexes -> numbits + 1
        let double_states = rounded_up - prob;
        let single_states = prob - double_states;
        let num_bits = acc_log - prob_log as u8;
        let mut baseline = (single_states as usize * (1 << (num_bits))) % (1 << acc_log);
        for (idx, state) in state.states.iter_mut().enumerate() {
            if (idx as u32) < double_states {
                let num_bits = num_bits + 1;
                state.baseline = baseline;
                state.num_bits = num_bits;
                state.last_index = baseline + ((1 << num_bits) - 1);

                baseline += 1 << num_bits;
                baseline %= 1 << acc_log;
            } else {
                state.baseline = baseline;
                state.num_bits = num_bits;
                state.last_index = baseline + ((1 << num_bits) - 1);
                baseline += 1 << num_bits;
            }
        }

        // For encoding we use the states ordered by the indexes they target
        state.states.sort_by_key(|l| l.baseline);
    }

    FSETable {
        table_size: 1 << acc_log,
        states,
    }
}

/// Calculate the position of the next entry of the table given the current
/// position and size of the table.
fn next_position(mut p: usize, table_size: usize) -> usize {
    p += (table_size >> 1) + (table_size >> 3) + 3;
    p &= table_size - 1;
    p
}

const ML_DIST: &[i32] = &[
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

const LL_DIST: &[i32] = &[
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];

const OF_DIST: &[i32] = &[
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

pub(crate) fn default_ml_table() -> FSETable {
    build_table_from_probabilities(ML_DIST, 6)
}

pub(crate) fn default_ll_table() -> FSETable {
    build_table_from_probabilities(LL_DIST, 6)
}

pub(crate) fn default_of_table() -> FSETable {
    build_table_from_probabilities(OF_DIST, 5)
}

#[cfg(test)]
mod normalization_tests {
    use super::{build_table_from_data, build_table_from_probabilities};
    use crate::encoding::{CompressionLevel, FrameCompressor, MatchGeneratorDriver};
    use alloc::vec::Vec;

    #[test]
    fn repeated_zero_symbol_has_nonzero_bit_states() {
        let table = build_table_from_data(core::iter::repeat(0).take(64), 9, true);
        assert_eq!(table.states[0].states.len(), table.table_size / 2);
        assert_eq!(table.states[1].states.len(), table.table_size / 2);
        assert!(table.states[2..]
            .iter()
            .all(|symbol| symbol.states.is_empty()));
        assert!(table.states[0]
            .states
            .iter()
            .all(|state| state.num_bits > 0));
        assert_eq!(
            table
                .states
                .iter()
                .map(|symbol| symbol.states.len())
                .sum::<usize>(),
            table.table_size
        );
        let mut writer = crate::bit_io::BitWriter::new();
        table.write_table(&mut writer);
        writer.flush();
        let bytes = writer.dump();
        let mut decoder = crate::fse::FSETable::new(255);
        decoder.build_decoder(&bytes, table.acc_log()).unwrap();
        crate::fse::check_tables(&decoder, &table);
    }

    #[test]
    fn singleton_alphabets_match_serialized_decoder_tables() {
        for symbol in 0u8..=255 {
            for avoid_zero_bits in [false, true] {
                let data = core::iter::repeat(symbol).take(64);
                let table = build_table_from_data(data, 9, avoid_zero_bits);
                let states = &table.states[symbol as usize].states;
                assert!(!states.is_empty());
                if avoid_zero_bits {
                    assert!(states.iter().all(|state| state.num_bits > 0));
                } else if symbol == 0 {
                    assert!(table.states[1].states.is_empty());
                    assert!(states.iter().all(|state| state.num_bits == 0));
                }
                let mut writer = crate::bit_io::BitWriter::new();
                table.write_table(&mut writer);
                writer.flush();
                let bytes = writer.dump();
                let mut decoder = crate::fse::FSETable::new(255);
                decoder.build_decoder(&bytes, table.acc_log()).unwrap();
                crate::fse::check_tables(&decoder, &table);
            }
        }
    }

    #[test]
    fn zero_literal_length_cross_block_frame_roundtrips() {
        let original = b"abcdefgh".repeat(32768);
        let matcher = MatchGeneratorDriver::new(128 * 1024, 4);
        let mut compressor = FrameCompressor::new_with_matcher(matcher, CompressionLevel::Fastest);
        compressor.set_source(original.as_slice());
        compressor.set_drain(Vec::new());
        compressor.compress();
        let output = compressor.take_drain().unwrap();
        assert_eq!(zstd::decode_all(output.as_slice()).unwrap(), original);
        let mut decoded = Vec::with_capacity(original.len());
        crate::decoding::FrameDecoder::new()
            .decode_all_to_vec(&output, &mut decoded)
            .unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn descriptions_match_reference_bytes() {
        // Golden bytes produced by the reference implementation (libzstd v1.5.7,
        // FSE_writeNCount_generic) for the same probability distributions. These
        // pin the description writer to the reference byte for byte and double as
        // a reader round-trip check (the old bit-writer produced different and,
        // for several of these shapes, non-round-trippable descriptions).
        use crate::bit_io::BitWriter;
        use crate::fse::FSETable;
        use alloc::vec;

        let mut zeros250 = vec![1i32];
        zeros250.extend(core::iter::repeat(0).take(250));
        zeros250.push(511);

        let cases: alloc::vec::Vec<(alloc::vec::Vec<i32>, u8, &str)> = vec![
            (vec![32, 2, 2, 2, 5, 12, 4, 3, 1, 1], 6, "118e314cab3a"),
            (
                vec![
                    256, 227, 3, 3, 1, 1, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1,
                    1, 1,
                ],
                9,
                "14909c1091111111119124490a1d",
            ),
            (
                vec![0, 0, 0, 1, 1, 6, 8, 8, 11, 76, 10, 7, 128],
                8,
                "13a080c0414282d1b440fc03",
            ),
            (
                vec![
                    0, 0, 186, 59, 58, 33, 17, 24, 14, 14, 10, 9, 5, 5, 8, 4, 9, 5, 7, 8, 4, 2, 4,
                    3, 0, 1, 1, 2, 2, 3, 1, 1, 1, 1, 1, 1, 1, 1, 2, 0, 2, 1, 0, 0, 1, 1,
                ],
                9,
                "14a05d3c3b2249e6f15814c3480ac540a98c42029119121191a485590e",
            ),
            (vec![256, 256], 9, "14f03f"),
            (vec![128, 128], 8, "13f80f"),
            (
                vec![256, 128, 64, 32, 16, 8, 4, 2, 1, 1],
                9,
                "14303018c6ec1c",
            ),
            (vec![512], 9, "f43f"),
            (vec![340, 43, 43, 43, 43], 9, "5495c562fd"),
            (
                zeros250,
                9,
                "2420c0ffffffffffffffffffffffffffffffffffffffffcfff",
            ),
        ];

        for (probs, log, expected) in cases {
            let table = build_table_from_probabilities(&probs, log);
            let mut writer = BitWriter::new();
            table.write_table(&mut writer);
            writer.flush();
            let bytes = writer.dump();

            let mut hex = alloc::string::String::new();
            for b in bytes.iter() {
                hex.push_str(&alloc::format!("{b:02x}"));
            }
            assert_eq!(
                hex, expected,
                "description bytes drifted for probs={probs:?} log={log}"
            );

            // The description must also round-trip through our own reader.
            let mut dec = FSETable::new(255);
            let used = dec.build_decoder(&bytes, log).unwrap();
            assert_eq!(used, bytes.len());
            let mut want = probs.clone();
            while want.last() == Some(&0) {
                want.pop();
            }
            let mut got = dec.symbol_probabilities.clone();
            while got.last() == Some(&0) {
                got.pop();
            }
            assert_eq!(
                got, want,
                "read-back mismatch for probs={probs:?} log={log}"
            );
        }
    }
}
