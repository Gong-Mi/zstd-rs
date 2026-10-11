//! 编码侧工作项计数（`encstats` 特性门控）：只计数不打点，用来回答
//! "每输入字节做了多少次哈希插入/查找、产出了多少匹配"。

use core::sync::atomic::{AtomicU64, Ordering};

pub const INSERTS: usize = 0;
pub const PROBES: usize = 1;
pub const HITS: usize = 2;
/// 原始哈希计算次数：同一位置探针+插入共用一次，故应 < PROBES + INSERTS。
pub const HASHES: usize = 3;
/// 观察到的序列总数（只统计带 offset 的 `Triple`，不含块尾纯 literal）。
pub const SEQ_TOTAL: usize = 4;
/// 其中 offset 命中当前 rep 历史（rep1/rep2/rep3）的序列数——即 C 的 repcode 可省下的序列。
pub const SEQ_REP_HITS: usize = 5;
/// 命中序列当前仍按真实 offset 编码、多付的 offset 附加位总和（bit）。
/// 上界口径：假设改用 repcode 后附加位为 0，且不改变 FSE 符号本身的开销。
pub const SEQ_REP_EXTRA_BITS: usize = 6;
/// 起始 rep 历史（zstd 语义：rep1/rep2/rep3 = 1/4/8）。
pub const REP_INIT: [u32; 3] = [1, 4, 8];
/// 走的压缩块数（`compress_block` 调用次数）。
pub const BLK_NUM: usize = 7;
/// ll/ml/of 三类 FSE 表模式里选 `Encoded`（重发表）的次数。
pub const MODE_ENCODED: usize = 8;
/// 选 `Predefined`（预定义表）的次数。
pub const MODE_PREDEFINED: usize = 9;
/// 表区占用（模式字节 + 三张表的 write_table）bit 数。
pub const BLK_TABLE_BITS: usize = 10;
/// literals 分区占用 bit 数（含 literals section header）。
pub const BLK_LITERALS_BITS: usize = 11;
/// sequences 分区占用 bit 数（含序列数头）。
pub const BLK_SEQ_BITS: usize = 12;
/// 因压缩无收益而退回 raw 的块数。
pub const BLK_RAW: usize = 13;
pub const N: usize = 14;

/// 诊断用：次选候选入围规则开关（0 = 现行规则）。
///
/// 现行规则 = `match_len > old_match_len || (match_len == old_match_len && offset < old_offset)`
/// ——任何"严格更长"或"等长且偏移更小"的候选都会顶掉当前选择。链走查最多比
/// `CHAIN_CMP_MAX` 次，因此窗口/历史一多，就容易用"更长但偏移更大"的匹配替换掉
/// 更划算的匹配（offset 附加位变多、局部性变差）。本开关让例程在不改默认行为的前提下
/// 逐个量各条加强规则的压缩率后果。
pub static CAND_RULE: core::sync::atomic::AtomicU8 = const { core::sync::atomic::AtomicU8::new(0) };

pub fn set_cand_rule(rule: u8) {
    CAND_RULE.store(rule, Ordering::Relaxed);
}

/// 次选入围判定（诊断）：返回是否接受新候选。
///
/// 规则表：
/// - 0 现行：更长即接受；等长时取更小 offset
/// - 1 严格更长：等长不接受
/// - 2 至少长 2 字节
/// - 3 至少长 4 字节
/// - 4 长度增益须盖过 offset 附加位代价（Δlen×1bit ≥ Δlog2(offset)）
/// - 5 规则 4 且等长时仅在 offset 更小时接受
pub fn cand_rule_accept(match_len: usize, offset: usize, old_len: usize, old_offset: usize) -> bool {
    let rule = CAND_RULE.load(Ordering::Relaxed);
    match rule {
        1 => match_len > old_len,
        2 => match_len > old_len + 1,
        3 => match_len > old_len + 3,
        4 | 5 => {
            let log2 = |v: usize| (usize::BITS - 1 - (v | 1).leading_zeros()) as i64;
            let extra_bits = log2(offset) - log2(old_offset);
            let len_gain = match_len as i64 - old_len as i64;
            if len_gain > extra_bits {
                true
            } else if len_gain == 0 {
                rule == 5 && offset < old_offset
            } else {
                false
            }
        }
        _ => match_len > old_len || (match_len == old_len && offset < old_offset),
    }
}

pub static C: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

#[inline(always)]
pub fn bump(i: usize, by: u64) {
    C[i].fetch_add(by, Ordering::Relaxed);
}

/// 逐序列推进的 rep 历史（zstd 语义：offset 1/2/3 表示复用前三个偏移）。
///
/// 仅用于计数/诊断：不参与任何编码决策，也不改变输出字节。
/// 起始历史为 `{1, 4, 8}`；命中时按 zstd 的 `ZSTD_updateRep` 规则推进。
#[derive(Clone, Copy)]
pub struct RepHistory {
    rep: [u32; 3],
}

impl Default for RepHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl RepHistory {
    pub const fn new() -> Self {
        Self { rep: REP_INIT }
    }

    pub fn reset(&mut self) {
        self.rep = REP_INIT;
    }

    /// 观察一条真实 offset（>= 1）的序列；命中 rep1/2/3 时返回类号，否则 0。
    pub fn observe(&mut self, offset: u32) -> u8 {
        let (kind, rep_code_bits): (u8, u64) = if offset == self.rep[0] {
            (1, 0)
        } else if offset == self.rep[1] {
            (2, 1)
        } else if offset == self.rep[2] {
            (3, 1)
        } else {
            (0, 0)
        };

        // 现有编码器把真实 offset 写成 `of = offset + 3`，符号值 `ilog2(of)` 即该序列
        // 实际写出的 offset 附加位数（见 `blocks/compressed.rs::encode_offset`）。
        let of = offset + 3;
        let offset_bits = (31 - (of | 1).leading_zeros()) as u64;

        bump(SEQ_TOTAL, 1);
        if kind == 0 {
            let r = self.rep;
            self.rep = [offset, r[0], r[1]];
            return 0;
        }

        bump(SEQ_REP_HITS, 1);
        bump(
            SEQ_REP_EXTRA_BITS,
            offset_bits.saturating_sub(rep_code_bits),
        );
        match kind {
            1 => {}
            2 => self.rep.swap(0, 1),
            _ => {
                let r = self.rep;
                self.rep = [r[2], r[0], r[1]];
            }
        }
        kind
    }
}

/// 诊断用：窗口条目走查顺序开关（0 = 现行：最老条目先走；1 = 最新条目先走）。
///
/// 现行代码 `self.window.iter()` 从最老条目开始，`candidate` 的首次接受者因此来自最老
/// 条目；再按"更长即替换"逐条目更新。窗口一大，远端更长匹配更容易顶掉近端匹配
/// （offset 附加位变多、局部性变差），与"窗口变大反而更差"的实测一致。本开关量这个顺序。
pub static WINDOW_ORDER: core::sync::atomic::AtomicU8 = const { core::sync::atomic::AtomicU8::new(0) };

pub fn set_window_order_newest_first(newest_first: bool) {
    WINDOW_ORDER.store(newest_first as u8, Ordering::Relaxed);
}

pub fn window_order_newest_first() -> bool {
    WINDOW_ORDER.load(Ordering::Relaxed) != 0
}

/// 读走并清零。
pub fn take() -> [u64; N] {
    let mut out = [0u64; N];
    for i in 0..N {
        out[i] = C[i].swap(0, Ordering::Relaxed);
    }
    out
}
