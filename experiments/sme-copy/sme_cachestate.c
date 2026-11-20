// Why is the in-situ SME copy slower / heavier-tailed than the isolated one?
// Hypothesis (from M4 SME research: the SME unit talks to L2, not L1): when the
// source or destination lines are L1-DIRTY (just written by the core, as in a
// decoder writing literals/matches), the streaming-mode copy pays extra.
//
// We vary the cache state of src/dst immediately before each timed copy and
// compare memcpy / NEON / SME(4x64 aligned) / SME(1x64) latency distributions.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

extern void ruzstd_sme_copy(unsigned char *dst, const unsigned char *src, size_t n);
extern void ruzstd_sme_copy_naive(unsigned char *dst, const unsigned char *src, size_t n);
extern void ruzstd_sme_copy_pf(unsigned char *dst, const unsigned char *src, size_t n);

#define POOL (32u << 20)
#define NBUCK 8192
#define NMAX 200000
static unsigned char *pool, *src, *dst, *scratch;
static uint32_t *hist;
static uint64_t tick(void){ uint64_t v; __asm__ volatile("mrs %0, cntvct_el0":"=r"(v)); return v; }

__attribute__((target("arch=armv8-a"),noinline))
static void neon_copy(unsigned char *d, const unsigned char *s, size_t n){
    size_t left=n;
    __asm__ volatile(
        "1:\ncmp %2,#64\nb.lo 2f\nldp q0,q1,[%1]\nldp q2,q3,[%1,#32]\nstp q0,q1,[%0]\nstp q2,q3,[%0,#32]\n"
        "add %1,%1,#64\nadd %0,%0,#64\nsub %2,%2,#64\nb 1b\n"
        "2:\ncbz %2,4f\n3:\nldrb w4,[%1]\nstrb w4,[%0]\nadd %1,%1,#1\nadd %0,%0,#1\nsubs %2,%2,#1\nb.ne 3b\n4:\n"
        : "+r"(d), "+r"(s), "+r"(left) : : "v0","v1","v2","v3","w4","cc","memory");
}
static void memcpy_w(unsigned char*d,const unsigned char*s,size_t n){ memcpy(d,s,n); }
typedef void (*fn_t)(unsigned char*, const unsigned char*, size_t);

static uint32_t pct(const uint32_t *h, int n, double q){
    uint32_t need=(uint32_t)(n*q), acc=0;
    for (int i=0;i<NBUCK;i++){ acc+=h[i]; if (acc>need) return i; }
    return NBUCK-1;
}

// case: 0=src clean (just read), 1=src dirty (just written), 2=src+dst dirty,
//       3=src cold (evicted by a 16 MiB sweep), 4=decoder-like pipeline
static void run_case(const char *cname, int cs, fn_t f, const char *mname, size_t size, int n){
    memset(hist, 0, NBUCK*sizeof(uint32_t));
    size_t span = size;
    unsigned char *s = src, *d = dst;
    volatile unsigned long long sink = 0;
    for (int i = 0; i < n; i++) {
        switch (cs) {
            case 0: for (size_t k=0;k<span;k+=64) sink += s[k]; break;              // read -> clean lines
            case 1: for (size_t k=0;k<span;k+=64) s[k] = (unsigned char)i; break;   // write -> dirty lines
            case 2: for (size_t k=0;k<span;k+=64) s[k] = (unsigned char)i;
                    for (size_t k=0;k<span;k+=64) d[k] = (unsigned char)(i+1); break;
            case 3: for (size_t k=0;k<(16u<<20);k+=64) scratch[k] = (unsigned char)i; break; // evict
            case 4: // decoder-like: fresh literals written, then a match copy
                    for (size_t k=0;k<4096;k+=64) s[k] = (unsigned char)i;
                    break;
        }
        uint64_t a = tick(); f(d, s, size); uint64_t b = tick();
        uint64_t dt = b-a; if (dt>=NBUCK) dt=NBUCK-1; hist[dt]++;
    }
    (void)sink;
    printf("    %-8s min=%4u p50=%5u p90=%5u p99=%5u max=%5u\n",
           mname, pct(hist,n,0), pct(hist,n,0.5), pct(hist,n,0.9), pct(hist,n,0.99), pct(hist,n,1.0));
}

int main(void){
    if (posix_memalign((void**)&pool, 65536, POOL)) return 1;
    hist = calloc(NBUCK, sizeof(uint32_t));
    src = pool; dst = pool + (8u<<20); scratch = pool + (16u<<20);
    for (size_t i=0;i<POOL;i++) pool[i] = (unsigned char)(i*31);

    const char *cases[] = {"src_clean(just read)","src_dirty(just written)","src+dst_dirty","src_cold(evicted)","decoder-like(write4K then copy)"};
    size_t sizes[] = {16384, 65536};
    for (unsigned si=0; si<2; si++) {
        size_t size = sizes[si];
        int n = size==16384 ? 60000 : 20000;
        printf("== size=%zu (n=%d) ==\n", size, n);
        for (int cs=0; cs<5; cs++) {
            printf("  case %d: %s\n", cs, cases[cs]);
            run_case(cases[cs], cs, memcpy_w,             "memcpy",  size, n);
            run_case(cases[cs], cs, neon_copy,            "neon64",  size, n);
            run_case(cases[cs], cs, ruzstd_sme_copy,      "sme_4x64",size, n);
            run_case(cases[cs], cs, ruzstd_sme_copy_pf,   "sme_pf",  size, n);
            run_case(cases[cs], cs, ruzstd_sme_copy_naive,"sme_1x64",size, n);
        }
    }
    return 0;
}
