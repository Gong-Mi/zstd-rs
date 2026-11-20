// Probe for a fine-grained clock: try perf_event_open (which can enable
// PMUSERENR.EL0.EN) and then read PMCCNTR_EL0 from EL0 under a SIGILL guard.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <setjmp.h>
#include <signal.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <linux/perf_event.h>

static sigjmp_buf jb;
static void on_ill(int s){ (void)s; siglongjmp(jb, 1); }

static int try_pmccntr(uint64_t *out) {
    uint64_t a = 0, b = 0;
    if (sigsetjmp(jb, 1)) return -1;            // trapped
    __asm__ volatile("mrs %0, pmccntr_el0" : "=r"(a));
    for (volatile int i = 0; i < 1000000; i++);
    __asm__ volatile("mrs %0, pmccntr_el0" : "=r"(b));
    *out = b - a;
    return (b > a) ? 0 : 1;
}

int main(void) {
    struct sigaction sa; memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_ill; sigaction(SIGILL, &sa, NULL);

    uint64_t d = 0;
    int r = try_pmccntr(&d);
    printf("before perf_event_open: PMCCNTR read %s (delta=%llu)\n",
           r == 0 ? "OK" : r == -1 ? "SIGILL (no EL0 access)" : "stuck", (unsigned long long)d);

    struct perf_event_attr pe; memset(&pe, 0, sizeof pe);
    pe.type = PERF_TYPE_HARDWARE; pe.size = sizeof pe;
    pe.config = PERF_COUNT_HW_CPU_CYCLES;
    pe.disabled = 0; pe.exclude_kernel = 1; pe.exclude_hv = 1;
    int fd = (int)syscall(__NR_perf_event_open, &pe, 0, -1, -1, 0);
    printf("perf_event_open: %d %s\n", fd, fd < 0 ? strerror(-fd) : "(ok)");

    r = try_pmccntr(&d);
    printf("after  perf_event_open: PMCCNTR read %s (delta=%llu)\n",
           r == 0 ? "OK -> cycle-accurate clock available" : r == -1 ? "SIGILL" : "stuck",
           (unsigned long long)d);
    if (fd >= 0) {
        uint64_t v = 0;
        if (read(fd, &v, sizeof v) == (ssize_t)sizeof v) printf("perf read: %llu cycles\n", (unsigned long long)v);
        close(fd);
    }
    return 0;
}
