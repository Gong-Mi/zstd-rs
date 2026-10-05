// Does destination alignment explain the in-situ regression?
// Compares memcpy / NEON / SME(naive) / SME(aligned) at several sizes and offsets.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

extern void ruzstd_sme_copy(unsigned char *dst, const unsigned char *src, size_t n);
extern void ruzstd_sme_copy_naive(unsigned char *dst, const unsigned char *src, size_t n);
extern uint64_t ruzstd_sme_pair_cost(int reps);

#define POOL (96u << 20)
static unsigned char *pool;
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
static void one(int mode, size_t size, size_t off, int reps){
    unsigned char *d0 = pool + 32u*1024*1024 + off;   // dst
    const unsigned char *s0 = pool + off;             // src, 32 MiB apart
    for (int i=0;i<reps;i++){
        unsigned char *d = d0 + (size_t)(i % 8) * size;
        const unsigned char *s = s0 + (size_t)(i % 8) * size;
        if ((size_t)(d - pool) + size > POOL - 4096) { d = d0; s = s0; }
        switch(mode){
            case 0: memcpy(d,s,size); break;
            case 1: neon_copy(d,s,size); break;
            case 2: ruzstd_sme_copy_naive(d,s,size); break;
            default: ruzstd_sme_copy(d,s,size); break;
        }
    }
}
static uint64_t run(int mode, size_t size, size_t off, int reps){
    uint64_t best=~0ull;
    for(int r=0;r<3;r++){ uint64_t a=cntvct(); one(mode,size,off,reps); uint64_t t=cntvct()-a; if(t<best)best=t; }
    return best;
}
int main(void){
    if (posix_memalign((void**)&pool,4096,POOL)) return 1;
    for (size_t i=0;i<POOL;i++) pool[i]=(unsigned char)(i*13);
    int reps = 300;
    printf("PAIR_TICKS=%llu\n",(unsigned long long)ruzstd_sme_pair_cost(2000));
    printf("%8s %6s %12s %12s %12s %12s\n","size","dst+","memcpy","neon","sme_naive","sme_aligned");
    size_t sizes[]={4096,16384,65536,262144};
    size_t offs[]={0,1,32};
    for (unsigned si=0; si<sizeof(sizes)/sizeof(sizes[0]); si++){
        for (unsigned oi=0; oi<sizeof(offs)/sizeof(offs[0]); oi++){
            size_t size=sizes[si], off=offs[oi];
            uint64_t m=run(0,size,off,reps), n=run(1,size,off,reps), a=run(2,size,off,reps), b=run(3,size,off,reps);
            printf("%8zu %6zu %12llu %12llu %12llu %12llu\n", size, off,
                (unsigned long long)m,(unsigned long long)n,(unsigned long long)a,(unsigned long long)b);
        }
    }
    return 0;
}
