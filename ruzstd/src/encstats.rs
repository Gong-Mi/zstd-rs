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
pub const N: usize = 7;

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

/// 读走并清零。
pub fn take() -> [u64; N] {
    let mut out = [0u64; N];
    for i in 0..N {
        out[i] = C[i].swap(0, Ordering::Relaxed);
    }
    out
}
