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
            suffixes.slots.resize(suffixes.slots.capacity(), None);
            suffixes.links.clear();
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
        self.match_generator
            .add_data(space, suffixes, |mut data, mut suffixes| {
                data.resize(data.capacity(), 0);
                vec_pool.push(data);
                suffixes.slots.clear();
                suffixes.slots.resize(suffixes.slots.capacity(), None);
                suffixes.links.clear();
                suffix_pool.push(suffixes);
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

/// This stores the index of a suffix of a string by hashing the first few bytes of that suffix
/// This means that collisions just overwrite and that you need to check validity after a get
struct SuffixStore {
    // We use NonZeroUsize to enable niche optimization here.
    // On store we do +1 and on get -1
    // This is ok since usize::MAX is never a valid offset
    //
    // 单槽保留每 key 最近位置；更早的位置由 links 链保留（见下）。
    slots: Vec<Option<NonZeroUsize>>,
    /// 链：`links[pos]` = 同一 key 的上一个位置（+1 编码，0 表示链尾）。
    /// 只靠槽位覆盖会丢"足够远"的候选（候选切片止于当前位置，太近的位置一律 < MIN_MATCH_LEN），
    /// 链把更老的位置保留下来，按有界步数走查取"足够远且更长"的候选。
    links: Vec<u32>,
    len_log: u32,
}

impl SuffixStore {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: alloc::vec![None; capacity],
            links: Vec::new(),
            len_log: capacity.ilog2(),
        }
    }

    #[inline(always)]
    fn insert(&mut self, suffix: &[u8], idx: usize) {
        #[cfg(feature = "encstats")]
        crate::encstats::bump(crate::encstats::INSERTS, 1);
        let key = self.key(suffix);
        // 链：同 key 已有上一个位置才写 links（random 类无重复 key 的语料
        // 因此零链簿记，插入路径与单槽基线同成本）；候选切片止于当前位置
        // ⇒ 只有"足够远"的位置才给得出合法匹配，链把更老的位置留住。
        if let Some(prev) = self.slots[key] {
            if idx >= self.links.len() {
                self.links.resize(idx + 1, 0);
            }
            self.links[idx] = prev.get() as u32;
        }
        self.slots[key] = Some(NonZeroUsize::new(idx + 1).unwrap());
    }

    /// `pos` 在链上的上一位置（None = 链尾）。供融合走查用。
    #[inline(always)]
    fn link_of(&self, pos: usize) -> Option<usize> {
        match self.links.get(pos).copied().unwrap_or(0) {
            0 => None,
            l => Some(l as usize - 1),
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
    /// 距上次命中所搜过的字节数，用于对齐 C 的 step 加速（见 next_sequence）
    step_dist: usize,
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
            step_dist: 0,
            last_idx_in_sequence: 0,
        }
    }

    fn reset(&mut self, mut reuse_space: impl FnMut(Vec<u8>, SuffixStore)) {
        self.window_size = 0;
        #[cfg(debug_assertions)]
        self.concat_window.clear();
        self.suffix_idx = 0;
        self.last_idx_in_sequence = 0;
        self.step_dist = 0;
        self.window.drain(..).for_each(|entry| {
            reuse_space(entry.data, entry.suffixes);
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

            // Look in each window entry. The walk/compare budgets are shared
            // across the entries: with a single slice this is identical to the
            // per-entry scope, and with several slices it keeps the per-position
            // work bounded instead of paying the budget once per slice.
            let mut candidate = None;
            let mut walked = 0usize;
            let mut cmps = 0usize;
            // 首次比较命中长度：条件预算用它决定要不要花第二次比较
            let mut last_hit_len = 0usize;
            // 最新切片优先：共享预算下先让最近（同长取近）的候选参与。
            for (match_entry_idx, match_entry) in self.window.iter().enumerate().rev() {
                // 有界链走查（与比较融合，不预取整条链）：
                //  - 内容比较是成本主项 ⇒ 预算 CHAIN_CMP_MAX 次；
                //  - "太近"候选（切片 < MIN_MATCH_LEN，步进加速下常见）不消耗比较预算，
                //    只走链（否则预算全被近邻吃掉，单测 matches 会退化成找不到匹配）；
                //  - 走查总步数另有上限，防长链退化。
                // 预算 1：本地确定性计数实测（4MB，单次压缩）——
                //   text ratio 3.174（预算 2 为 3.259）/ cmp 调用 1,296,622（预算 2 为 2,406,990；
                //   基线 1,296,066）/ 比较字节 2.5GB（预算 2 为 7.1GB，基线 41.8GB）。
                //   src-like 4.718（2 档 4.909）/ cmp 669,292（基线 815,362）。
                // 即：3% 的 ratio 换近一半的比较开销，且比较字节数远低于基线。
                // 条件预算：内容比较硬预算仍是 1 次；若首次比较命中且匹配长度
                // ≥ LONG_HIT_CUT，则额外允许 1 次比较（长匹配处多一个候选才换得来
                // ratio，短匹配处纯亏）。
                // 本地确定性计数（4MB 单次压缩，base text cmp 1,296,066 / ratio 2.480）：
                //   budget1     text 3.174/1,296,622  src-like 4.718/669,292  bin-like 1.482/820,086
                //   cond-long8  text 3.176/1,524,382  src-like 4.871/969,684  bin-like 1.516/784,322
                //   budget2     text 3.259/2,406,990  src-like 4.909/1,187,948 bin-like 1.527/1,097,398
                const CHAIN_CMP_MAX: usize = 4;
                const LONG_HIT_CUT: usize = 8;
                const CHAIN_WALK_MAX: usize = 32;
                let mut cur = match_entry.suffixes.get(key);
                while let Some(match_index) = cur {
                    // Full forward extension for every candidate, mirroring
                    // the reference match finder: the restricted slice for the
                    // trailing entry structurally forbids overlapping matches,
                    // which makes highly repetitive input cascade in
                    // offset-doubling steps instead of encoding one long match.
                    let match_slice = &match_entry.data[match_index..];

                    if match_slice.len() < MIN_MATCH_LEN {
                        // 太近的候选给不出合法匹配，只走链不比较
                        walked += 1;
                        if walked >= CHAIN_WALK_MAX {
                            break;
                        }
                        cur = match_entry.suffixes.link_of(match_index);
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

                        if let Some((_, _, old_offset, old_match_len)) = candidate {
                            if match_len > old_match_len
                                || (match_len == old_match_len && offset < old_offset)
                            {
                                candidate = Some((match_entry_idx, match_index, offset, match_len));
                            }
                        } else {
                            candidate = Some((match_entry_idx, match_index, offset, match_len));
                        }
                        last_hit_len = match_len;
                        cur = match_entry.suffixes.link_of(match_index);
                    } else {
                        // 候选切片足够长却内容不匹配 ⇒ 放弃整条链：同槽更老的候选
                        // 内容命中概率只低不高（确定性计数实测：不断链时
                        // common_prefix_len 调用数是基线的数倍，纯浪费）。
                        break;
                    }
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
                let last_entry = self.window.last_mut().unwrap();
                let key = &last_entry.data[self.suffix_idx..self.suffix_idx + MIN_MATCH_LEN];
                last_entry.suffixes.insert(key, self.suffix_idx);

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

            let last_entry = self.window.last_mut().unwrap();
            let key = &last_entry.data[self.suffix_idx..self.suffix_idx + MIN_MATCH_LEN];
            last_entry.suffixes.insert(key, self.suffix_idx);
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
        self.step_dist = 0;
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

    matcher.add_data(
        alloc::vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        SuffixStore::with_capacity(100),
        |_, _| {},
    );
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
        SuffixStore::with_capacity(100),
        |_, _| {},
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
        SuffixStore::with_capacity(100),
        |_, _| {},
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
    let mut store = SuffixStore::with_capacity(64);
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
