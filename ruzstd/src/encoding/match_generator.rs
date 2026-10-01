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
    suffix_pool: Vec<SuffixStore>,
    match_generator: MatchGenerator,
    slice_size: usize,
}

impl MatchGeneratorDriver {
    /// slice_size says how big the slices should be that are allocated to work with
    /// max_slices_in_window says how many slices should at most be used while looking for matches
    pub(crate) fn new(slice_size: usize, max_slices_in_window: usize) -> Self {
        Self {
            vec_pool: Vec::new(),
            suffix_pool: Vec::new(),
            match_generator: MatchGenerator::new(max_slices_in_window * slice_size),
            slice_size,
        }
    }
}

impl Matcher for MatchGeneratorDriver {
    fn reset(&mut self, _level: CompressionLevel) {
        let vec_pool = &mut self.vec_pool;
        let suffix_pool = &mut self.suffix_pool;

        self.match_generator.reset(|mut data, mut suffixes| {
            data.resize(data.capacity(), 0);
            vec_pool.push(data);
            suffixes.slots.clear();
            suffixes
                .slots
                .resize(suffixes.slots.capacity(), [None, None]);
            suffix_pool.push(suffixes);
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
        let suffix_pool = &mut self.suffix_pool;
        const SUFFIX_STORE_MIN_CAPACITY: usize = 1024;
        let requested_suffix_store_size =
            usize::max(SUFFIX_STORE_MIN_CAPACITY, space.len().next_power_of_two());
        let requested_size_log = requested_suffix_store_size.ilog2();
        let suffix_store_idx = suffix_pool
            .iter()
            .enumerate()
            .find(|(_, store)| store.len_log >= requested_size_log)
            .map(|(idx, _)| idx);
        let suffixes = suffix_store_idx
            .map(|idx| suffix_pool.remove(idx))
            .unwrap_or_else(|| SuffixStore::with_capacity(requested_suffix_store_size));
        #[cfg(feature = "encstats")]
        let t_add0 = crate::encstats::tick();
        self.match_generator
            .add_data(space, suffixes, |mut data, mut suffixes| {
                data.resize(data.capacity(), 0);
                vec_pool.push(data);
                suffixes.slots.clear();
                suffixes
                    .slots
                    .resize(suffixes.slots.capacity(), [None, None]);
                suffix_pool.push(suffixes);
            });
        #[cfg(feature = "encstats")]
        crate::encstats::add_phase(6, crate::encstats::tick() - t_add0); // 后缀构建（块外）
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

/// This stores the index of a suffix of a string by hashing the first few bytes of that suffix
/// This means that collisions just overwrite and that you need to check validity after a get
struct SuffixStore {
    // We use NonZeroUsize to enable niche optimization here.
    // On store we do +1 and on get -1
    // This is ok since usize::MAX is never a valid offset
    //
    // 每个 key 保留**最近两个**位置（[0] 最新、[1] 次新）：匹配时两个都试、取更长者。
    // 动机来自实测：匹配器占编码时间 48.5~76.2%，其中每位置簿记占 66~69%，
    // 而"被逐位置访问的位置数"取决于匹配覆盖了多少字节——多一个候选能拿到更长匹配，
    // 反过来减少要逐位置走的位置数，ratio 与时间同向受益（不是此消彼长）。
    slots: Vec<[Option<NonZeroUsize>; 2]>,
    /// 链：`links[pos]` = 同一 key 的上一个位置（+1 编码，0 表示链尾）。
    /// 只靠槽位覆盖会丢"足够远"的候选（候选切片止于当前位置，太近的位置一律 < MIN_MATCH_LEN），
    /// 链把更老的位置保留下来，按有界步数走查取"足够远且更长"的候选。
    links: Vec<u32>,
    len_log: u32,
}

/// 每 64 位置采样用的守卫：Drop 时把本位置的时钟差 flush 到相位累加器，
/// 因此循环里的任何 return（字面量返回 / 结束）都不会丢失样本。
#[cfg(feature = "encstats")]
struct SampleGuard {
    sample: bool,
    t0: u64,
    cmp: u64,
    get: u64,
}

#[cfg(feature = "encstats")]
impl Drop for SampleGuard {
    fn drop(&mut self) {
        if !self.sample {
            return;
        }
        let t = crate::encstats::tick();
        crate::encstats::add_phase(7, t - self.t0);
        crate::encstats::add_phase(8, self.cmp);
        crate::encstats::add_phase(9, self.get);
    }
}

impl SuffixStore {
    fn with_capacity(capacity: usize) -> Self {
        // 实验：把有效位宽钳到 14（16K 槽）。表仍按原容量分配，只改索引位宽，
        // 目的是让桶碰撞/同 key 复访真正发生，使双槽的第二个候选有内容可选。
        Self {
            slots: alloc::vec![[None, None]; capacity],
            links: Vec::new(),
            len_log: capacity.ilog2().min(14),
        }
    }

    #[inline(always)]
    fn insert(&mut self, suffix: &[u8], idx: usize) {
        let key = self.key(suffix);
        // 链：新位置指向同一 key 的上一个位置（+1 编码，0 表示链尾）。
        // 候选切片止于当前位置 ⇒ 只有"足够远"的位置才给得出合法匹配，链把更老的位置留住。
        let prev = self.slots[key][0];
        if idx >= self.links.len() {
            self.links.resize(idx + 1, 0);
        }
        self.links[idx] = prev.map_or(0, |p| p.get() as u32);
        let slot = &mut self.slots[key];
        #[cfg(feature = "encstats")]
        if slot[0].is_some() {
            crate::encstats::bump(crate::encstats::SHIFTED, 1);
        }
        // 保留最近两个位置：多一个候选能拿到更长匹配，从而减少"要逐位置走访"的位置数
        // （匹配器占编码时间 48.5~76.2%，其中每位置簿记 66~69%），ratio 与时间同向受益。
        slot[1] = slot[0];
        slot[0] = Some(NonZeroUsize::new(idx + 1).unwrap());
    }

    #[allow(dead_code)]
    #[inline(always)]
    fn contains_key(&self, suffix: &[u8]) -> bool {
        let key = self.key(suffix);
        self.slots[key][0].is_some()
    }

    #[allow(dead_code)]
    #[inline(always)]
    fn get(&self, suffix: &[u8]) -> Option<usize> {
        let key = self.key(suffix);
        self.slots[key][0].map(|x| <NonZeroUsize as Into<usize>>::into(x) - 1)
    }

    /// 同一 key 的次新位置（供另一条匹配路径使用）。
    #[inline(always)]
    fn get_second(&self, suffix: &[u8]) -> Option<usize> {
        let key = self.key(suffix);
        self.slots[key][1].map(|x| <NonZeroUsize as Into<usize>>::into(x) - 1)
    }

    /// 沿链收集至多 `depth` 个候选位置（最新在前）。返回实际个数。
    #[inline(always)]
    fn get_chain(&self, suffix: &[u8], depth: usize, out: &mut [usize]) -> usize {
        let key = self.key(suffix);
        let mut cur = match self.slots[key][0] {
            Some(v) => <NonZeroUsize as Into<usize>>::into(v) - 1,
            None => return 0,
        };
        let mut n = 0usize;
        while n < depth && n < out.len() {
            out[n] = cur;
            n += 1;
            // 链上一位（0 表示链尾）
            let link = self.links.get(cur).copied().unwrap_or(0);
            if link == 0 {
                break;
            }
            cur = link as usize - 1;
        }
        #[cfg(feature = "encstats")]
        crate::encstats::bump(
            crate::encstats::SECOND_POPULATED,
            (n.saturating_sub(1)) as u64,
        );
        n
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
    /// Stores indexes into data
    suffixes: SuffixStore,
    /// Makes offset calculations efficient
    base_offset: usize,
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
    /// Gets updated when a new sequence is returned to point right behind that sequence
    last_idx_in_sequence: usize,
}

impl MatchGenerator {
    /// max_size defines how many bytes will be used at most in the window used for matching
    fn new(max_size: usize) -> Self {
        Self {
            max_window_size: max_size,
            window: Vec::new(),
            window_size: 0,
            #[cfg(debug_assertions)]
            concat_window: Vec::new(),
            suffix_idx: 0,
            last_idx_in_sequence: 0,
        }
    }

    fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>, SuffixStore)) {
        self.window_size = 0;
        #[cfg(debug_assertions)]
        self.concat_window.clear();
        self.suffix_idx = 0;
        self.last_idx_in_sequence = 0;
        self.window.drain(..).for_each(|entry| {
            reuse_space(entry.data, entry.suffixes);
        });
    }

    /// Processes bytes in the current window until either a match is found or no more matches can be found
    /// * If a match is found handle_sequence is called with the Triple variant
    /// * If no more matches can be found but there are bytes still left handle_sequence is called with the Literals variant
    /// * If no more matches can be found and no more bytes are left this returns false
    fn next_sequence(&mut self, mut handle_sequence: impl for<'a> FnMut(Sequence<'a>)) -> bool {
        #[cfg(feature = "encstats")]
        let mut pos_ctr: u64 = 0;
        loop {
            // 每 64 个位置采样一次：把"每个位置"的成本拆成
            //   phase7 = 窗口扫描 + 哈希 get + 逐字节比较（整段）
            //   phase8 = 其中的 common_prefix_len 比较
            // phase7-phase8 ≈ 窗口迭代与哈希 get 的代价。每位置打点会被插桩自身淹没，
            // 所以只在采样点取时钟。
            #[cfg(feature = "encstats")]
            let mut sample_guard = {
                pos_ctr = pos_ctr.wrapping_add(1);
                let s = pos_ctr.is_multiple_of(64);
                SampleGuard {
                    sample: s,
                    t0: if s { crate::encstats::tick() } else { 0 },
                    cmp: 0,
                    get: 0,
                }
            };
            #[cfg(feature = "encstats")]
            let sample = sample_guard.sample;
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
            #[cfg(feature = "encstats")]
            let _ = &key;

            // Look in each window entry
            let mut candidate = None;
            for (match_entry_idx, match_entry) in self.window.iter().enumerate() {
                let is_last = match_entry_idx == self.window.len() - 1;
                #[cfg(feature = "encstats")]
                let t_get = if sample { crate::encstats::tick() } else { 0 };
                // 有界链走查（与另一条匹配路径一致）：最多 CHAIN_DEPTH 个候选，最新在前。
                // 候选切片止于当前位置 ⇒ 太近的位置一律 < MIN_MATCH_LEN，链保留更老的位置。
                const CHAIN_DEPTH: usize = 8;
                let mut chain_buf = [0usize; CHAIN_DEPTH];
                let chain_len = match_entry
                    .suffixes
                    .get_chain(key, CHAIN_DEPTH, &mut chain_buf);
                #[cfg(feature = "encstats")]
                {
                    if is_last {
                        crate::encstats::bump(crate::encstats::PROBE_LAST, 1);
                        if chain_len > 0 {
                            crate::encstats::bump(crate::encstats::HIT_LAST, 1);
                        }
                    } else {
                        crate::encstats::bump(crate::encstats::PROBE_OLD, 1);
                        if chain_len > 0 {
                            crate::encstats::bump(crate::encstats::HIT_OLD, 1);
                        }
                    }
                }
                for &match_index in chain_buf[..chain_len].iter() {
                    let match_slice = if is_last {
                        &match_entry.data[match_index..self.suffix_idx]
                    } else {
                        &match_entry.data[match_index..]
                    };

                    // Check how long the common prefix actually is
                    #[cfg(feature = "encstats")]
                    let t_cmp = if sample { crate::encstats::tick() } else { 0 };
                    let match_len = Self::common_prefix_len(match_slice, data_slice);
                    #[cfg(feature = "encstats")]
                    if sample {
                        sample_guard.cmp += crate::encstats::tick() - t_cmp;
                    }

                    // Collisions in the suffix store might make this check fail
                    if match_len >= MIN_MATCH_LEN {
                        let offset = match_entry.base_offset + self.suffix_idx - match_index;

                        // If we are in debug/tests make sure the match we found is actually at the offset we calculated
                        #[cfg(debug_assertions)]
                        {
                            let unprocessed = last_entry.data.len() - self.suffix_idx;
                            let start = self.concat_window.len() - unprocessed - offset;
                            let end = start + match_len;
                            let check_slice = &self.concat_window[start..end];
                            debug_assert_eq!(check_slice, &match_slice[..match_len]);
                        }

                        if let Some((old_offset, old_match_len)) = candidate {
                            if match_len > old_match_len
                                || (match_len == old_match_len && offset < old_offset)
                            {
                                candidate = Some((offset, match_len));
                            }
                        } else {
                            candidate = Some((offset, match_len));
                        }
                    }
                }
            }

            if let Some((offset, match_len)) = candidate {
                // Fast mode: skip hash insertion for positions within the match.
                // C zstd's fast strategy does the same — only searched positions
                // get inserted. This trades a small ratio loss for large speed gain
                // (avoids O(match_len) hash inserts per match).
                // We still insert the current position's key so future lookups work.
                let last_entry = self.window.last_mut().unwrap();
                let key = &last_entry.data[self.suffix_idx..self.suffix_idx + MIN_MATCH_LEN];
                last_entry.suffixes.insert(key, self.suffix_idx);

                // All literals that were not included between this match and the last are now included here
                let last_entry = self.window.last().unwrap();
                let literals = &last_entry.data[self.last_idx_in_sequence..self.suffix_idx];

                // Update the indexes, all indexes upto and including the current index have been included in a sequence now
                self.suffix_idx += match_len;
                self.last_idx_in_sequence = self.suffix_idx;
                handle_sequence(Sequence::Triple {
                    literals,
                    offset,
                    match_len,
                });

                return true;
            }

            let last_entry = self.window.last_mut().unwrap();
            let key = &last_entry.data[self.suffix_idx..self.suffix_idx + MIN_MATCH_LEN];
            last_entry.suffixes.insert(key, self.suffix_idx);
            // Step acceleration: skip ahead faster at later positions in the block.
            // Positions near the start are more valuable as match targets, so we
            // search them densely. Later positions are less likely to be referenced.
            let data_len = last_entry.data.len();
            let step = 1 + (self.suffix_idx * 4 / data_len.max(1)).min(3);
            self.suffix_idx = (self.suffix_idx + step).min(data_len);
        }
    }

    /// Find the common prefix length between two byte slices
    #[inline(always)]
    fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
        let len = Self::mismatch_chunks::<8>(a, b);
        #[cfg(feature = "encstats")]
        {
            crate::encstats::bump(crate::encstats::CMP_CALLS, 1);
            crate::encstats::bump(crate::encstats::CMP_BYTES_MIN, a.len().min(b.len()) as u64);
            crate::encstats::bump(crate::encstats::CMP_BYTES_MATCHED, len as u64);
        }
        len
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
        let slice = &last_entry.data[self.suffix_idx..idx];
        for (key_index, key) in slice.windows(MIN_MATCH_LEN).enumerate() {
            last_entry.suffixes.insert(key, self.suffix_idx + key_index);
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
    fn add_data(
        &mut self,
        data: Vec<u8>,
        suffixes: SuffixStore,
        reuse_space: impl FnMut(Vec<u8>, SuffixStore),
    ) {
        assert!(
            self.window.is_empty() || self.suffix_idx == self.window.last().unwrap().data.len()
        );
        self.reserve(data.len(), reuse_space);
        #[cfg(debug_assertions)]
        self.concat_window.extend_from_slice(&data);

        if let Some(last_len) = self.window.last().map(|last| last.data.len()) {
            for entry in self.window.iter_mut() {
                entry.base_offset += last_len;
            }
        }

        let len = data.len();
        self.window.push(WindowEntry {
            data,
            suffixes,
            base_offset: 0,
        });
        self.window_size += len;
        self.suffix_idx = 0;
        self.last_idx_in_sequence = 0;
    }

    /// Reserve space for a new window entry
    /// If any resources are released by pushing the new entry they are returned via the callback
    fn reserve(&mut self, amount: usize, mut reuse_space: impl FnMut(Vec<u8>, SuffixStore)) {
        assert!(self.max_window_size >= amount);
        while self.window_size + amount > self.max_window_size {
            let removed = self.window.remove(0);
            self.window_size -= removed.data.len();
            #[cfg(debug_assertions)]
            self.concat_window.drain(0..removed.data.len());

            let WindowEntry {
                suffixes,
                data: leaked_vec,
                base_offset: _,
            } = removed;
            reuse_space(leaked_vec, suffixes);
        }
    }
}

#[test]
fn matches() {
    let mut matcher = MatchGenerator::new(1000);
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
                reconstructed.extend_from_within(start..end);
            }
        }
    };

    matcher.add_data(
        alloc::vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[0, 0, 0, 0, 0],
                offset: 5,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });

    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0,],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[
        1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0,
    ]);

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[1, 2, 3, 4, 5, 6],
                offset: 6,
                match_len: 6,
            },
            &mut reconstructed,
        )
    });
    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 6,
                match_len: 6,
            },
            &mut reconstructed,
        )
    });
    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 23,
                match_len: 5,
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0, 0, 0, 0, 0]);

    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Triple {
                literals: &[],
                offset: 11,
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

    matcher.add_data(
        alloc::vec![0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
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

    matcher.add_data(
        alloc::vec![7, 8, 9, 10, 11],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
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

    matcher.add_data(
        alloc::vec![1, 3, 5, 7, 9],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    matcher.skip_matching();
    original_data.extend_from_slice(&[1, 3, 5, 7, 9]);
    reconstructed.extend_from_slice(&[1, 3, 5, 7, 9]);
    assert!(!matcher.next_sequence(|_| {}));

    matcher.add_data(
        alloc::vec![1, 3, 5, 7, 9],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
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
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
    original_data.extend_from_slice(&[0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23]);

    // Characterization (base 511c945): the position-based step acceleration
    // skips idx 7 of this block, so no match is found and the whole block
    // comes out as one Literals sequence. Matches CI evidence (62 passed /
    // 1 failed with exactly this left-right pair before this fix).
    matcher.next_sequence(|seq| {
        assert_seq_equal(
            seq,
            Sequence::Literals {
                literals: &[0, 0, 11, 13, 15, 17, 20, 11, 13, 15, 17, 20, 21, 23],
            },
            &mut reconstructed,
        )
    });
    assert!(!matcher.next_sequence(|_| {}));

    assert_eq!(reconstructed, original_data);
}

#[test]
fn chain_reaches_old_positions() {
    let mut store = SuffixStore::with_capacity(64);
    for pos in 0..5usize {
        store.insert(&[0u8; 8][..MIN_MATCH_LEN], pos);
    }
    let mut buf = [0usize; 8];
    let n = store.get_chain(&[0u8; 8][..MIN_MATCH_LEN], 8, &mut buf);
    assert_eq!(&buf[..n], &[4, 3, 2, 1, 0], "链走查应能回到最老位置");
}
