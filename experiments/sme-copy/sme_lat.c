// Latency DISTRIBUTION of a single copy: memcpy vs NEON vs streaming-SME.
// Uses a counting histogram (CNTVCT ticks, 52 ns granularity) instead of qsort.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

extern void ruzstd_sme_copy(unsigned char *dst, const unsigned char *src, size_t n);        // 4x64B
extern void ruzstd_sme_copy_naive(unsigned char *dst, const unsigned char *src, size_t n);  // 1x64B

#define POOL (32u << 20)
#define NBUCK 4096          // tick buckets (0..4095 ticks = 0..213 us)
#define MAXN 400000
static unsigned char *pool;
static uint32_t *hist, *hist2;
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
__attribute__((target("arch=armv9-a+sme2"),noinline))
static void sme_kick(void){ __asm__ volatile("smstart\nsmstop" ::: "memory"); }
static void memcpy_w(unsigned char*d,const unsigned char*s,size_t n){ memcpy(d,s,n); }

typedef void (*fn_t)(unsigned char*, const unsigned char*, size_t);

static uint32_t pct(const uint32_t *h, int n, double q){
    uint32_t need = (uint32_t)(n * q); uint32_t acc = 0;
    for (int i = 0; i < NBUCK; i++) { acc += h[i]; if (acc > need) return i; }
    return NBUCK - 1;
}

static void measure(const char *name, fn_t f, size_t size, int n, size_t off){
    memset(hist, 0, NBUCK * sizeof(uint32_t));
    unsigned char *d0 = pool + (16u<<20) + off;
    const unsigned char *s0 = pool + off;
    for (int i = 0; i < n; i++) {
        unsigned char *d = d0 + (size_t)(i & 7) * size;
        const unsigned char *s = s0 + (size_t)(i & 7) * size;
        if ((size_t)(d - pool) + size > POOL - 4096) { d = d0; s = s0; }
        uint64_t a = tick(); f(d, s, size); uint64_t b = tick();
        uint64_t dt = b - a; if (dt >= NBUCK) dt = NBUCK - 1;
        hist[dt]++;
    }
    double sum = 0; for (int i = 0; i < NBUCK; i++) sum += (double)i * hist[i];
    uint32_t over = hist[NBUCK-1];
    printf("  %-11s min=%3u p50=%4u p90=%4u p99=%4u max=%4u mean=%7.1f  >%dticks=%u\n",
           name, pct(hist,n,0.0), pct(hist,n,0.5), pct(hist,n,0.9), pct(hist,n,0.99),
           pct(hist,n,0.999999), sum / n, NBUCK-1, over);
    // coarse shape: 8 log-ish buckets of ticks
    const int edges[9] = {0,1,2,3,4,6,9,16,NBUCK};
    printf("            dist[ticks]: ");
    for (int e = 0; e < 8; e++) {
        uint32_t c = 0; for (int i = edges[e]; i < edges[e+1]; i++) c += hist[i];
        printf("%d-%d:%.0f%% ", edges[e], edges[e+1]-1, 100.0*c/n);
    }
    printf("\n");
}

int main(void){
    if (posix_memalign((void**)&pool, 65536, POOL)) return 1;
    hist = calloc(NBUCK, sizeof(uint32_t)); hist2 = calloc(NBUCK, sizeof(uint32_t));
    for (size_t i = 0; i < POOL; i++) pool[i] = (unsigned char)(i * 29);

    printf("clock: CNTVCT 19.2 MHz -> 1 tick = 52.1 ns (quantization floor)\n\n");
    printf("A) empty measurement floor (tick();tick())\n");
    { memset(hist,0,NBUCK*sizeof(uint32_t)); int n=100000;
      for (int i=0;i<n;i++){ uint64_t a=tick(); uint64_t b=tick(); uint64_t d=b-a; if(d>=NBUCK)d=NBUCK-1; hist[d]++; }
      printf("   min=%u p50=%u p99=%u max=%u\n", pct(hist,n,0), pct(hist,n,.5), pct(hist,n,.99), pct(hist,n,.99999)); }
    printf("B) smstart+smstop only (call into C, no data moved)\n");
    { memset(hist,0,NBUCK*sizeof(uint32_t)); int n=100000;
      for (int i=0;i<n;i++){ uint64_t a=tick(); sme_kick(); uint64_t b=tick(); uint64_t d=b-a; if(d>=NBUCK)d=NBUCK-1; hist[d]++; }
      printf("   min=%u p50=%u p90=%u p99=%u max=%u mean=%.2f ticks (%.0f ns)\n",
             pct(hist,n,0),pct(hist,n,.5),pct(hist,n,.9),pct(hist,n,.99),pct(hist,n,.99999),
             ({double s=0; for(int i=0;i<NBUCK;i++) s+=(double)i*hist[i]; s/n;}), 
             ({double s=0; for(int i=0;i<NBUCK;i++) s+=(double)i*hist[i]; s/n*52.1;})); }
    printf("\nC) per-copy latency, dst unaligned (+3), rotating 8 slots x size\n");
    size_t sizes[] = {1024, 4096, 16384, 65536, 262144};
    int reps[]     = {300000, 200000, 120000, 30000, 8000};
    for (unsigned si = 0; si < sizeof(sizes)/sizeof(sizes[0]); si++) {
        printf(" size=%zu (n=%d)\n", sizes[si], reps[si]);
        measure("memcpy",   memcpy_w,              sizes[si], reps[si], 3);
        measure("neon64",   neon_copy,             sizes[si], reps[si], 3);
        measure("sme_1x64", ruzstd_sme_copy_naive, sizes[si], reps[si], 3);
        measure("sme_4x64", ruzstd_sme_copy,       sizes[si], reps[si], 3);
    }
    return 0;
}
