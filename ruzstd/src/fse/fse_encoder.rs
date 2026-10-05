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
    /// Indexed by symbol（长度固定 256，内容在堆上）
    ///
    /// 以前这里是内联 `[SymbolStates; 256]`，使 FSETable 自身 8,200 字节：任何
    /// 按值传递都要整块搬移——构造返回、放进 `CompressState`/`FrameCompressor`，
    /// 以及每块把新表存回 `*_previous`。改成堆上 Vec 后 FSETable 只剩
    /// 指针/长度/`table_size`，这些搬移在最终 IR 里不再表现为大 memcpy。
    pub(super) states: Vec<SymbolStates>,
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
        writer.write_bits(self.acc_log() - 5, 4);
        let mut probability_counter = 0usize;
        let probability_sum = 1 << self.acc_log();

        let mut prob_idx = 0;
        while probability_counter < probability_sum {
            let max_remaining_value = probability_sum - probability_counter + 1;
            let bits_to_write = max_remaining_value.ilog2() + 1;
            let low_threshold = ((1 << bits_to_write) - 1) - (max_remaining_value);
            let mask = (1 << (bits_to_write - 1)) - 1;

            let prob = self.states[prob_idx].probability;
            prob_idx += 1;
            let value = (prob + 1) as u32;
            if value < low_threshold as u32 {
                writer.write_bits(value, bits_to_write as usize - 1);
            } else if value > mask {
                writer.write_bits(value + low_threshold as u32, bits_to_write as usize);
            } else {
                writer.write_bits(value, bits_to_write as usize);
            }

            if prob == -1 {
                probability_counter += 1;
            } else if prob > 0 {
                probability_counter += prob as usize;
            } else {
                let mut zeros = 0u8;
                while self.states[prob_idx].probability == 0 {
                    zeros += 1;
                    prob_idx += 1;
                    if zeros == 3 {
                        writer.write_bits(3u8, 2);
                        zeros = 0;
                    }
                }
                writer.write_bits(zeros, 2);
            }
        }
        writer.write_bits(0u8, writer.misaligned());
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
    build_table_from_counts(&counts[..=max_symbol], max_log, avoid_0_numbit)
}

fn build_table_from_counts(counts: &[usize], max_log: u8, avoid_0_numbit: bool) -> FSETable {
    let mut probs = [0; 256];
    let probs = &mut probs[..counts.len()];
    let mut min_count = 0;
    for (idx, count) in counts.iter().copied().enumerate() {
        probs[idx] = count as i32;
        if count > 0 && (count < min_count || min_count == 0) {
            min_count = count;
        }
    }

    // shift all probabilities down so that the lowest are 1
    min_count -= 1;
    let mut max_prob = 0i32;
    for prob in probs.iter_mut() {
        if *prob > 0 {
            *prob -= min_count as i32;
        }
        max_prob = max_prob.max(*prob);
    }

    if max_prob > 0 && max_prob as usize > probs.len() {
        let divisor = max_prob / (probs.len() as i32);
        for prob in probs.iter_mut() {
            if *prob > 0 {
                *prob = (*prob / divisor).max(1)
            }
        }
    }

    // normalize probabilities to a 2^x
    let sum = probs.iter().sum::<i32>();
    assert!(sum > 0);
    let sum = sum as usize;
    let acc_log = (sum.ilog2() as u8 + 1).max(5);
    let acc_log = u8::min(acc_log, max_log);

    if sum < 1 << acc_log {
        // just raise the maximum probability as much as possible
        // TODO is this optimal?
        let diff = (1 << acc_log) - sum;
        let max = probs.iter_mut().max().unwrap();
        *max += diff as i32;
    } else {
        // decrease the smallest ones to 1 first
        let mut diff = sum - (1 << acc_log);
        while diff > 0 {
            let min = probs.iter_mut().filter(|prob| **prob > 1).min().unwrap();
            let decrease = usize::min(*min as usize - 1, diff);
            diff -= decrease;
            *min -= decrease as i32;
        }
    }
    let max = probs.iter_mut().max().unwrap();
    if avoid_0_numbit && *max > 1 << (acc_log - 1) {
        let redistribute = *max - (1 << (acc_log - 1));
        *max -= redistribute;
        let max = *max;

        // find first occurence of the second_max to avoid lifting the last zero
        let second_max = *probs.iter_mut().filter(|x| **x != max).max().unwrap();
        let second_max = probs.iter_mut().find(|x| **x == second_max).unwrap();
        *second_max += redistribute;
        assert!(*second_max <= max);
    }

    build_table_from_probabilities(probs, acc_log)
}

pub(super) fn build_table_from_probabilities(probs: &[i32], acc_log: u8) -> FSETable {
    // 256 个描述符一次性建在堆上（只分配、不搬移），避免内联大数组每次按值传递都 memcpy
    let mut states: Vec<SymbolStates> = Vec::with_capacity(256);
    for _ in 0..256 {
        states.push(SymbolStates {
            states: Vec::new(),
            probability: 0,
        });
    }

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

    debug_assert_eq!(states.len(), 256, "FSETable 必须以符号为下标覆盖 256 项");
    FSETable {
        table_size: 1 << acc_log,
        states,
    }
}

#[cfg(test)]
mod table_layout_tests {
    use super::*;
    use core::mem::size_of;

    /// 本片的不变量：FSETable 自身必须是指针大小的间接结构，不能把 256 个描述符
    /// 内联进去。8,200 字节的内联版本会让每次按值传递产生大 memcpy（构造期
    /// 49,200/49,320、每块 `compress_block` 6×8,200）。
    #[test]
    fn fsetable_is_heap_indirect() {
        let descriptors = size_of::<SymbolStates>() * 256;
        assert_eq!(descriptors, 8_192, "256 个描述符应为 8,192 字节");
        assert!(
            size_of::<FSETable>() < 128,
            "FSETable 大小 {} 字节，说明描述符仍被内联",
            size_of::<FSETable>()
        );
    }

    /// 三种默认表内容不变：仍以符号为下标覆盖 256 项，且任何符号/下标组合上
    /// `next_state` 返回的状态都必须包含该下标（查表语义）。
    #[test]
    fn default_tables_keep_lookup_semantics() {
        for (table, acc_log) in [
            (default_ll_table(), 6u8),
            (default_ml_table(), 6u8),
            (default_of_table(), 5u8),
        ] {
            assert_eq!(table.states.len(), 256);
            assert_eq!(table.table_size, 1 << acc_log);
            assert_eq!(table.acc_log(), acc_log);
            for symbol in 0..256usize {
                let states = &table.states[symbol];
                if states.states.is_empty() {
                    continue;
                }
                for idx in 0..table.table_size {
                    let state = table.next_state(symbol as u8, idx);
                    assert!(
                        state.contains(idx),
                        "symbol {symbol} idx {idx} 未被返回状态覆盖"
                    );
                }
            }
        }
    }

    /// 表构建走的是同一套概率分布，重建两次必须逐项一致（堆化不改变内容）。
    #[test]
    fn rebuilt_default_table_is_identical() {
        let a = default_of_table();
        let b = default_of_table();
        assert_eq!(a.table_size, b.table_size);
        for symbol in 0..256usize {
            let (x, y) = (&a.states[symbol], &b.states[symbol]);
            assert_eq!(x.probability, y.probability);
            assert_eq!(x.states.len(), y.states.len());
            for (p, q) in x.states.iter().zip(y.states.iter()) {
                assert_eq!(
                    (p.num_bits, p.baseline, p.last_index, p.index),
                    (q.num_bits, q.baseline, q.last_index, q.index)
                );
            }
        }
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
