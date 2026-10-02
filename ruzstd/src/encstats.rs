//! 编码侧工作项计数（`encstats` 特性门控）：只计数不打点，用来回答
//! "每输入字节做了多少次哈希插入/查找、产出了多少匹配"。

use core::sync::atomic::{AtomicU64, Ordering};

pub const INSERTS: usize = 0;
pub const PROBES: usize = 1;
pub const HITS: usize = 2;
pub const N: usize = 3;

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
