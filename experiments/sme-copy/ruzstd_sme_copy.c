// ruzstd SME copy experiment (C side).
//   ruzstd_sme_copy(dst, src, n)       : streaming-SVE copy, aligns dst to 64 B.
//   ruzstd_sme_copy_naive(dst, src, n) : same loop without alignment handling.
//   ruzstd_sme_pair_cost(reps)         : smstart/smstop pair cost, CNTVCT ticks.
// Rust has no SME intrinsics at all (feature `stdarch_aarch64_sme` does not
// exist), so the wide copy has to live in C and be called over the C ABI.
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(__aarch64__)

static inline uint64_t read_cntvct(void) {
    uint64_t v; __asm__ volatile("isb; mrs %0, cntvct_el0" : "=r"(v)); return v;
}

// ---- instrumentation (env: RUZSTD_SME_STATS=1) -------------------------
// Per-call latency histogram + size histogram + total time inside the copy hook,
// so the hook's share of decode wall time can be stated. Also allows routing the
// same call sites to memcpy (RUZSTD_SME_BACKEND=memcpy) for an apples-to-apples
// in-situ comparison.
#define STAT_BUCKETS 4096          // ticks (52.1 ns each): 0..213 us
static uint32_t lat_hist[STAT_BUCKETS];
static uint32_t size_hist[24];     // log2 buckets of call size
static unsigned long long stat_calls, stat_bytes, stat_ticks;
static int stats_on, backend_sel;   // backend: 0=sme(production) 1=memcpy 2=naive 3=neon
static int stats_init(void) {
    static int done = 0;
    if (done) return stats_on;
    done = 1;
    const char *s = getenv("RUZSTD_SME_STATS");
    stats_on = (s && *s && *s != '0');
    const char *b = getenv("RUZSTD_SME_BACKEND");
    if (b) {
        if (!strcmp(b, "memcpy")) backend_sel = 1;
        else if (!strcmp(b, "naive")) backend_sel = 2;
        else if (!strcmp(b, "neon")) backend_sel = 3;
        if (backend_sel) stats_on = 1;   // dispatch needs the timing path
    }
    return stats_on;
}

static unsigned long long sme_calls, sme_bytes, sme_hist[8];
__attribute__((destructor)) static void sme_report(void) {
    if (sme_calls) {
        fprintf(stderr, "SME_COPY calls=%llu bytes=%llu hist<16K=%llu 16K=%llu 64K=%llu 256K=%llu 1M=%llu 4M+=%llu\n",
                sme_calls, sme_bytes, sme_hist[0], sme_hist[1], sme_hist[2], sme_hist[3], sme_hist[4], sme_hist[5]);
    }
    if (stat_calls) {
        fprintf(stderr, "COPY_STATS calls=%llu bytes=%llu ticks_total=%llu mean_ticks=%.1f\n",
                stat_calls, stat_bytes, stat_ticks, (double)stat_ticks / stat_calls);
        // size distribution
        fprintf(stderr, "COPY_SIZE ");
        for (int i = 0; i < 24; i++) if (size_hist[i]) fprintf(stderr, "2^%d:%u ", i, size_hist[i]);
        fprintf(stderr, "\nCOPY_LAT ");
        for (int i = 0; i < STAT_BUCKETS; i++) if (lat_hist[i]) fprintf(stderr, "%d:%u ", i, lat_hist[i]);
        fprintf(stderr, "\n");
    }
}
static inline void stat_begin(size_t n) {
    if (!stats_init()) return;
    stat_calls++; stat_bytes += n;
    unsigned b = 0; size_t v = n; while (v > 1) { v >>= 1; b++; }
    if (b < 24) size_hist[b]++;
}
static inline void stat_end(uint64_t t0) {
    if (!stats_on) return;
    uint64_t d = read_cntvct() - t0;
    stat_ticks += d;
    if (d >= STAT_BUCKETS) d = STAT_BUCKETS - 1;
    lat_hist[d]++;
}
static inline void sme_account(size_t n) {
    sme_calls++;
    sme_bytes += n;
    unsigned b = (n >= (4u << 20)) ? 5 : (n >= (1u << 20)) ? 4 : (n >= (256u << 10)) ? 3
               : (n >= (64u << 10)) ? 2 : (n >= (16u << 10)) ? 1 : 0;
    sme_hist[b]++;
}

// ACLE (Arm C Language Extensions): an inline asm that performs an
// SMSTART/SMSTOP pair invalidates all Z and P register state. Every register
// whose value might be changed therefore has to be named in the clobber list —
// otherwise the compiler may keep live values (including timing values) in
// registers that the streaming region silently destroys.
#define SME_CLOBBERS                                                                         \
    "z0","z1","z2","z3","z4","z5","z6","z7","z8","z9","z10","z11","z12","z13","z14","z15",   \
    "z16","z17","z18","z19","z20","z21","z22","z23","z24","z25","z26","z27","z28","z29",      \
    "z30","z31","p0","p1","p2","p3","p4","p5","p6","p7","p8","p9","p10","p11","p12","p13",    \
    "p14","p15","cc","memory"

// Bandwidth-oriented body: 4x64 B per iteration, then 1x64 B, then a predicated
// tail. All inside one streaming region (smstart/smstop).
#define SME_BODY(dstp, srcp, lenp)                                        \
    __asm__ volatile(                                                     \
        "smstart\n"                                                       \
        "ptrue   p0.b\n"                                                  \
        "1:\n"                                                            \
        "cmp     %2, #256\n"                                              \
        "b.lo    2f\n"                                                    \
        "ld1b    {z0.b}, p0/z, [%1]\n"                                    \
        "ld1b    {z1.b}, p0/z, [%1, #1, mul vl]\n"                        \
        "ld1b    {z2.b}, p0/z, [%1, #2, mul vl]\n"                        \
        "ld1b    {z3.b}, p0/z, [%1, #3, mul vl]\n"                        \
        "st1b    {z0.b}, p0, [%0]\n"                                      \
        "st1b    {z1.b}, p0, [%0, #1, mul vl]\n"                          \
        "st1b    {z2.b}, p0, [%0, #2, mul vl]\n"                          \
        "st1b    {z3.b}, p0, [%0, #3, mul vl]\n"                          \
        "add     %1, %1, #256\n"                                          \
        "add     %0, %0, #256\n"                                          \
        "sub     %2, %2, #256\n"                                          \
        "b       1b\n"                                                    \
        "2:\n"                                                            \
        "cmp     %2, #64\n"                                               \
        "b.lo    3f\n"                                                    \
        "ld1b    {z0.b}, p0/z, [%1]\n"                                    \
        "st1b    {z0.b}, p0, [%0]\n"                                      \
        "add     %1, %1, #64\n"                                           \
        "add     %0, %0, #64\n"                                           \
        "sub     %2, %2, #64\n"                                           \
        "b       2b\n"                                                    \
        "3:\n"                                                            \
        "cbz     %2, 4f\n"                                                \
        "whilelt p1.b, xzr, %2\n"                                         \
        "ld1b    {z0.b}, p1/z, [%1]\n"                                    \
        "st1b    {z0.b}, p1, [%0]\n"                                      \
        "4:\n"                                                            \
        "smstop\n"                                                        \
        : "+r"(dstp), "+r"(srcp), "+r"(lenp)                              \
        :                                                                 \
        : SME_CLOBBERS)

// NEON 16B-chunk reference copy (for third-party comparison in-situ).
__attribute__((target("arch=armv8-a"), noinline))
static void neon_ref_copy(unsigned char *d, const unsigned char *s, size_t n) {
    size_t left = n;
    if (!left) return;
    __asm__ volatile(
        "1:\ncmp %2,#64\nb.lo 2f\nldp q0,q1,[%1]\nldp q2,q3,[%1,#32]\nstp q0,q1,[%0]\nstp q2,q3,[%0,#32]\n"
        "add %1,%1,#64\nadd %0,%0,#64\nsub %2,%2,#64\nb 1b\n"
        "2:\ncbz %2,4f\n3:\nldrb w4,[%1]\nstrb w4,[%0]\nadd %1,%1,#1\nadd %0,%0,#1\nsubs %2,%2,#1\nb.ne 3b\n4:\n"
        : "+r"(d), "+r"(s), "+r"(left) : : "v0","v1","v2","v3","w4","cc","memory");
}

// Alignment-aware: peel until dst is 64 B aligned, bulk copy, predicated tail.
__attribute__((target("arch=armv9-a+sme2")))
void ruzstd_sme_copy(unsigned char *dst, const unsigned char *src, size_t n) {
    if (n == 0) return;
    if (stats_init()) {
        if (backend_sel == 1) {
            uint64_t t0 = read_cntvct(); stat_begin(n);
            memcpy(dst, src, n);
            stat_end(t0); return;
        }
        if (backend_sel == 2) {        // naive 1x64B kernel, no alignment peel
            uint64_t t0 = read_cntvct(); stat_begin(n);
            unsigned char *d = dst; const unsigned char *s = src; size_t left = n;
            __asm__ volatile("smstart\nptrue p0.b\n1:\ncmp %2,#64\nb.lo 2f\n"
                "ld1b {z0.b},p0/z,[%1]\nst1b {z0.b},p0,[%0]\nadd %1,%1,#64\nadd %0,%0,#64\nsub %2,%2,#64\nb 1b\n"
                "2:\ncbz %2,3f\nwhilelt p1.b,xzr,%2\nld1b {z0.b},p1/z,[%1]\nst1b {z0.b},p1,[%0]\n3:\nsmstop\n"
                : "+r"(d), "+r"(s), "+r"(left) : : SME_CLOBBERS);
            stat_end(t0); return;
        }
        if (backend_sel == 3) {        // NEON reference
            uint64_t t0 = read_cntvct(); stat_begin(n);
            neon_ref_copy(dst, src, n);
            stat_end(t0); return;
        }
    }
    sme_account(n);
    uint64_t t0 = 0;
    if (stats_on) { t0 = read_cntvct(); stat_begin(n); }
    size_t head = (size_t)((0u - (uintptr_t)dst) & 63u);
    if (head > n) head = n;
    // Head: at most 63 B; let the compiler pick (NEON/memcpy-style), not a byte loop.
    if (head) __builtin_memcpy(dst, src, head);
    unsigned char *d = dst + head;
    const unsigned char *s = src + head;
    size_t left = n - head;
    if (left) SME_BODY(d, s, left);
    if (stats_on) stat_end(t0);
}

// No alignment handling: a 64 B access at an odd offset straddles two lines.
__attribute__((target("arch=armv9-a+sme2"), noinline))
void ruzstd_sme_copy_naive(unsigned char *dst, const unsigned char *src, size_t n) {
    if (n == 0) return;
    sme_account(n);
    unsigned char *d = dst;
    const unsigned char *s = src;
    size_t left = n;
    SME_BODY(d, s, left);
}

// Prefetching variant: same streaming copy, but issue PRFM a few lines ahead so
// the load stream is not limited to the outstanding-loads of the SME path.
// (PRFM is a plain AArch64 hint instruction, legal in streaming mode.)
__attribute__((target("arch=armv9-a+sme2"), noinline))
void ruzstd_sme_copy_pf(unsigned char *dst, const unsigned char *src, size_t n) {
    if (n == 0) return;
    sme_account(n);
    size_t head = (size_t)((0u - (uintptr_t)dst) & 63u);
    if (head > n) head = n;
    if (head) __builtin_memcpy(dst, src, head);
    unsigned char *d = dst + head;
    const unsigned char *s = src + head;
    size_t left = n - head;
    if (!left) return;
    __asm__ volatile(
        "smstart\n"
        "ptrue   p0.b\n"
        "1:\n"
        "cmp     %2, #256\n"
        "b.lo    2f\n"
        "prfm    pldl1keep, [%1, #256]\n"
        "prfm    pldl1keep, [%1, #512]\n"
        "ld1b    {z0.b}, p0/z, [%1]\n"
        "ld1b    {z1.b}, p0/z, [%1, #1, mul vl]\n"
        "ld1b    {z2.b}, p0/z, [%1, #2, mul vl]\n"
        "ld1b    {z3.b}, p0/z, [%1, #3, mul vl]\n"
        "st1b    {z0.b}, p0, [%0]\n"
        "st1b    {z1.b}, p0, [%0, #1, mul vl]\n"
        "st1b    {z2.b}, p0, [%0, #2, mul vl]\n"
        "st1b    {z3.b}, p0, [%0, #3, mul vl]\n"
        "add     %1, %1, #256\n"
        "add     %0, %0, #256\n"
        "sub     %2, %2, #256\n"
        "b       1b\n"
        "2:\n"
        "cmp     %2, #64\n"
        "b.lo    3f\n"
        "prfm    pldl1keep, [%1, #256]\n"
        "ld1b    {z0.b}, p0/z, [%1]\n"
        "st1b    {z0.b}, p0, [%0]\n"
        "add     %1, %1, #64\n"
        "add     %0, %0, #64\n"
        "sub     %2, %2, #64\n"
        "b       2b\n"
        "3:\n"
        "cbz     %2, 4f\n"
        "whilelt p1.b, xzr, %2\n"
        "ld1b    {z0.b}, p1/z, [%1]\n"
        "st1b    {z0.b}, p1, [%0]\n"
        "4:\n"
        "smstop\n"
        : "+r"(d), "+r"(s), "+r"(left)
        :
        : SME_CLOBBERS);
}

uint64_t ruzstd_sme_pair_cost(int reps) {
    uint64_t a, b;
    __asm__ volatile("isb; mrs %0, cntvct_el0" : "=r"(a));
    for (int i = 0; i < reps; i++) __asm__ volatile("smstart\nsmstop" ::: SME_CLOBBERS);
    __asm__ volatile("isb; mrs %0, cntvct_el0" : "=r"(b));
    return (b - a) / (uint64_t)reps;
}

#else
void ruzstd_sme_copy(unsigned char *dst, const unsigned char *src, size_t n) {
    for (size_t i = 0; i < n; i++) dst[i] = src[i];
}
void ruzstd_sme_copy_naive(unsigned char *dst, const unsigned char *src, size_t n) {
    ruzstd_sme_copy(dst, src, n);
}
uint64_t ruzstd_sme_pair_cost(int reps) { (void)reps; return 0; }
#endif
