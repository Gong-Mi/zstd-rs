//! Matching algorithm used find repeated parts in the original data
//!
//! The Zstd format relies on finden repeated sequences of data and compressing these sequences as instructions to the decoder.
//! A sequence basically tells the decoder "Go back X bytes and copy Y bytes to the end of your decode buffer".
//!
//! The task here is to efficiently find matches in the already encoded data for the current suffix of the not yet encoded data.

use alloc::vec::Vec;
use core::num::NonZeroUsize;

use super::CompressionLevel;
use super::Matcher;
use super::Sequence;

const MIN_MATCH_LEN: usize = 5;

/// This is the default implementation of the `Matcher` trait. It allocates and reuses the buffers when possible.
pub struct MatchGeneratorDriver {
    vec_pool: Vec<Vec<u8>>,
    match_generator: MatchGenerator,
    slice_size: usize,
}

impl MatchGeneratorDriver {
    /// slice_size says how big the slices should be that are allocated to work with
    /// max_slices_in_window says how many slices should at most be used while looking for matches
    pub(crate) fn new(slice_size: usize, max_slices_in_window: usize) -> Self {
        Self {
            vec_pool: Vec::new(),
            match_generator: MatchGenerator::new(
                max_slices_in_window * slice_size,
                slice_size.next_power_of_two().max(1024),
            ),
            slice_size,
        }
    }
}

impl Matcher for MatchGeneratorDriver {
    fn reset(&mut self, _level: CompressionLevel) {
        let vec_pool = &mut self.vec_pool;

        self.match_generator.reset(|mut data| {
            data.resize(data.capacity(), 0);
            vec_pool.push(data);
        });
    }

    fn window_size(&self) -> u64 {
        self.match_generator.max_window_size as u64
    }

    fn get_next_space(&mut self) -> Vec<u8> {
        self.vec_pool.pop().unwrap_or_else(|| {
            let mut space = alloc::vec![0; self.slice_size];
            space.resize(space.capacity(), 0);
            space
        })
    }

    fn get_last_space(&mut self) -> &[u8] {
        self.match_generator.window.last().unwrap().data.as_slice()
    }

    fn commit_space(&mut self, space: Vec<u8>) {
        let vec_pool = &mut self.vec_pool;
        self.match_generator.add_data(space, |mut data| {
            data.resize(data.capacity(), 0);
            vec_pool.push(data);
        });
    }

    fn start_matching(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) {
        while self.match_generator.next_sequence(&mut handle_sequence) {}
    }
    fn skip_matching(&mut self) {
        self.match_generator.skip_matching();
    }
    fn recycle_space(&mut self, mut space: Vec<u8>) {
        space.resize(space.capacity(), 0);
        self.vec_pool.push(space);
    }
}

/// A store for suffixes (hash table + chains) shared by the whole window.
///
/// Positions are GLOBAL stream positions (`stream_start` of a window entry
/// plus the local index), so a single table covers every slice of the window
/// and each position costs one probe (the previous per-slice layout probed
/// one hash table per slice: deterministic counting showed 2.8x the probes
/// and 4x the hash working set against a 4-slice window).
///
/// Chain links live in a ring buffer indexed by `pos & ring_mask`: a link
/// aliased by a position one ring-length older always sits at least the
/// window size away, so window-distance validation in the walk rejects it.
struct SuffixStore {
    slots: Vec<Option<NonZeroUsize>>,
    /// Ring of previous-position links (value = stream_pos + 1, 0 = chain end)
    links: Vec<u32>,
    ring_mask: usize,
    len_log: u32,
}

impl SuffixStore {
    /// `capacity` = hash slot count (power of two), `ring_len` = link ring
    /// length (power of two, at least the window size).
    fn with_capacity(capacity: usize, ring_len: usize) -> Self {
        Self {
            slots: alloc::vec![None; capacity],
            links: alloc::vec![0u32; ring_len],
            ring_mask: ring_len - 1,
            len_log: capacity.ilog2(),
        }
    }

    fn clear(&mut self) {
        self.slots.clear();
        self.slots.resize(self.slots.capacity(), None);
        self.links.clear();
        self.links.resize(self.links.capacity(), 0);
    }

    #[inline(always)]
    fn insert(&mut self, suffix: &[u8], stream_pos: usize) {
        #[cfg(feature = "encstats")]
        crate::encstats::bump(crate::encstats::INSERTS, 1);
        let key = self.key(suffix);
        let slot = &mut self.slots[key];
        if let Some(prev) = *slot {
            let prev = <NonZeroUsize as Into<usize>>::into(prev) - 1;
            let idx = stream_pos & self.ring_mask;
            // u32 stream positions cover frames up to 4 GiB; beyond that the
            // link wraps and the distance check treats it as a chain end.
            self.links[idx] = (prev as u32).wrapping_add(1);
        }
        *slot = Some(NonZeroUsize::new(stream_pos + 1).unwrap());
    }

    /// Previous stream position on the chain for `stream_pos` (None = end).
    #[inline(always)]
    fn link_of(&self, stream_pos: usize) -> Option<usize> {
        match self.links[stream_pos & self.ring_mask] {
            0 => None,
            v => Some(v as usize - 1),
        }
    }

    #[inline(always)]
    fn get(&self, suffix: &[u8]) -> Option<usize> {
        #[cfg(feature = "encstats")]
        crate::encstats::bump(crate::encstats::PROBES, 1);
        let key = self.key(suffix);
        let hit = self.slots[key].map(|x| <NonZeroUsize as Into<usize>>::into(x) - 1);
        #[cfg(feature = "encstats")]
        if hit.is_some() {
            crate::encstats::bump(crate::encstats::HITS, 1);
        }
        hit
    }

    #[inline(always)]
    fn key(&self, suffix: &[u8]) -> usize {
        let s0 = suffix[0] as u64;
        let s1 = suffix[1] as u64;
        let s2 = suffix[2] as u64;
        let s3 = suffix[3] as u64;
        let s4 = suffix[4] as u64;

        const POLY: u64 = 0xCF3BCCDCABu64;

        let s0 = (s0 << 24).wrapping_mul(POLY);
        let s1 = (s1 << 32).wrapping_mul(POLY);
        let s2 = (s2 << 40).wrapping_mul(POLY);
        let s3 = (s3 << 48).wrapping_mul(POLY);
        let s4 = (s4 << 56).wrapping_mul(POLY);

        let index = s0 ^ s1 ^ s2 ^ s3 ^ s4;
        let index = index >> (64 - self.len_log);
        index as usize % self.slots.len()
    }
}

/// We keep a window of a few of these entries
/// All of these are valid targets for a match to be generated for
struct WindowEntry {
    data: Vec<u8>,
    /// Global stream position of this slice's first byte (monotonic across
    /// the whole frame); candidates from any entry are addressed through it.
    stream_start: usize,
}

pub(crate) struct MatchGenerator {
    max_window_size: usize,
    /// Data window we are operating on to find matches
    /// The data we want to find matches for is in the last slice
    window: Vec<WindowEntry>,
    window_size: usize,
    #[cfg(debug_assertions)]
    concat_window: Vec<u8>,
    /// Index in the last slice that we already processed
    suffix_idx: usize,
    /// 距上次命中所搜过的字节数，用于对齐 C 的 step 加速（见 next_sequence）
    step_dist: usize,
    /// Gets updated when a new sequence is returned to point right behind that sequence
    last_idx_in_sequence: usize,
    /// Hash table + chains covering the whole window (global stream positions)
    store: SuffixStore,
    /// Global stream position just past the end of the newest slice
    stream_cumulative: usize,
}

impl MatchGenerator {
    /// max_size defines how many bytes will be used at most in the window used for matching
    fn new(max_size: usize, suffix_slot_capacity: usize) -> Self {
        Self {
            max_window_size: max_size,
            window: Vec::new(),
            window_size: 0,
            #[cfg(debug_assertions)]
            concat_window: Vec::new(),
            suffix_idx: 0,
            step_dist: 0,
            last_idx_in_sequence: 0,
            store: SuffixStore::with_capacity(
                suffix_slot_capacity.next_power_of_two().max(1024),
                max_size.next_power_of_two().max(1024),
            ),
            stream_cumulative: 0,
        }
    }

    fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>)) {
        self.window_size = 0;
        #[cfg(debug_assertions)]
        self.concat_window.clear();
        self.suffix_idx = 0;
        self.last_idx_in_sequence = 0;
        self.step_dist = 0;
        self.store.clear();
        self.stream_cumulative = 0;
        self.window.drain(..).for_each(|entry| {
            reuse_space(entry.data);
        });
    }

    /// Processes bytes in the current window until either a match is found or no more matches can be found
    /// * If a match is found handle_sequence is called with the Triple variant
    /// * If no more matches can be found but there are bytes still left handle_sequence is called with the Literals variant
    /// * If no more matches can be found and no more bytes are left this returns false
    fn next_sequence(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) -> bool {
        loop {
            let last_entry = self.window.last().unwrap();
            let data_slice = &last_entry.data;

            // We already reached the end of the window, check if we need to return a Literals{}
            if self.suffix_idx >= data_slice.len() {
                if self.last_idx_in_sequence != self.suffix_idx {
                    let literals = &data_slice[self.last_idx_in_sequence..];
                    self.last_idx_in_sequence = self.suffix_idx;
                    handle_sequence(Sequence::Literals { literals });
                    return true;
                } else {
                    return false;
                }
            }

            // If the remaining data is smaller than the minimum match length we can stop and return a Literals{}
            let data_slice = &data_slice[self.suffix_idx..];
            if data_slice.len() < MIN_MATCH_LEN {
                let last_idx_in_sequence = self.last_idx_in_sequence;
                self.last_idx_in_sequence = last_entry.data.len();
                self.suffix_idx = last_entry.data.len();
                handle_sequence(Sequence::Literals {
                    literals: &last_entry.data[last_idx_in_sequence..],
                });
                return true;
            }

            // This is the key we are looking to find a match for
            let key = &data_slice[..MIN_MATCH_LEN];

            // Single shared store: one probe per position, one chain spanning
            // the whole window (newest first). 有界链走查（与比较融合）：
            //  - 内容比较是成本主项 ⇒ 预算 CHAIN_CMP_MAX 次；
            //  - "太近"候选（切片 < MIN_MATCH_LEN，步进加速下常见）不消耗比较预算，
            //    只走链（否则预算全被近邻吃掉，单测 matches 会退化成找不到匹配）；
            //  - 走查总步数另有上限，防长链退化。
            // 单表下这些预算天然全窗口共享（旧逐切片布局会按切片数重复计费）。
            const CHAIN_CMP_MAX: usize = 4;
            const LONG_HIT_CUT: usize = 8;
            const CHAIN_WALK_MAX: usize = 32;
            let mut candidate = None;
            let mut walked = 0usize;
            let mut cmps = 0usize;
            // 首次比较命中长度：条件预算用它决定要不要花第二次比较
            let mut last_hit_len = 0usize;
            let current_stream = last_entry.stream_start + self.suffix_idx;
            let mut cur = self.store.get(key);
            while let Some(pos) = cur {
                // 链按位置降序；出窗口即可收链（更老的位置只会更远）
                let dist = current_stream - pos;
                if dist > self.window_size {
                    break;
                }
                // 定位候选所在切片（窗口内 ≤ 切片数 次比较）
                let (match_entry_idx, match_local) = {
                    let mut found = None;
                    for (idx, entry) in self.window.iter().enumerate() {
                        if pos >= entry.stream_start && pos < entry.stream_start + entry.data.len()
                        {
                            found = Some((idx, pos - entry.stream_start));
                            break;
                        }
                    }
                    match found {
                        Some(x) => x,
                        None => break,
                    }
                };
                let match_slice = &self.window[match_entry_idx].data[match_local..];

                if match_slice.len() < MIN_MATCH_LEN {
                    // 太近的候选给不出合法匹配，只走链不比较
                    walked += 1;
                    if walked >= CHAIN_WALK_MAX {
                        break;
                    }
                    cur = self.store.link_of(pos);
                    continue;
                }
                if cmps >= CHAIN_CMP_MAX && !(cmps == 1 && last_hit_len >= LONG_HIT_CUT) {
                    break;
                }
                cmps += 1;

                // Check how long the common prefix actually is
                let match_len = Self::common_prefix_len(match_slice, data_slice);

                // Collisions in the suffix store might make this check fail
                if match_len >= MIN_MATCH_LEN {
                    let offset = dist;

                    // If we are in debug/tests make sure the match we found is actually at the offset we calculated
                    #[cfg(debug_assertions)]
                    {
                        let unprocessed = last_entry.data.len() - self.suffix_idx;
                        let start = self.concat_window.len() - unprocessed - offset;
                        let end = start + match_len;
                        let check_slice = &self.concat_window[start..end];
                        debug_assert_eq!(check_slice, &match_slice[..match_len]);
                    }

                    if let Some((_, _, old_offset, old_match_len)) = candidate {
                        if match_len > old_match_len
                            || (match_len == old_match_len && offset < old_offset)
                        {
                            candidate = Some((match_entry_idx, match_local, offset, match_len));
                        }
                    } else {
                        candidate = Some((match_entry_idx, match_local, offset, match_len));
                    }
                    last_hit_len = match_len;
                    cur = self.store.link_of(pos);
                } else {
                    // 候选切片足够长却内容不匹配 ⇒ 放弃整条链：同槽更老的候选
                    // 内容命中概率只低不高（确定性计数实测：不断链时
                    // common_prefix_len 调用数是基线的数倍，纯浪费）。
                    break;
                }
            }

            if let Some((match_entry_idx, match_index, offset, mut match_len)) = candidate {
                // Catch-up: 沿历史匹配与当前位置同时向前倒退，尽可能把前驱字面量合并进 match (对齐 C zstd_fast.c)
                let last_entry = self.window.last().unwrap();
                let match_data = &self.window[match_entry_idx].data[..match_index];
                let curr_data = &last_entry.data[self.last_idx_in_sequence..self.suffix_idx];
                let max_back = match_data.len().min(curr_data.len());
                let mut back = 0;
                while back < max_back {
                    if match_data[match_data.len() - 1 - back]
                        == curr_data[curr_data.len() - 1 - back]
                    {
                        back += 1;
                    } else {
                        break;
                    }
                }

                // Fast mode: skip hash insertion for positions within the match.
                // C zstd's fast strategy does the same — only searched positions
                // get inserted. This trades a small ratio loss for large speed gain
                // (avoids O(match_len) hash inserts per match).
                // We still insert the current position's key so future lookups work.
                let last_entry = self.window.last().unwrap();
                let key = &last_entry.data[self.suffix_idx..self.suffix_idx + MIN_MATCH_LEN];
                let current_stream = last_entry.stream_start + self.suffix_idx;
                self.store.insert(key, current_stream);

                // 扣除倒退吸纳的字面量
                let last_entry = self.window.last().unwrap();
                let match_start = self.suffix_idx - back;
                let literals = &last_entry.data[self.last_idx_in_sequence..match_start];

                // Update the indexes, all indexes upto and including the current index have been included in a sequence now
                self.step_dist = 0;
                self.suffix_idx += match_len;
                match_len += back;
                self.last_idx_in_sequence = self.suffix_idx;
                handle_sequence(Sequence::Triple {
                    literals,
                    offset,
                    match_len,
                });

                return true;
            }

            let last_entry = self.window.last().unwrap();
            let key = &last_entry.data[self.suffix_idx..self.suffix_idx + MIN_MATCH_LEN];
            let current_stream = last_entry.stream_start + self.suffix_idx;
            self.store.insert(key, current_stream);
            // Step acceleration, aligned with C's fast path
            // (`ZSTD_compressBlock_fast_generic`, lib/compress/zstd_fast.c:
            //  `step = stepSize` / `if (ip1 >= nextStep) { step++; nextStep += kStepIncr; }`
            //  with kSearchStrength=8 => kStepIncr=256, and `step` reset on every match).
            // The step starts at 1 and gains +1 for every 256 bytes searched since the
            // last match, with no upper bound: right after a match the search stays
            // dense (ratio), while a match-free run (incompressible data) skips ahead
            // quadratically, so a whole block costs O(sqrt(n)) probes instead of O(n).
            let data_len = last_entry.data.len();
            let step = 1 + self.step_dist / 256;
            self.step_dist += step;
            self.suffix_idx = (self.suffix_idx + step).min(data_len);
        }
    }

    /// Find the common prefix length between two byte slices
    #[inline(always)]
    fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
        Self::mismatch_chunks::<8>(a, b)
    }

    /// Find the common prefix length between two byte slices with a configurable chunk length
    /// This enables vectorization optimizations
    fn mismatch_chunks<const N: usize>(xs: &[u8], ys: &[u8]) -> usize {
        let off = core::iter::zip(xs.chunks_exact(N), ys.chunks_exact(N))
            .take_while(|(x, y)| x == y)
            .count()
            * N;
        off + core::iter::zip(&xs[off..], &ys[off..])
            .take_while(|(x, y)| x == y)
            .count()
    }

    /// Process bytes and add the suffixes to the suffix store up to a specific index
    #[inline(always)]
    fn add_suffixes_till(&mut self, idx: usize) {
        let last_entry = self.window.last_mut().unwrap();
        if last_entry.data.len() < MIN_MATCH_LEN {
            return;
        }
        let base = last_entry.stream_start;
        let slice = &self.window.last().unwrap().data[self.suffix_idx..idx];
        for (key_index, key) in slice.windows(MIN_MATCH_LEN).enumerate() {
            self.store.insert(key, base + self.suffix_idx + key_index);
        }
    }

    /// Skip matching for the whole current window entry
    fn skip_matching(&mut self) {
        let len = self.window.last().unwrap().data.len();
        self.add_suffixes_till(len);
        self.suffix_idx = len;
        self.last_idx_in_sequence = len;
    }

    /// Add a new window entry. Will panic if the last window entry hasn't been processed properly.
    /// If any resources are released by pushing the new entry they are returned via the callback
    fn add_data(&mut self, data: Vec<u8>, reuse_space: impl FnMut(Vec<u8>)) {
        assert!(
            self.window.is_empty() || self.suffix_idx == self.window.last().unwrap().data.len()
        );
        self.reserve(data.len(), reuse_space);
        #[cfg(debug_assertions)]
        self.concat_window.extend_from_slice(&data);

        let len = data.len();
        let stream_start = self.stream_cumulative;
        self.stream_cumulative += len;
        self.window.push(WindowEntry { data, stream_start });
        self.window_size += len;
        self.suffix_idx = 0;
        self.last_idx_in_sequence = 0;
        self.step_dist = 0;
    }

    /// Reserve space for a new window entry
    /// If any resources are released by pushing the new entry they are returned via the callback
    fn reserve(&mut self, amount: usize, mut reuse_space: impl FnMut(Vec<u8>)) {
        assert!(self.max_window_size >= amount);
        while self.window_size + amount > self.max_window_size {
            let removed = self.window.remove(0);
            self.window_size -= removed.data.len();
            #[cfg(debug_assertions)]
            self.concat_window.drain(0..removed.data.len());

            // The shared store keeps its slots/links; entries dropped from the
            // window are rejected by the distance check during the walk.
            let WindowEntry {
                data: leaked_vec,
                stream_start: _,
            } = removed;
            reuse_space(leaked_vec);
        }
    }
}

#[test]
fn matches() {
    let mut matcher = MatchGenerator::new(1000, 1024);
    let mut original_data = Vec::new();
    let mut reconstructed = Vec::new();

    let assert_seq_equal = |seq1: Sequence<'_>, seq2: Sequence<'_>, reconstructed: &mut Vec<u8>| {
        assert_eq!(seq1, seq2);
        match seq2 {
            Sequence::Literals { literals } => reconstructed.extend_from_slice(literals),
            Sequence::Triple {
                literals,
                offset,
                match_len,
            } => {
                reconstructed.extend_from_slice(literals);
                let start = reconstructed.len() - offset;
                let end = start + match_len;
                if end <= reconstructed.len() {
                    reconstructed.extend_from_within(start..end);
                } else {
                    // Overlapping match (match_len > offset): the decoder copies
                    // the periodic run byte by byte as the buffer grows; mirror
                    // that here instead of extend_from_within (which requires
                    // end <= len).
                    for i in 0..match_len {
                        let b = reconstructed[start + i];
                        reconstructed.push(b);
                    }
                }
            }
        }
    };

    matcher.add_data(alloc::vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0], |_| {});
    original_data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| {
        // 全前向扩展（对齐 C 的 ZSTD_count）：十个 0 由 1 字面量 + 距离 1 的
        // 9 字节重叠匹配覆盖（旧截断策略产出 5+5@5，更差且非 C 行为）。
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[0],
                offset: 1,
                match_len: 9,
            },
            &mut reconstructed,
        )
    });

    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0,],
        |_| {},
    );
    original_data.extend_from_slice(&[
        1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0,
    ]);

    matcher.next_sequence(|seq| {
        // 全前向扩展：pos 6 处的候选不再止于当前位置，[1..6] 两个完整重复
        // 一次吃满（12 字节），而非旧行为的 2×6。
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[1, 2, 3, 4, 5, 6],
                offset: 6,
                match_len: 12,
            },
            &mut reconstructed,
        )
    });
    matcher.next_sequence(|seq| {
        // 全前向扩展把两段 [1..6] 并进上一条 12 字节匹配；随后只剩尾部 5 个 0，
        // 仍由跨块候选覆盖（旧表征里它是第三条：offset 23×5）。
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 27,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0],
        |_| {},
    );
    original_data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| {
        // 全前向扩展改变走位后，本块 [1..6] 的等长候选仍是更近者，但偏移随
        // 前序覆盖变化：11 → 17。（取用规则不变：等长取更近，流逐字节重建。）
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 17,
                match_len: 6,
            },
            &mut reconstructed,
        )
    });
    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[7, 8, 9, 10, 11],
                offset: 16,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(alloc::vec![0, 0, 0, 0, 0], |_| {});
    original_data.extend_from_slice(&[0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 5,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(alloc::vec![7, 8, 9, 10, 11], |_| {});
    original_data.extend_from_slice(&[7, 8, 9, 10, 11]);

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 15,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(alloc::vec![1, 3, 5, 7, 9], |_| {});
    matcher.skip_matching();
    original_data.extend_from_slice(&[1, 3, 5, 7, 9]);
    reconstructed.extend_from_slice(&[1, 3, 5, 7, 9]);
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(alloc::vec![1, 3, 5, 7, 9], |_| {});
    original_data.extend_from_slice(&[1, 3, 5, 7, 9]);

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 5,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23],
        |_| {},
    );
    original_data.extend_from_slice(&[0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23]);

    // Characterization: with the C-aligned step acceleration (step=1 at the start,
    // +1 per 256 bytes searched since the last match, reset on match), the search is
    // dense right after the start, so the repeat at idx 7 IS matched. The previous
    // position-based acceleration skipped idx 7 and emitted the whole block as one
    // Literals sequence — i.e. that scheme paid for speed with missed matches.
    // Sequences: 7 literals + Triple(offset=5, len=5), then the 2-byte tail.
    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[0, 0, 11, 13, 15, 17, 20],
                offset: 5,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Literals {
                literals: &[21, 23],
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    assert_eq!(reconstructed, original_data);
}

#[test]
fn chain_reaches_old_positions() {
    let mut store = SuffixStore::with_capacity(64, 64);
    for pos in 0..5usize {
        store.insert(&[0u8; 8][..MIN_MATCH_LEN], pos);
    }
    let mut cur = store.get(&[0u8; 8][..MIN_MATCH_LEN]);
    let mut seen = Vec::new();
    while let Some(p) = cur {
        seen.push(p);
        cur = store.link_of(p);
    }
    assert_eq!(seen, alloc::vec![4, 3, 2, 1, 0], "链走查应能回到最老位置");
}

#[cfg(test)]
mod tests {
    use crate::encoding::{CompressionLevel, FrameCompressor};
    use alloc::vec::Vec;

    /// Repetitive input must be covered by long overlapping matches instead of
    /// the offset-doubling cascade. The previous candidate truncation ("slice
    /// ends at the current position") structurally forbade overlapping matches
    /// and produced 11~12 sequences per 128 KiB block for this pattern
    /// (~750 bytes for 1 MiB; full forward extension measures 309).
    #[test]
    fn repetitive_input_encodes_without_offset_cascade() {
        let original = b"abcdefgh".repeat(131072);
        let mut compressor = FrameCompressor::new(CompressionLevel::Fastest);
        compressor.set_source(original.as_slice());
        compressor.set_drain(Vec::new());
        compressor.compress();
        let output = compressor.take_drain().unwrap();
        assert!(
            output.len() < 450,
            "repetitive 1 MiB input produced {} bytes; the offset-doubling cascade is likely back",
            output.len()
        );
        // The stream must decode back both with the C reference and our decoder.
        assert_eq!(zstd::decode_all(output.as_slice()).unwrap(), original);
        let mut decoded = Vec::with_capacity(original.len());
        crate::decoding::FrameDecoder::new()
            .decode_all_to_vec(&output, &mut decoded)
            .unwrap();
        assert_eq!(decoded, original);
    }
}
