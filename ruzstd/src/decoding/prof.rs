//! Internal, temporary decode-time attribution for the CI perf harness
//! (feature `prof`). Not part of the public interface.
//!
//! Two kinds of probes:
//! - **block level**: one tick pair around the literals decode and one around the
//!   fused sequence loop, per block. 64 blocks per 8 MB, so the overhead is
//!   negligible and the split is trustworthy.
//! - **sampled phases**: inside the sequence loop only every 64th sequence is
//!   timed (FSE symbol+bits / literals copy / match copy), accumulated into
//!   locals and flushed once per call. Timing every sequence would cost
//!   20-30ns/sequence on a ~20ns/sequence loop and swamp the result.
//!
//! Ticks are raw architectural counters (`cntvct_el0` on aarch64, `rdtsc` on
//! x86_64) so a single tick is a couple of instructions; only *ratios* between
//! phases are meaningful, not absolute ns. The instrumented build is slower than
//! a clean one — never compare its wall time to a clean build.

use core::sync::atomic::{AtomicU64, Ordering};

/// Block-level: literals section decode (raw/RLE/huffman).
pub static LITERALS_TICKS: AtomicU64 = AtomicU64::new(0);
/// Block-level: the fused sequence decode+execute loop.
pub static SEQ_LOOP_TICKS: AtomicU64 = AtomicU64::new(0);
/// Regenerated literal bytes, per section type.
pub static LIT_RAW_BYTES: AtomicU64 = AtomicU64::new(0);
pub static LIT_RLE_BYTES: AtomicU64 = AtomicU64::new(0);
pub static LIT_HUF_BYTES: AtomicU64 = AtomicU64::new(0);
/// Huffman sections by stream count.
pub static HUF4_SECTIONS: AtomicU64 = AtomicU64::new(0);
pub static HUF1_SECTIONS: AtomicU64 = AtomicU64::new(0);
/// Blocks whose literals section was decoded, by type.
pub static LIT_BLOCKS_RAW: AtomicU64 = AtomicU64::new(0);
pub static LIT_BLOCKS_RLE: AtomicU64 = AtomicU64::new(0);
pub static LIT_BLOCKS_HUF: AtomicU64 = AtomicU64::new(0);

/// Sampled in-loop phases (every 64th sequence).
pub static FSE_TICKS: AtomicU64 = AtomicU64::new(0);
pub static LIT_COPY_TICKS: AtomicU64 = AtomicU64::new(0);
pub static MATCH_COPY_TICKS: AtomicU64 = AtomicU64::new(0);
pub static SAMPLES: AtomicU64 = AtomicU64::new(0);

/// Sequences decoded; blocks with a sequence section.
pub static SEQS: AtomicU64 = AtomicU64::new(0);
pub static BLOCKS: AtomicU64 = AtomicU64::new(0);

/// Read the architectural counter. Only differences are meaningful.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub fn tick() -> u64 {
    let t: u64;
    // SAFETY: reading cntvct_el0 is a side-effect-free register read, permitted
    // in user space when CNTKCTL_EL1.EL0VCTEN is set (the default on Linux).
    unsafe {
        core::arch::asm!("mrs {}, cntvct_el0", out(reg) t, options(nomem, nostack, preserves_flags));
    }
    t
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn tick() -> u64 {
    // SAFETY: `_rdtsc` has no side effects besides reading the counter; it is
    // available in user space on every x86_64 Linux runner.
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
#[inline(always)]
pub fn tick() -> u64 {
    0
}

/// Add a locally accumulated span to `counter` (one flush per block/call, not
/// per sequence).
#[inline(always)]
pub fn flush(counter: &AtomicU64, span: u64) {
    counter.fetch_add(span, Ordering::Relaxed);
}

#[inline(always)]
pub fn count(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

pub fn reset() {
    for c in [
        &LITERALS_TICKS,
        &SEQ_LOOP_TICKS,
        &LIT_RAW_BYTES,
        &LIT_RLE_BYTES,
        &LIT_HUF_BYTES,
        &HUF4_SECTIONS,
        &HUF1_SECTIONS,
        &LIT_BLOCKS_RAW,
        &LIT_BLOCKS_RLE,
        &LIT_BLOCKS_HUF,
        &FSE_TICKS,
        &LIT_COPY_TICKS,
        &MATCH_COPY_TICKS,
        &SAMPLES,
        &SEQS,
        &BLOCKS,
    ] {
        c.store(0, Ordering::Relaxed);
    }
}
