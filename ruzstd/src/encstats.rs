#![allow(dead_code)]
//! 编码侧工作项计数（`encstats` 特性门控）：只计数不打点，用来回答
//! "每输入字节做了多少次哈希插入/查找、产出了多少匹配"。

use core::sync::atomic::{AtomicU64, Ordering};

pub const INSERTS: usize = 0;
pub const PROBES: usize = 1;
pub const HITS: usize = 2;
pub const FSE_BUILDS: usize = 3;
pub const FSE_REUSED: usize = 4;
/// common_prefix_len 的调用次数
pub const CMP_CALLS: usize = 5;
/// 比较时触及的字节上限（min(两边长度) 之和）
pub const CMP_BYTES_MIN: usize = 6;
/// 实际匹配上的字节数之和
pub const CMP_BYTES_MATCHED: usize = 7;
/// 次新槽（第二个候选）真正参与比较的次数；若恒为 0 说明第二槽从未命中。
pub const SECOND_HITS: usize = 8;
/// 诊断：slot[1] 非空的探测次数（若为 0 说明 insert 从未把旧值挪到第二槽）
pub const SECOND_POPULATED: usize = 9;
/// 诊断：insert 时 slot[0] 已非空（即真正发生了"旧值→[1]"移位）的次数
pub const SHIFTED: usize = 10;
/// 命中来源：当前块（最后一个窗口条目）
pub const HIT_LAST: usize = 11;
/// 命中来源：更早的窗口条目（前几个块）
pub const HIT_OLD: usize = 12;
/// 探测次数：当前块 / 更早块
pub const PROBE_LAST: usize = 13;
pub const PROBE_OLD: usize = 14;
pub const N: usize = 15;

/// 块级相位计时累加器（tick 数，架构间不可比；只看同进程内占比）。
pub const PHASES: usize = 10;
pub static P: [AtomicU64; PHASES] = [const { AtomicU64::new(0) }; PHASES];

/// 廉价时间戳：aarch64 读 cntvct_el0，x86_64 读 rdtsc，其他架构返回 0。
#[inline(always)]
pub fn tick() -> u64 {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let v: u64;
        core::arch::asm!("mrs {}, cntvct_el0", out(reg) v);
        v
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        0
    }
}

#[inline(always)]
pub fn add_phase(i: usize, delta: u64) {
    P[i].fetch_add(delta, Ordering::Relaxed);
}

/// 读走并清零相位计时。
pub fn take_phases() -> [u64; PHASES] {
    let mut out = [0u64; PHASES];
    for i in 0..PHASES {
        out[i] = P[i].swap(0, Ordering::Relaxed);
    }
    out
}

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
