/* SC-6: affinity, getcpu, nice values, scheduling policies, times/getrusage and the CPU clocks, with raw
 * syscalls (musl's sched_setscheduler is a stub). Every change is made to the program itself; limits are lowered
 * to known values first (RLIMIT_NICE and RLIMIT_RTPRIO 0), the nice value is set to a known value before it is
 * read (the host may start programs niced). No CPU numbers or counts are printed, only relations.
 *
 * Modes: none = the transcript; "burn" = ~300 ms of user time, then ~300 ms of syscalls, then what times(),
 * getrusage() and the CPU clocks say about it (relations with generous margins); "child" = a forked child
 * inherits affinity, nice and policy (Linux sched_fork), the parent changes them by the child's pid and the
 * child sees the change, then wait4's rusage and RUSAGE_CHILDREN. */
#include "abi.h"
#include <sched.h>
#include <sys/resource.h>
#include <sys/times.h>
#include <sys/wait.h>

#define SCHED_RESET_ON_FORK 0x40000000
#define CPUCLOCK_PROF 0
#define CPUCLOCK_VIRT 1
#define CPUCLOCK_SCHED 2
#define THREAD_CLOCK(pid, which) ((clockid_t)((~(unsigned)(pid) << 3) | 4u | (unsigned)(which)))
#define PROCESS_CLOCK(pid, which) ((clockid_t)((~(unsigned)(pid) << 3) | (unsigned)(which)))
#define NOBODY 4000000

static long ns(struct timespec t) {
    return t.tv_sec * 1000000000l + t.tv_nsec;
}

static long tv_ns(struct timeval t) {
    return t.tv_sec * 1000000000l + t.tv_usec * 1000l;
}

static void limits(void) {
    struct rlimit z = { 0, 0 };
    SYS("setrlimit(RLIMIT_NICE, 0)", SYS_setrlimit, RLIMIT_NICE, &z);
    SYS("setrlimit(RLIMIT_RTPRIO, 0)", SYS_setrlimit, RLIMIT_RTPRIO, &z);
}

static void affinity(void) {
    unsigned char m[128], g[128];
    long size = syscall(SYS_sched_getaffinity, 0, 128, g);
    yes("sched_getaffinity(len 128) returns the cpumask size: a multiple of 8, at most 128", size > 0 && size % 8 == 0 && size <= 128);
    SYS("sched_getaffinity(len 4)", SYS_sched_getaffinity, 0, 4, g);
    SYS("sched_getaffinity(len 12)", SYS_sched_getaffinity, 0, 12, g);
    SYS("sched_getaffinity(pid -1)", SYS_sched_getaffinity, -1, 128, g);
    SYS("sched_getaffinity(no such pid)", SYS_sched_getaffinity, NOBODY, 128, g);
    SYS("sched_getaffinity(kernel address)", SYS_sched_getaffinity, 0, 128, KERNEL_ADDR);
    /* len is an unsigned int, multiplied by 8 in 32 bits: 2^29 wraps to 0 bits; bits above 32 are dropped. */
    SYS("sched_getaffinity(len 2^29)", SYS_sched_getaffinity, 0, 0x20000000l, g);
    yes("sched_getaffinity(len 2^32 + 128) answers like len 128", syscall(SYS_sched_getaffinity, 0, (1l << 32) + 128, g) == size);
    memset(m, 0, sizeof m);
    SYS("sched_setaffinity(bad pointer, no such pid)", SYS_sched_setaffinity, NOBODY, 8, KERNEL_ADDR);
    SYS("sched_setaffinity(no such pid)", SYS_sched_setaffinity, NOBODY, 8, m);
    SYS("sched_setaffinity(pid -1)", SYS_sched_setaffinity, -1, 8, m);
    SYS("sched_setaffinity(empty)", SYS_sched_setaffinity, 0, 8, m);
    SYS("sched_setaffinity(len 0)", SYS_sched_setaffinity, 0, 0, m);
    m[7] = 0x80; /* CPU 63 only: beyond the CPUs of both machines */
    SYS("sched_setaffinity(only CPU 63)", SYS_sched_setaffinity, 0, 8, m);
    memset(m, 0, sizeof m);
    m[0] = 1;
    m[127] = 0xff; /* beyond cpumask_size: ignored */
    SYS("sched_setaffinity(CPU 0, len 128)", SYS_sched_setaffinity, 0, 128, m);
    unsigned cpu = 99, node = 99;
    SYS("getcpu", SYS_getcpu, &cpu, &node, 0);
    printf("  cpu %u node %u\n", cpu, node);
    memset(g, 0, sizeof g);
    syscall(SYS_sched_getaffinity, 0, 128, g);
    int n = 0;
    for (int i = 0; i < 128; i++)
        n += __builtin_popcount(g[i]);
    yes("  one CPU in the mask, CPU 0", n == 1 && g[0] == 1);
    m[0] = 3;
    SYS("sched_setaffinity(CPUs 0-1, len 1)", SYS_sched_setaffinity, 0, 1, m);
    syscall(SYS_getcpu, &cpu, 0, 0);
    yes("  on CPU 0 or 1", cpu <= 1);
    memset(m, 0xff, sizeof m);
    SYS("sched_setaffinity(all)", SYS_sched_setaffinity, 0, 128, m);
    memset(g, 0, sizeof g);
    syscall(SYS_sched_getaffinity, 0, 128, g);
    n = 0;
    for (int i = 0; i < 128; i++)
        n += __builtin_popcount(g[i]);
    yes("  every online CPU in the mask", n == sysconf(_SC_NPROCESSORS_ONLN));
}

static void getcpu(void) {
    unsigned cpu = 99999, node = 99999;
    SYS("getcpu(NULL, NULL)", SYS_getcpu, 0, 0, 0);
    SYS("getcpu(cpu, NULL)", SYS_getcpu, &cpu, 0, 0);
    yes("  cpu below the online count", cpu < (unsigned)sysconf(_SC_NPROCESSORS_ONLN));
    SYS("getcpu(NULL, node)", SYS_getcpu, 0, &node, 0);
    printf("  node %u\n", node);
    SYS("getcpu(kernel address, node)", SYS_getcpu, KERNEL_ADDR, &node, 0);
    SYS("getcpu(cpu, kernel address)", SYS_getcpu, &cpu, KERNEL_ADDR, 0);
}

static void priority(void) {
    SYS("setpriority(which 3)", SYS_setpriority, 3, 0, 0);
    SYS("setpriority(which -1)", SYS_setpriority, -1, 0, 0);
    SYS("getpriority(which 3)", SYS_getpriority, 3, 0);
    SYS("setpriority(PRIO_PROCESS, 0, 5)", SYS_setpriority, PRIO_PROCESS, 0, 5);
    SYS("getpriority(PRIO_PROCESS, 0) (20 - nice)", SYS_getpriority, PRIO_PROCESS, 0);
    SYS("setpriority(PRIO_PROCESS, own pid, 7)", SYS_setpriority, PRIO_PROCESS, getpid(), 7);
    SYS("getpriority(PRIO_PROCESS, own pid)", SYS_getpriority, PRIO_PROCESS, getpid());
    SYS("setpriority(5): lowering without RLIMIT_NICE", SYS_setpriority, PRIO_PROCESS, 0, 5);
    SYS("setpriority(7): the same value", SYS_setpriority, PRIO_PROCESS, 0, 7);
    SYS("setpriority(100): clamped to 19", SYS_setpriority, PRIO_PROCESS, 0, 100);
    SYS("getpriority after 100", SYS_getpriority, PRIO_PROCESS, 0);
    SYS("setpriority(-100) as a user", SYS_setpriority, PRIO_PROCESS, 0, -100);
    SYS("setpriority(no such pid)", SYS_setpriority, PRIO_PROCESS, NOBODY, 0);
    SYS("getpriority(no such pid)", SYS_getpriority, PRIO_PROCESS, NOBODY);
    SYS("setpriority(pid -1)", SYS_setpriority, PRIO_PROCESS, -1, 0);
    SYS("getpriority(pid -1)", SYS_getpriority, PRIO_PROCESS, -1);
    SYS("setpriority(PRIO_USER, no such user)", SYS_setpriority, PRIO_USER, NOBODY, 19);
    SYS("getpriority(PRIO_USER, no such user)", SYS_getpriority, PRIO_USER, NOBODY);
    SYS("setpriority(PRIO_PGRP, no such group)", SYS_setpriority, PRIO_PGRP, NOBODY, 19);
    long r = syscall(SYS_getpriority, PRIO_PGRP, 0);
    yes("getpriority(PRIO_PGRP, 0) in 1..40", r >= 1 && r <= 40);
    r = syscall(SYS_getpriority, PRIO_USER, 0);
    yes("getpriority(PRIO_USER, 0) in 1..40", r >= 1 && r <= 40);
}

static void policy(void) {
    int p0 = 0, p1 = 1, p50 = 50, p100 = 100, pm1 = -1, out = 77;
    SYS("sched_getscheduler(0)", SYS_sched_getscheduler, 0);
    SYS("sched_getscheduler(-1)", SYS_sched_getscheduler, -1);
    SYS("sched_getscheduler(no such pid)", SYS_sched_getscheduler, NOBODY);
    SYS("sched_getparam(0)", SYS_sched_getparam, 0, &out);
    printf("  priority %d\n", out);
    SYS("sched_getparam(NULL)", SYS_sched_getparam, 0, NULL);
    SYS("sched_getparam(-1)", SYS_sched_getparam, -1, &out);
    SYS("sched_getparam(no such pid)", SYS_sched_getparam, NOBODY, &out);
    SYS("sched_getparam(kernel address)", SYS_sched_getparam, 0, KERNEL_ADDR);
    SYS("sched_setscheduler(policy -1)", SYS_sched_setscheduler, 0, -1, &p0);
    SYS("sched_setscheduler(NULL)", SYS_sched_setscheduler, 0, SCHED_OTHER, NULL);
    SYS("sched_setscheduler(pid -1)", SYS_sched_setscheduler, -1, SCHED_OTHER, &p0);
    SYS("sched_setscheduler(kernel address)", SYS_sched_setscheduler, 0, SCHED_OTHER, KERNEL_ADDR);
    SYS("sched_setscheduler(kernel address, no such pid)", SYS_sched_setscheduler, NOBODY, SCHED_OTHER, KERNEL_ADDR);
    SYS("sched_setscheduler(no such pid)", SYS_sched_setscheduler, NOBODY, SCHED_OTHER, &p0);
    SYS("sched_setscheduler(policy 4)", SYS_sched_setscheduler, 0, 4, &p0);
    SYS("sched_setscheduler(policy 8)", SYS_sched_setscheduler, 0, 8, &p0);
    SYS("sched_setscheduler(policy 4 | RESET_ON_FORK)", SYS_sched_setscheduler, 0, 4 | SCHED_RESET_ON_FORK, &p0);
    SYS("sched_setscheduler(SCHED_DEADLINE)", SYS_sched_setscheduler, 0, 6, &p0);
    SYS("sched_setscheduler(SCHED_FIFO, 0)", SYS_sched_setscheduler, 0, SCHED_FIFO, &p0);
    SYS("sched_setscheduler(SCHED_FIFO, 100)", SYS_sched_setscheduler, 0, SCHED_FIFO, &p100);
    SYS("sched_setscheduler(SCHED_FIFO, -1)", SYS_sched_setscheduler, 0, SCHED_FIFO, &pm1);
    SYS("sched_setscheduler(SCHED_OTHER, 1)", SYS_sched_setscheduler, 0, SCHED_OTHER, &p1);
    SYS("sched_setscheduler(SCHED_FIFO, 1) without RLIMIT_RTPRIO", SYS_sched_setscheduler, 0, SCHED_FIFO, &p1);
    SYS("sched_setscheduler(SCHED_RR, 50) without RLIMIT_RTPRIO", SYS_sched_setscheduler, 0, SCHED_RR, &p50);
    SYS("sched_setscheduler(SCHED_EXT, 0)", SYS_sched_setscheduler, 0, 7, &p0);
    SYS("sched_getscheduler", SYS_sched_getscheduler, 0);
    SYS("sched_setscheduler(SCHED_OTHER, 0)", SYS_sched_setscheduler, 0, SCHED_OTHER, &p0);
    SYS("sched_getscheduler", SYS_sched_getscheduler, 0);
    SYS("sched_setscheduler(SCHED_BATCH, 0)", SYS_sched_setscheduler, 0, SCHED_BATCH, &p0);
    SYS("sched_getscheduler", SYS_sched_getscheduler, 0);
    SYS("sched_setparam(0)", SYS_sched_setparam, 0, &p0);
    SYS("sched_setparam(1)", SYS_sched_setparam, 0, &p1);
    SYS("sched_setparam(NULL)", SYS_sched_setparam, 0, NULL);
    SYS("sched_setparam(no such pid)", SYS_sched_setparam, NOBODY, &p0);
    /* SC-7a fixer: the argument checks around the real-time policies, the same with or without a grant. */
    SYS("sched_setparam(-1)", SYS_sched_setparam, -1, &p0);
    SYS("sched_setparam(kernel address)", SYS_sched_setparam, 0, KERNEL_ADDR);
    SYS("sched_setparam(-1 priority)", SYS_sched_setparam, 0, &pm1);
    SYS("sched_setscheduler(SCHED_RR, 0)", SYS_sched_setscheduler, 0, SCHED_RR, &p0);
    SYS("sched_setscheduler(SCHED_RR, 100)", SYS_sched_setscheduler, 0, SCHED_RR, &p100);
    SYS("sched_setscheduler(SCHED_IDLE, 1)", SYS_sched_setscheduler, 0, SCHED_IDLE, &p1);
    SYS("sched_setscheduler(SCHED_FIFO | RESET_ON_FORK, 1) without RLIMIT_RTPRIO", SYS_sched_setscheduler, 0, SCHED_FIFO | SCHED_RESET_ON_FORK, &p1);
    SYS("sched_getscheduler", SYS_sched_getscheduler, 0);
    SYS("sched_setscheduler(SCHED_BATCH | RESET_ON_FORK)", SYS_sched_setscheduler, 0, SCHED_BATCH | SCHED_RESET_ON_FORK, &p0);
    SYS("sched_getscheduler", SYS_sched_getscheduler, 0);
    SYS("sched_setparam(0) keeps the flag", SYS_sched_setparam, 0, &p0);
    SYS("sched_getscheduler", SYS_sched_getscheduler, 0);
    SYS("sched_setscheduler(SCHED_BATCH): clearing the flag", SYS_sched_setscheduler, 0, SCHED_BATCH, &p0);
    SYS("sched_setscheduler(SCHED_IDLE | RESET_ON_FORK)", SYS_sched_setscheduler, 0, SCHED_IDLE | SCHED_RESET_ON_FORK, &p0);
    SYS("sched_getscheduler", SYS_sched_getscheduler, 0);
    out = 77;
    SYS("sched_getparam under SCHED_IDLE", SYS_sched_getparam, 0, &out);
    printf("  priority %d\n", out);
    SYS("sched_setparam(1) under SCHED_IDLE", SYS_sched_setparam, 0, &p1);
    SYS("sched_setscheduler(SCHED_OTHER | RESET_ON_FORK): leaving SCHED_IDLE at nice 19", SYS_sched_setscheduler, 0, SCHED_OTHER | SCHED_RESET_ON_FORK, &p0);
    SYS("sched_getscheduler", SYS_sched_getscheduler, 0);
    SYS("getpriority stays", SYS_getpriority, PRIO_PROCESS, 0);
    static const int pols[] = { 0, 1, 2, 3, 5, 6, 7, 4, 8, -1 };
    char label[64];
    for (unsigned i = 0; i < sizeof pols / sizeof pols[0]; i++) {
        snprintf(label, sizeof label, "sched_get_priority_max(%d)", pols[i]);
        SYS(label, SYS_sched_get_priority_max, pols[i]);
        snprintf(label, sizeof label, "sched_get_priority_min(%d)", pols[i]);
        SYS(label, SYS_sched_get_priority_min, pols[i]);
    }
}

static void cputimes(void) {
    struct tms t;
    long r = syscall(SYS_times, NULL);
    yes("times(NULL) > 0", r > 0);
    long r2 = syscall(SYS_times, &t);
    yes("times(buf) >= times(NULL)", r2 >= r);
    yes("  no children's time", t.tms_cutime == 0 && t.tms_cstime == 0);
    SYS("times(kernel address)", SYS_times, KERNEL_ADDR);
    struct rusage u;
    SYS("getrusage(2)", SYS_getrusage, 2, &u);
    SYS("getrusage(-2)", SYS_getrusage, -2, &u);
    SYS("getrusage(kernel address)", SYS_getrusage, RUSAGE_SELF, KERNEL_ADDR);
    memset(&u, 0x55, sizeof u);
    SYS("getrusage(RUSAGE_CHILDREN)", SYS_getrusage, RUSAGE_CHILDREN, &u);
    static const struct rusage zero;
    yes("  all zero without children", !memcmp(&u, &zero, 144)); /* the kernel's part; musl adds __reserved */
    SYS("getrusage(RUSAGE_SELF)", SYS_getrusage, RUSAGE_SELF, &u);
    yes("  some CPU time, usec below 1e6", tv_ns(u.ru_utime) + tv_ns(u.ru_stime) > 0 && u.ru_utime.tv_usec < 1000000 && u.ru_stime.tv_usec < 1000000);
    yes("  ru_maxrss > 0", u.ru_maxrss > 0);
    struct rusage th, me;
    memset(&th, 0x55, sizeof th);
    SYS("getrusage(RUSAGE_THREAD)", SYS_getrusage, RUSAGE_THREAD, &th);
    syscall(SYS_getrusage, RUSAGE_SELF, &me);
    yes("  usec below 1e6, some CPU time, ru_maxrss > 0", th.ru_utime.tv_usec < 1000000 && th.ru_stime.tv_usec < 1000000 && tv_ns(th.ru_utime) + tv_ns(th.ru_stime) > 0 && th.ru_maxrss > 0);
    /* One thread: its time is the process's (read later; each field is cut to whole us, so 2 us of slack). */
    yes("  at most RUSAGE_SELF's time and switches", tv_ns(th.ru_utime) + tv_ns(th.ru_stime) <= tv_ns(me.ru_utime) + tv_ns(me.ru_stime) + 2000 && th.ru_nvcsw <= me.ru_nvcsw && th.ru_nivcsw <= me.ru_nivcsw);
    yes("  page-fault, block-I/O and IPC fields of a thread: no 0x55 left", th.ru_ixrss != 0x5555555555555555l && th.ru_nswap != 0x5555555555555555l && th.ru_msgsnd != 0x5555555555555555l);
    struct timespec ts;
    char label[96];
    static const int self[] = { 2, 3, PROCESS_CLOCK(0, CPUCLOCK_PROF), PROCESS_CLOCK(0, CPUCLOCK_VIRT), PROCESS_CLOCK(0, CPUCLOCK_SCHED), THREAD_CLOCK(0, CPUCLOCK_PROF), THREAD_CLOCK(0, CPUCLOCK_VIRT), THREAD_CLOCK(0, CPUCLOCK_SCHED) };
    for (unsigned i = 0; i < sizeof self / sizeof self[0]; i++) {
        snprintf(label, sizeof label, "clock_getres(%d)", self[i]);
        if (SYS(label, SYS_clock_getres, self[i], &ts) == 0)
            printf("  resolution %ld ns\n", ns(ts));
        snprintf(label, sizeof label, "clock_gettime(%d)", self[i]);
        SYS(label, SYS_clock_gettime, self[i], &ts);
    }
    struct timespec prof, virt;
    syscall(SYS_clock_gettime, PROCESS_CLOCK(0, CPUCLOCK_PROF), &prof);
    syscall(SYS_clock_gettime, PROCESS_CLOCK(0, CPUCLOCK_VIRT), &virt);
    yes("VIRT <= PROF", ns(virt) <= ns(prof));
    SYS("clock_gettime(own thread clock by pid)", SYS_clock_gettime, THREAD_CLOCK(getpid(), CPUCLOCK_SCHED), &ts);
    SYS("clock_gettime(own process clock by pid)", SYS_clock_gettime, PROCESS_CLOCK(getpid(), CPUCLOCK_SCHED), &ts);
    SYS("clock_gettime(process clock of no such pid)", SYS_clock_gettime, PROCESS_CLOCK(NOBODY, CPUCLOCK_SCHED), &ts);
    SYS("clock_gettime(thread clock of no such pid)", SYS_clock_gettime, THREAD_CLOCK(NOBODY, CPUCLOCK_SCHED), &ts);
    SYS("clock_getres(process clock of no such pid)", SYS_clock_getres, PROCESS_CLOCK(NOBODY, CPUCLOCK_SCHED), &ts);
    struct timespec z = { 0, 0 }, one = { 0, 1 };
    /* Linux samples PROF/VIRT by ticks: a process that ran less than a tick has PROF 0, and an absolute sleep
     * until 1 ns of it would never end (its only thread sleeps). 20 ms of CPU time first. */
    struct timespec t0, t1;
    syscall(SYS_clock_gettime, 3, &t0);
    volatile unsigned long x = 1;
    do {
        for (int i = 0; i < 100000; i++)
            x = x * 6364136223846793005ul + 1;
        syscall(SYS_clock_gettime, 3, &t1);
    } while (ns(t1) - ns(t0) < 20000000l);
    SYS("clock_nanosleep(CLOCK_THREAD_CPUTIME_ID)", SYS_clock_nanosleep, 3, 0, &z, 0);
    SYS("clock_nanosleep(own thread clock)", SYS_clock_nanosleep, THREAD_CLOCK(0, CPUCLOCK_SCHED), 0, &z, 0);
    SYS("clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, 0)", SYS_clock_nanosleep, 2, 0, &z, 0);
    SYS("clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, ABSTIME, 1 ns)", SYS_clock_nanosleep, 2, 1, &one, 0);
    SYS("clock_nanosleep(PROF clock, ABSTIME, 1 ns)", SYS_clock_nanosleep, PROCESS_CLOCK(0, CPUCLOCK_PROF), 1, &one, 0);
    SYS("clock_nanosleep(process clock of no such pid)", SYS_clock_nanosleep, PROCESS_CLOCK(NOBODY, CPUCLOCK_SCHED), 0, &z, 0);
}

static long thread_cpu(void) {
    struct timespec t;
    syscall(SYS_clock_gettime, 3, &t);
    return ns(t);
}

static long self_ns(int user) {
    struct rusage u;
    syscall(SYS_getrusage, RUSAGE_SELF, &u);
    return user ? tv_ns(u.ru_utime) : tv_ns(u.ru_stime);
}

static void burn(void) {
    const long MS = 1000000;
    long u0 = self_ns(1), s0 = self_ns(0), c0 = thread_cpu();
    volatile unsigned long x = 1;
    while (thread_cpu() - c0 < 300 * MS)
        for (int i = 0; i < 2000000; i++)
            x = x * 6364136223846793005ul + 1442695040888963407ul;
    long u1 = self_ns(1), s1 = self_ns(0), c1 = thread_cpu();
    yes("user loop: user time grew by >= 250 ms", u1 - u0 >= 250 * MS);
    yes("user loop: system time grew by < 100 ms", s1 - s0 < 100 * MS);
    while (thread_cpu() - c1 < 300 * MS)
        for (int i = 0; i < 1000; i++)
            syscall(SYS_getppid);
    long u2 = self_ns(1), s2 = self_ns(0);
    yes("syscall loop: system time grew by >= 30 ms", s2 - s1 >= 30 * MS);
    yes("syscall loop: user + system grew by >= 250 ms", (u2 - u1) + (s2 - s1) >= 250 * MS);
    struct tms t;
    syscall(SYS_times, &t);
    yes("times: tms_utime >= 25 ticks", t.tms_utime >= 25);
    yes("times: tms_utime + tms_stime >= 55 ticks", t.tms_utime + t.tms_stime >= 55);
    struct timespec p, v, s;
    syscall(SYS_clock_gettime, PROCESS_CLOCK(0, CPUCLOCK_PROF), &p);
    syscall(SYS_clock_gettime, PROCESS_CLOCK(0, CPUCLOCK_VIRT), &v);
    syscall(SYS_clock_gettime, 2, &s);
    yes("CPU clocks: VIRT >= 250 ms", ns(v) >= 250 * MS);
    yes("CPU clocks: PROF >= 550 ms and >= VIRT", ns(p) >= 550 * MS && ns(p) >= ns(v));
    yes("CPU clocks: PROCESS_CPUTIME >= PROF - 50 ms", ns(s) + 50 * MS >= ns(p));
    struct rusage th;
    syscall(SYS_getrusage, RUSAGE_THREAD, &th);
    yes("RUSAGE_THREAD: user >= 250 ms", tv_ns(th.ru_utime) >= 250 * MS);
}

static int popcount(const unsigned char *m, int n) {
    int c = 0;
    for (int i = 0; i < n; i++)
        c += __builtin_popcount(m[i]);
    return c;
}

static void child(void) {
    int last = (int)sysconf(_SC_NPROCESSORS_ONLN) - 1;
    unsigned char m[128] = { 0 }, g[128];
    int p0 = 0, pfd[2];
    if (pipe(pfd)) {
        printf("pipe = -%s\n", ename(errno));
        return;
    }
    m[0] = 1;
    SYS("sched_setaffinity(CPU 0)", SYS_sched_setaffinity, 0, 128, m);
    SYS("setpriority(PRIO_PROCESS, 0, 3)", SYS_setpriority, PRIO_PROCESS, 0, 3);
    SYS("sched_setscheduler(SCHED_BATCH, 0)", SYS_sched_setscheduler, 0, SCHED_BATCH, &p0);
    pid_t c = fork();
    if (c < 0) {
        printf("fork = -%s\n", ename(errno));
        return;
    }
    if (c == 0) {
        char b;
        /* The parent changes this process's attributes, then lets it look. */
        if (read(pfd[0], &b, 1) != 1)
            _exit(100);
        memset(g, 0, sizeof g);
        syscall(SYS_sched_getaffinity, 0, 128, g);
        int mask = popcount(g, 128) == 1 && (g[last / 8] >> (last % 8)) & 1;
        long prio = syscall(SYS_getpriority, PRIO_PROCESS, 0);
        long pol = syscall(SYS_sched_getscheduler, 0);
        volatile unsigned long x = 1;
        for (long i = 0; i < 20000000; i++) /* some user time for the parent's rusage */
            x = x * 6364136223846793005ul + 1;
        _exit((mask ? 1 : 0) | (prio == 10 ? 2 : 0) | (pol == SCHED_BATCH ? 4 : 0));
    }
    memset(g, 0, sizeof g);
    syscall(SYS_sched_getaffinity, c, 128, g);
    yes("child inherited the mask (CPU 0 only)", popcount(g, 128) == 1 && g[0] == 1);
    SYS("getpriority(child) (nice 3 inherited)", SYS_getpriority, PRIO_PROCESS, c);
    SYS("sched_getscheduler(child)", SYS_sched_getscheduler, c);
    memset(m, 0, sizeof m);
    m[last / 8] = (unsigned char)(1u << (last % 8));
    SYS("sched_setaffinity(child, last CPU)", SYS_sched_setaffinity, c, 128, m);
    memset(g, 0, sizeof g);
    syscall(SYS_sched_getaffinity, c, 128, g);
    yes("  read back: the last CPU only", popcount(g, 128) == 1 && !memcmp(g, m, 128));
    SYS("setpriority(PRIO_PROCESS, child, 10)", SYS_setpriority, PRIO_PROCESS, c, 10);
    SYS("getpriority(child)", SYS_getpriority, PRIO_PROCESS, c);
    SYS("setpriority(PRIO_PROCESS, child, 2): lowering without RLIMIT_NICE", SYS_setpriority, PRIO_PROCESS, c, 2);
    SYS("getpriority(own, unchanged)", SYS_getpriority, PRIO_PROCESS, 0);
    if (write(pfd[1], "x", 1) != 1)
        printf("write failed\n");
    int st = 0;
    struct rusage ru;
    memset(&ru, 0x55, sizeof ru);
    yes("wait4 collects the child", syscall(SYS_wait4, c, &st, 0, &ru) == c);
    yes("  it exited", WIFEXITED(st));
    int code = WEXITSTATUS(st);
    yes("  child saw its new mask", code & 1);
    yes("  child saw nice 10", code & 2);
    yes("  child kept SCHED_BATCH", code & 4);
    yes("  wait4 rusage: usec below 1e6, user time > 0", ru.ru_utime.tv_usec < 1000000 && ru.ru_stime.tv_usec < 1000000 && tv_ns(ru.ru_utime) > 0);
    struct rusage ch;
    SYS("getrusage(RUSAGE_CHILDREN)", SYS_getrusage, RUSAGE_CHILDREN, &ch);
    yes("  equal to wait4's times", tv_ns(ch.ru_utime) == tv_ns(ru.ru_utime) && tv_ns(ch.ru_stime) == tv_ns(ru.ru_stime));
    struct tms t;
    syscall(SYS_times, &t);
    yes("times: children's ticks match", t.tms_cutime == tv_ns(ru.ru_utime) / 10000000 && t.tms_cstime == tv_ns(ru.ru_stime) / 10000000);
    SYS("getpriority(reaped child)", SYS_getpriority, PRIO_PROCESS, c);
    SYS("sched_getaffinity(reaped child)", SYS_sched_getaffinity, c, 128, g);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc > 1 && !strcmp(argv[1], "burn")) {
        burn();
        return 0;
    }
    if (argc > 1 && !strcmp(argv[1], "child")) {
        limits();
        child();
        return 0;
    }
    limits();
    affinity();
    getcpu();
    priority();
    policy();
    cputimes();
    return 0;
}
