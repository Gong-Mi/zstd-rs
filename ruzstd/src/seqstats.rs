//! 每序列工作项计数（`seqstats` 特性门控）。
//!
//! 只计数不打点：性能判定走 CI A/B，这里要回答的是"每序列到底调用了多少次、
//! 搬了多少字节"，用来和 CI 的相位占比相乘得到 ns/调用、ns/字节。

use core::sync::atomic::{AtomicU64, Ordering};

pub const SEQ: usize = 0;
pub const TRIPLE: usize = 1;
pub const UPD: usize = 2;
pub const BITSREM: usize = 3;
pub const LIT_CALLS: usize = 4;
pub const LIT_BYTES: usize = 5;
pub const MATCH_CALLS: usize = 6;
pub const MATCH_BYTES: usize = 7;
pub const PRODUCED: usize = 8;
pub const DRAINED: usize = 9;
pub const DRAIN_CALLS: usize = 10;
pub const N: usize = 11;

pub static C: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

#[inline(always)]
pub fn bump(i: usize, by: u64) {
    C[i].fetch_add(by, Ordering::Relaxed);
}

/// 读走并清零。
pub fn take() -> [u64; N] {
    let mut out = [0u64; N];
    for i in 0..N {
        out[i] = C[i].swap(0, Ordering::Relaxed);
    }
    out
}
