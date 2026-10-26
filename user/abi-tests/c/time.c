/* Wave A: clock_gettime / clock_getres per clock id, nanosleep and clock_nanosleep rules, gettimeofday and
 * time agreeing with CLOCK_REALTIME. Raw syscalls (no vDSO), so the kernel answers every call. Absolute wall
 * times are never printed (V2 has no wall clock source yet). The alarm clocks (8, 9) are left out: they
 * depend on an RTC device (CachyOS has one, V2 does not). */
#include "abi.h"
#include <sys/time.h>

static long ts_ns(struct timespec t) {
    return t.tv_sec * 1000000000l + t.tv_nsec;
}

static void clocks(void) {
    static const int ids[] = { 0, 1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 100, -1, -2, -5, -6 };
    struct timespec t;
    char label[64];
    for (unsigned i = 0; i < sizeof ids / sizeof ids[0]; i++) {
        int id = ids[i];
        snprintf(label, sizeof label, "clock_gettime(%d)", id);
        long r = SYS(label, SYS_clock_gettime, id, &t);
        if (r == 0)
            yes("  nsec below 1e9", t.tv_nsec >= 0 && t.tv_nsec < 1000000000);
        snprintf(label, sizeof label, "clock_getres(%d)", id);
        r = SYS(label, SYS_clock_getres, id, &t);
        /* Coarse clocks report the tick length (HZ differs between kernels); the others 1 ns. */
        if (r == 0 && id != 5 && id != 6)
            printf("  resolution %ld ns\n", ts_ns(t));
    }
    SYS("clock_getres(1, NULL)", SYS_clock_getres, 1, NULL);
    SYS("clock_gettime(1, kernel address)", SYS_clock_gettime, 1, KERNEL_ADDR);
    SYS("clock_gettime(1, NULL)", SYS_clock_gettime, 1, NULL);
    int back = 0;
    long last = 0;
    for (int i = 0; i < 2000; i++) {
        syscall(SYS_clock_gettime, 1, &t);
        if (ts_ns(t) < last)
            back = 1;
        last = ts_ns(t);
    }
    yes("CLOCK_MONOTONIC never went back in 2000 reads", !back);
    struct timespec c0, c1;
    syscall(SYS_clock_gettime, 2, &c0);
    for (volatile int i = 0; i < 1000000; i++)
        ;
    syscall(SYS_clock_gettime, 2, &c1);
    yes("CLOCK_PROCESS_CPUTIME_ID did not go back", ts_ns(c1) >= ts_ns(c0));
}

static void walltime(void) {
    struct timespec rt;
    struct timeval tv;
    struct { int minuteswest, dsttime; } tz;
    syscall(SYS_clock_gettime, 0, &rt);
    SYS("gettimeofday(&tv, &tz)", SYS_gettimeofday, &tv, &tz);
    yes("gettimeofday within a second of CLOCK_REALTIME", tv.tv_sec - rt.tv_sec <= 1 && tv.tv_sec >= rt.tv_sec && tv.tv_usec < 1000000);
    SYS("gettimeofday(NULL, NULL)", SYS_gettimeofday, NULL, NULL);
    SYS("gettimeofday(kernel address, NULL)", SYS_gettimeofday, KERNEL_ADDR, NULL);
    SYS("gettimeofday(NULL, kernel address)", SYS_gettimeofday, NULL, KERNEL_ADDR);
    time_t t = 0;
    long s = syscall(SYS_time, &t);
    yes("time() agrees with CLOCK_REALTIME and its argument", s == t && s - rt.tv_sec <= 1 && s >= rt.tv_sec);
    SYS("time(kernel address)", SYS_time, KERNEL_ADDR);
}

static void sleeps(void) {
    struct timespec req, rem;
    req = (struct timespec){ 0, 1000000000 };
    SYS("nanosleep(nsec 1e9)", SYS_nanosleep, &req, 0);
    req = (struct timespec){ 0, -1 };
    SYS("nanosleep(nsec -1)", SYS_nanosleep, &req, 0);
    req = (struct timespec){ -1, 0 };
    SYS("nanosleep(sec -1)", SYS_nanosleep, &req, 0);
    SYS("nanosleep(NULL)", SYS_nanosleep, NULL, 0);
    req = (struct timespec){ 0, 20000000 };
    uint64_t t0 = ns_of(CLOCK_MONOTONIC);
    SYS("nanosleep(20 ms)", SYS_nanosleep, &req, &rem);
    yes("slept at least 20 ms", ns_of(CLOCK_MONOTONIC) - t0 >= 20000000);
    req = (struct timespec){ 0, 0 };
    SYS("nanosleep(0)", SYS_nanosleep, &req, 0);
    static const int ids[] = { 0, 1, 2, 3, 4, 5, 6, 7, 10, 11, 12 };
    char label[64];
    req = (struct timespec){ 0, 1000000 };
    for (unsigned i = 0; i < sizeof ids / sizeof ids[0]; i++) {
        if (ids[i] == 2)
            continue; /* a CPU-time sleep: V2 refuses it (no CPU-time wake-ups yet) */
        snprintf(label, sizeof label, "clock_nanosleep(%d, 0, 1 ms)", ids[i]);
        SYS(label, SYS_clock_nanosleep, ids[i], 0, &req, 0);
    }
    SYS("clock_nanosleep(1, 0, NULL)", SYS_clock_nanosleep, 1, 0, NULL, 0);
    req = (struct timespec){ 0, 1000000000 };
    SYS("clock_nanosleep(1, 0, nsec 1e9)", SYS_clock_nanosleep, 1, 0, &req, 0);
    SYS("clock_nanosleep(4, 0, nsec 1e9)", SYS_clock_nanosleep, 4, 0, &req, 0);
    /* Absolute times in the past return at once. */
    req = (struct timespec){ 0, 5 };
    t0 = ns_of(CLOCK_MONOTONIC);
    SYS("clock_nanosleep(MONOTONIC, ABSTIME, 5 ns after boot)", SYS_clock_nanosleep, 1, 1, &req, 0);
    SYS("clock_nanosleep(REALTIME, ABSTIME, the epoch)", SYS_clock_nanosleep, 0, 1, &req, 0);
    yes("absolute sleeps into the past returned at once", ns_of(CLOCK_MONOTONIC) - t0 < 500000000);
    /* An absolute time 30 ms ahead. */
    struct timespec now;
    syscall(SYS_clock_gettime, 1, &now);
    long target = ts_ns(now) + 30000000;
    req = (struct timespec){ target / 1000000000, target % 1000000000 };
    SYS("clock_nanosleep(MONOTONIC, ABSTIME, now + 30 ms)", SYS_clock_nanosleep, 1, 1, &req, 0);
    syscall(SYS_clock_gettime, 1, &now);
    yes("woke at or after the absolute time", ts_ns(now) >= target);
    /* Unknown flag bits are ignored. */
    req = (struct timespec){ 0, 1000000 };
    SYS("clock_nanosleep(1, flags 0x10, 1 ms)", SYS_clock_nanosleep, 1, 0x10, &req, 0);
}

int main(void) {
    clocks();
    walltime();
    sleeps();
    return 0;
}
