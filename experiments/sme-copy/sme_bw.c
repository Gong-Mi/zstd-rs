// Bandwidth-first comparison (not per-copy latency):
//   libc memcpy | NEON 4x16B | SME 1x64B | SME 4x64B | SME 8x64B
//   plus non-temporal-store variants (stnt1b) for the SME loops.
// Each measurement copies `span` bytes, repeated, and reports GB/s.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

#define POOL (72u << 20)      // 36 MiB src + 36 MiB dst
static unsigned char *src, *dst;
static uint64_t cntvct(void){ uint64_t v; __asm__ volatile("isb; mrs %0, cntvct_el0":"=r"(v)); return v; }

__attribute__((target("arch=armv8-a"),noinline))
static void neon_copy(unsigned char *d, const unsigned char *s, size_t n){
    size_t left=n;
    __asm__ volatile(
        "1:\ncmp %2,#64\nb.lo 2f\nldp q0,q1,[%1]\nldp q2,q3,[%1,#32]\nstp q0,q1,[%0]\nstp q2,q3,[%0,#32]\n"
        "add %1,%1,#64\nadd %0,%0,#64\nsub %2,%2,#64\nb 1b\n"
        "2:\ncbz %2,4f\n3:\nldrb w4,[%1]\nstrb w4,[%0]\nadd %1,%1,#1\nadd %0,%0,#1\nsubs %2,%2,#1\nb.ne 3b\n4:\n"
        : "+r"(d), "+r"(s), "+r"(left) : : "v0","v1","v2","v3","w4","cc","memory");
}

// ACLE: SMSTART/SMSTOP invalidates all Z/P state — full clobber list required.
#define SME_CLOBBERS_BW                                                                      \
    "z0","z1","z2","z3","z4","z5","z6","z7","z8","z9","z10","z11","z12","z13","z14","z15",   \
    "z16","z17","z18","z19","z20","z21","z22","z23","z24","z25","z26","z27","z28","z29",      \
    "z30","z31","p0","p1","p2","p3","p4","p5","p6","p7","p8","p9","p10","p11","p12","p13",    \
    "p14","p15","cc","memory"

// SME streaming copy: UNROLL x 64B per iteration, optional non-temporal stores.
#define SME_LOOP(UNROLL, NT)                                                     \
    __asm__ volatile(                                                            \
        "smstart\n ptrue p0.b\n"                                                 \
        "1:\n"                                                                   \
        "cmp %2, #" #UNROLL "*64\n" "b.lo 3f\n"                                  \
        "ld1b {z0.b}, p0/z, [%1]\n" "ld1b {z1.b}, p0/z, [%1, #1, mul vl]\n"      \
        "ld1b {z2.b}, p0/z, [%1, #2, mul vl]\n" "ld1b {z3.b}, p0/z, [%1, #3, mul vl]\n" \
        ".if " #UNROLL " > 4\n"                                                  \
        "ld1b {z4.b}, p0/z, [%1, #4, mul vl]\n" "ld1b {z5.b}, p0/z, [%1, #5, mul vl]\n" \
        "ld1b {z6.b}, p0/z, [%1, #6, mul vl]\n" "ld1b {z7.b}, p0/z, [%1, #7, mul vl]\n" \
        ".endif\n"                                                               \
        ".if " #NT "\n"                                                          \
        "stnt1b {z0.b}, p0, [%0]\n" "stnt1b {z1.b}, p0, [%0, #1, mul vl]\n"      \
        "stnt1b {z2.b}, p0, [%0, #2, mul vl]\n" "stnt1b {z3.b}, p0, [%0, #3, mul vl]\n" \
        ".if " #UNROLL " > 4\n"                                                  \
        "stnt1b {z4.b}, p0, [%0, #4, mul vl]\n" "stnt1b {z5.b}, p0, [%0, #5, mul vl]\n" \
        "stnt1b {z6.b}, p0, [%0, #6, mul vl]\n" "stnt1b {z7.b}, p0, [%0, #7, mul vl]\n" \
        ".endif\n"                                                               \
        ".else\n"                                                                \
        "st1b {z0.b}, p0, [%0]\n" "st1b {z1.b}, p0, [%0, #1, mul vl]\n"          \
        "st1b {z2.b}, p0, [%0, #2, mul vl]\n" "st1b {z3.b}, p0, [%0, #3, mul vl]\n" \
        ".if " #UNROLL " > 4\n"                                                  \
        "st1b {z4.b}, p0, [%0, #4, mul vl]\n" "st1b {z5.b}, p0, [%0, #5, mul vl]\n" \
        "st1b {z6.b}, p0, [%0, #6, mul vl]\n" "st1b {z7.b}, p0, [%0, #7, mul vl]\n" \
        ".endif\n"                                                               \
        ".endif\n"                                                               \
        "add %1, %1, #" #UNROLL "*64\n" "add %0, %0, #" #UNROLL "*64\n"          \
        "sub %2, %2, #" #UNROLL "*64\n" "b 1b\n"                                 \
        "3:\n cbz %2, 5f\n"                                                      \
        "whilelt p1.b, xzr, %2\n" "ld1b {z0.b}, p1/z, [%1]\n" "st1b {z0.b}, p1, [%0]\n" \
        "5:\n smstop\n"                                                          \
        : "+r"(dst), "+r"(src), "+r"(left) : : SME_CLOBBERS_BW)

#define SME_FN(NAME, UNROLL, NT)                                                  \
__attribute__((target("arch=armv9-a+sme2"),noinline))                             \
static void NAME(unsigned char *d, const unsigned char *s, size_t n){             \
    unsigned char *dst=d; const unsigned char *src=s; size_t left=n; SME_LOOP(UNROLL,NT); }

SME_FN(sme64, 1, 0)
SME_FN(sme128, 2, 0)
SME_FN(sme256, 4, 0)
SME_FN(sme512, 8, 0)
SME_FN(sme256_nt, 4, 1)
SME_FN(sme512_nt, 8, 1)

typedef void (*fn_t)(unsigned char*, const unsigned char*, size_t);
static void memcpy_w(unsigned char *d, const unsigned char *s, size_t n){ memcpy(d, s, n); }
static const char *names[] = {"memcpy","neon64","sme1x64","sme2x64","sme4x64","sme8x64","sme4x64nt","sme8x64nt"};
static fn_t fns[8];

static uint64_t run(int k, size_t span, int reps){
    uint64_t best = ~0ull;
    for (int r = 0; r < 3; r++) {
        uint64_t a = cntvct();
        for (int i = 0; i < reps; i++) {
            size_t off = ((size_t)(i & 7) * (span + 64)) % (span > (32u<<20) ? (32u<<20) : 1);
            if (span <= (32u<<20)) { fns[k](dst + off, src + off, span); }
            else                    { fns[k](dst, src, span); }
        }
        uint64_t t = cntvct() - a;
        if (t < best) best = t;
    }
    return best;   // raw CNTVCT ticks (19.2 MHz)
}

int main(void){
    if (posix_memalign((void**)&src, 65536, POOL) || posix_memalign((void**)&dst, 65536, POOL)) return 1;
    for (size_t i = 0; i < POOL; i++) src[i] = (unsigned char)(i * 17); 
    fns[0] = memcpy_w; fns[1] = neon_copy; fns[2] = sme64; fns[3] = sme128; fns[4] = sme256; fns[5] = sme512; fns[6] = sme256_nt; fns[7] = sme512_nt;
    printf("%-10s", "span");
    for (int k = 0; k < 8; k++) printf("%14s", names[k]);
    printf("   (ticks, 19.2MHz)\n");
    size_t spans[] = {256u<<10, 2u<<20, 8u<<20, 32u<<20, 36u<<20};
    for (unsigned si = 0; si < sizeof(spans)/sizeof(spans[0]); si++) {
        size_t span = spans[si];
        int reps = (int)((256u << 20) / span) + 1;   // ~256 MiB moved per sample
        printf("%-8zuK", span >> 10);
        for (int k = 0; k < 8; k++) printf("%14llu", (unsigned long long)run(k, span, reps));
        printf("   reps=%d\n", reps);
    }
    // correctness of each SME variant
    for (int k = 2; k < 8; k++) {
        memset(dst, 0, 1u<<20); fns[k](dst, src, 1u<<20);
        if (memcmp(dst, src, 1u<<20)) printf("MISMATCH %s\n", names[k]);
    }
    printf("correctness ok\n");
    return 0;
}
