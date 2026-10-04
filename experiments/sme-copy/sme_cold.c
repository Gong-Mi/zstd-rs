// Focused, interleaved test: SME copy vs core copies as a function of source
// cache residency. All modes run round-robin inside the same loop so drift and
// machine load hit every mode equally.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

extern void ruzstd_sme_copy(unsigned char *dst, const unsigned char *src, size_t n);
extern void ruzstd_sme_copy_pf(unsigned char *dst, const unsigned char *src, size_t n);

#define POOL (48u << 20)
#define NBUCK 4096
#define NMODE 4
static unsigned char *pool, *src, *dst, *evict_buf;
static uint32_t hist[NMODE][NBUCK];
static const char *names[NMODE] = {"memcpy","neon64","sme_4x64","sme_pf"};
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
static fn_t fns[NMODE];

static inline void evict(void){ for (size_t k=0;k<(24u<<20);k+=64) evict_buf[k] = (unsigned char)k; }
static inline void warm(void){ for (size_t k=0;k<(16u<<10);k+=64) src[k] = (unsigned char)k; }

static uint32_t pct(const uint32_t *h,int n,double q){ uint32_t need=(uint32_t)(n*q),acc=0;
    for(int i=0;i<NBUCK;i++){acc+=h[i]; if(acc>need) return i;} return NBUCK-1; }

static void report(const char *tag, size_t size, int n){
    printf("== %s  size=%zu  n=%d ==\n", tag, size, n);
    for (int m=0;m<NMODE;m++)
        printf("   %-11s min=%4u p50=%5u p90=%5u p99=%5u mean=%7.1f ticks\n", names[m],
               pct(hist[m],n,0), pct(hist[m],n,0.5), pct(hist[m],n,0.9), pct(hist[m],n,0.99),
               ({double s=0; for(int i=0;i<NBUCK;i++) s+=(double)i*hist[m][i]; s/n;}));
}

int main(void){
    if (posix_memalign((void**)&pool,65536,POOL)) return 1;
    src = pool; dst = pool + (16u<<20); evict_buf = pool + (24u<<20);
    for (size_t i=0;i<POOL;i++) pool[i]=(unsigned char)(i*7);
    fns[0]=memcpy_w; fns[1]=neon_copy; fns[2]=ruzstd_sme_copy; fns[3]=ruzstd_sme_copy_pf;

    size_t sizes[] = {16384, 65536, 262144};
    for (unsigned si=0; si<3; si++) {
        size_t size = sizes[si];
        int n = (size<=16384)? 3000 : (size==65536? 1500 : 400);
        // warm source (resident)
        memset(hist,0,sizeof hist);
        warm();
        for (int i=0;i<n;i++) for (int m=0;m<NMODE;m++){
            uint64_t a=tick(); fns[m](dst,src,size); uint64_t b=tick();
            uint64_t d=b-a; if(d>=NBUCK)d=NBUCK-1; hist[m][d]++;
        }
        report("src RESIDENT (warm)", size, n);
        // evict before every copy
        memset(hist,0,sizeof hist);
        for (int i=0;i<n;i++) for (int m=0;m<NMODE;m++){
            evict();
            uint64_t a=tick(); fns[m](dst,src,size); uint64_t b=tick();
            uint64_t d=b-a; if(d>=NBUCK)d=NBUCK-1; hist[m][d]++;
        }
        report("src EVICTED before each copy", size, n);
        printf("\n");
    }
    return 0;
}
