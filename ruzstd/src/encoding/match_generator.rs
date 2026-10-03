//! Matching algorithm used find repeated parts in the original data
//!
//! The Zstd format relies on finden repeated sequences of data and compressing these sequences as instructions to the decoder.
//! A sequence basically tells the decoder "Go back X bytes and copy Y bytes to the end of your decode buffer".
//!
//! The task here is to efficiently find matches in the already encoded data for the current suffix of the not yet encoded data.

use alloc::vec::Vec;

use super::CompressionLevel;
use super::Matcher;
use super::Sequence;
use core::convert::TryInto;

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
            suffixes.slots.resize(suffixes.slots.capacity(), 0u32);
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
                suffixes.slots.resize(suffixes.slots.capacity(), 0u32);
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
    // 表项用 u32（与 C 的 hashTable 一致）：0 = 空，否则位置+1。
    // 用 Option<NonZeroUsize> 时每项 8 字节，表一样大却占两倍内存 ——
    // 128K 槽的表是 1MB vs 512KB，直接决定有多少次访问越过 L2。
    // 单槽保留每 key 最近位置；更早的位置由 links 链保留（见下）。
    slots: Vec<u32>,
    /// 链：`links[pos]` = 同一 key 的上一个位置（+1 编码，0 表示链尾）。
    /// 只靠槽位覆盖会丢"足够远"的候选（候选切片止于当前位置，太近的位置一律 < MIN_MATCH_LEN），
    /// 链把更老的位置保留下来，按有界步数走查取"足够远且更长"的候选。
    links: Vec<u32>,
    len_log: u32,
}

impl SuffixStore {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            slots: alloc::vec![0u32; capacity],
            links: Vec::new(),
            len_log: capacity.ilog2(),
        }
    }

    /// 与 `insert` 等价，但 key 由调用方算好（同一位置探针+插入只算一次哈希）
    #[inline(always)]
    fn insert_hashed(&mut self, key: usize, idx: usize) {
        #[cfg(feature = "encstats")]
        crate::encstats::bump(crate::encstats::INSERTS, 1);
        // 链：同 key 已有上一个位置才写 links（random 类无重复 key 的语料
        // 因此零链簿记，插入路径与单槽基线同成本）；候选切片止于当前位置
        // ⇒ 只有"足够远"的位置才给得出合法匹配，链把更老的位置留住。
        let prev = self.slots[key];
        if prev != 0 {
            if idx >= self.links.len() {
                self.links.resize(idx + 1, 0);
            }
            // links 与 slots 同为"位置+1"编码，直接套用
            self.links[idx] = prev;
        }
        self.slots[key] = idx as u32 + 1;
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
    fn insert(&mut self, suffix: &[u8], idx: usize) {
        let key = self.key_hash(suffix);
        self.insert_hashed(key, idx);
    }

    /// 与 `get` 等价，但 key 由调用方算好
    #[inline(always)]
    fn get_hashed(&self, key: usize) -> Option<usize> {
        #[cfg(feature = "encstats")]
        crate::encstats::bump(crate::encstats::PROBES, 1);
        let raw = self.slots[key];
        let hit = if raw == 0 {
            None
        } else {
            Some(raw as usize - 1)
        };
        #[cfg(feature = "encstats")]
        if hit.is_some() {
            crate::encstats::bump(crate::encstats::HITS, 1);
        }
        hit
    }

    /// 仅供单测与 API 对称性使用；库内热路径走 `get_hashed`（key 已算好）
    #[allow(dead_code)]
    #[inline(always)]
    fn get(&self, suffix: &[u8]) -> Option<usize> {
        self.get_hashed(self.key_hash(suffix))
    }

    /// 单乘法哈希（对齐 C 的 ZSTD_hashPtr）：读 8 字节 → 一次 64 位乘法 → 取高位。
    ///
    /// 原实现是 5 次 64 位乘法 + 5 次移位 + 4 次异或（suffix[0..5] 各混一次），
    /// 而 `key_hash` 是每个走访位置都要跑的路径；C 的 fast 只用 1 次乘法 + 移位。
    ///
    /// 这是**算法改动**（哈希分布变化 ⇒ 候选集合变化 ⇒ ratio 可能变），因此
    /// 必须与时间一起由 CI 同 run 判定，不能按"等价改动"处理。
    #[inline(always)]
    fn key_hash(&self, suffix: &[u8]) -> usize {
        // 需要 5 字节的区分度（MIN_MATCH_LEN=5）：尾部不足 8 字节时零填充
        let v = if suffix.len() >= 8 {
            u64::from_le_bytes(suffix[..8].try_into().unwrap())
        } else {
            let mut buf = [0u8; 8];
            let n = suffix.len().min(8);
            buf[..n].copy_from_slice(&suffix[..n]);
            u64::from_le_bytes(buf)
        };
        const PRIME: u64 = 0x9E37_79B9_7F4A_7C15;
        let mixed = v.wrapping_mul(PRIME);
        // `x >> (64 - len_log)` 恒 < 2^len_log <= slots.len()（len_log = capacity.ilog2()），
        // 所以原先这里的 `% self.slots.len()` 是恒等变换：LLVM 看不到 ilog2 与 len 的关系，
        // 于是每个位置都生成一次 64 位取模（aarch64: udiv + msub + 除零检查；cortex-x4 静态
        // 模型里单条 udiv 占 20 个吞吐周期），而结果永远不变。删掉它逐字节等价。
        (mixed >> (64 - self.len_log)) as usize
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
    miss_count: usize,
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
            miss_count: 0,
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
            // 每个位置只算一次哈希，探针与插入共用（原先 get/insert 各算一次，各 5 次乘法）
            let key_hash = self
                .window
                .last()
                .unwrap()
                .suffixes
                .key_hash(&data_slice[..MIN_MATCH_LEN]);

            // Look in each window entry
            let mut candidate = None;
            for (match_entry_idx, match_entry) in self.window.iter().enumerate() {
                let is_last = match_entry_idx == self.window.len() - 1;
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
                const CHAIN_CMP_MAX: usize = 1;
                const LONG_HIT_CUT: usize = 8;
                const CHAIN_WALK_MAX: usize = 32;
                let mut walked = 0usize;
                let mut cmps = 0usize;
                // 首次比较命中长度：条件预算用它决定要不要花第二次比较
                let mut last_hit_len = 0usize;
                let mut cur = match_entry.suffixes.get_hashed(key_hash);
                while let Some(match_index) = cur {
                    let match_slice = if is_last {
                        &match_entry.data[match_index..self.suffix_idx]
                    } else {
                        &match_entry.data[match_index..]
                    };

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
                last_entry.suffixes.insert_hashed(key_hash, self.suffix_idx);

                // 扣除倒退吸纳的字面量
                let last_entry = self.window.last().unwrap();
                let match_start = self.suffix_idx - back;
                let literals = &last_entry.data[self.last_idx_in_sequence..match_start];

                // Update the indexes, all indexes upto and including the current index have been included in a sequence now
                self.miss_count = 0;
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
            last_entry.suffixes.insert_hashed(key_hash, self.suffix_idx);
            // Step acceleration: skip ahead faster at later positions in the block.
            // Positions near the start are more valuable as match targets, so we
            // search them densely. Later positions are less likely to be referenced.
            self.miss_count += 1;
            let data_len = last_entry.data.len();
            // 步进只由"连续未命中"驱动（miss_count 在命中处清零），去掉无条件的
            // 位置斜坡：C 的 fast 步进以 `step = stepSize` 在 _start 复位、命中分支
            // 尾部 `goto _start`（zstd_fast.c:249 / 422），没有"越靠后跳得越多"这一项。
            // 成长速率取 shift5/cap32：本地 2 reps 实测 ratio 与 shift6/cap16 同档
            // （bin-like 1.552 vs 1.561、binary 1.063 vs 1.069）而耗时低 15~27%。
            // 实测（4MB 单次压缩，确定性 ratio）：位置斜坡是纯损失——
            //   bin-like 1.516→1.635、src-like 4.871→5.037、repo-sources 3.213→3.361，
            //   text/binary-medium 不变；代价只在 random 类语料（另有不可压缩块早退兜底）。
            let step = 1 + (self.miss_count >> 5).min(32);
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
    ///
    /// 实现说明：原实现用 `chunks_exact(N)` + 迭代器 `take_while` 逐块比较，编译器
    /// 生成的是逐字节循环；这里改成**字对字**比较（读 N 字节 → XOR → trailing_zeros），
    /// 与 C 的 `ZSTD_count`（MEM_readST + XOR + ctz）同路数。语义与截断口径完全不变
    /// （返回 min(xs.len(), ys.len()) 内的公共前缀长度），故 ratio 必须逐字节相同。
    fn mismatch_chunks<const N: usize>(xs: &[u8], ys: &[u8]) -> usize {
        debug_assert!(N == 8 || N == 4 || N == 2 || N == 1);
        let n = xs.len().min(ys.len());
        let mut i = 0;
        // 整块：一次读 N 字节做 XOR，非零即用 trailing_zeros 定位首个不同字节
        if N == 8 {
            while i + 8 <= n {
                let a = u64::from_le_bytes(xs[i..i + 8].try_into().unwrap());
                let b = u64::from_le_bytes(ys[i..i + 8].try_into().unwrap());
                let x = a ^ b;
                if x != 0 {
                    return i + (x.trailing_zeros() as usize >> 3);
                }
                i += 8;
            }
        } else if N == 4 {
            while i + 4 <= n {
                let a = u32::from_le_bytes(xs[i..i + 4].try_into().unwrap());
                let b = u32::from_le_bytes(ys[i..i + 4].try_into().unwrap());
                let x = a ^ b;
                if x != 0 {
                    return i + (x.trailing_zeros() as usize >> 3);
                }
                i += 4;
            }
        } else {
            while i + N <= n {
                if xs[i..i + N] != ys[i..i + N] {
                    break;
                }
                i += N;
            }
        }
        // 尾部不足一块
        while i < n && xs[i] == ys[i] {
            i += 1;
        }
        i
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
        // 链候选使这里多出一个 **等长且更近** 的候选（offset 6，指向本块内前一段
        // `1..6`）；按本实现既有的取用规则（等长取更近 offset）应取 6 而非基线
        // 单槽给的 12。压缩比与正确性均不受损（等长替换，流仍逐字节重建）。
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
        // 同上：等长候选更多，按"等长取更近"应取 23（更靠近当前位置）而非基线的 28。
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
        // 链候选带来的第三个表征变化：本块内 offset 11 的等长候选（1..6 段）胜过
        // 基线的 23（跨块候选）。取用规则不变（等长取更近），仍是等长替换。
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

    // 表征更新（步进改为纯 miss 驱动后）：位置斜坡不再跳过 idx 7，本块找到
    // offset 5 / match_len 5 的匹配（此前被斜坡跳过 ⇒ 整块退化成一条 Literals）。
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
    // 尾部不足 MIN_MATCH_LEN 的 2 字节按字面量收尾
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
