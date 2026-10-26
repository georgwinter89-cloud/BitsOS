/* abi-ab5: the wave-B rest of AB-5 as programs see it, for the ABI diff (CachyOS is the oracle). Prints
 * results, errno names and yes/no relations only (no pids, addresses, absolute times or host counts).
 * Modes (argv[1]):
 *   timerfd   timerfd_create/settime/gettime: clocks, flags, errors, one-shot, periodic (missed periods
 *             counted at read), absolute times, poll/epoll readiness (EPOLLET: one edge per expiry), blocking
 *             read, TFD_IOC_SET_TICKS, what the fd is (F_GETFL, fstat, lseek, write, pread)
 *   prctl     names, dumpable, pdeath signal (with a child subreaper: the grandchild dies when its parent does
 *             and comes back to us), no_new_privs, timer slack, MCE kill, THP, capabilities, the LSM options,
 *             PR_GET_AUXV, PR_SET_MM's size query, errors
 *   misc      sysinfo, getrandom flags and partial copies, prlimit64/getrlimit/setrlimit rules (NOFILE
 *             enforced by open/dup/fcntl), dup/dup2/dup3, sched_yield, F_SETPIPE_SZ/F_GETPIPE_SZ (FS-3's
 *             pipes through fcntl), close_range, nice and affinity by thread id and their copy at fork
 *   flock     flock(2): shared/exclusive between open files and processes, conversion, LOCK_NB, release at
 *             close and at exit, errors
 *   fcntl     fcntl record locks between processes (F_GETLK reports the holder, F_SETLKW waits, EDEADLK,
 *             release at any close), OFD locks, F_SETOWN/F_GETOWN(_EX), F_SETSIG, leases, write hints,
 *             F_DUPFD_QUERY, F_SETFL refusals, seals on a non-memfd; the POSIX locks go at dup2/dup3 over an
 *             fd of the file (also one naming the same open file) and at close_range, not at dup2(fd, fd) */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <sched.h>
#include <sys/epoll.h>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysinfo.h>
#include <sys/timerfd.h>
#include <sys/uio.h>
#include <elf.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define KERNEL_ADDR ((void *)0xffff888000000000ul)
#ifndef TFD_IOC_SET_TICKS
#define TFD_IOC_SET_TICKS _IOW('T', 0, uint64_t)
#endif
#ifndef PR_SET_SYSCALL_USER_DISPATCH
#define PR_SET_SYSCALL_USER_DISPATCH 59
#define PR_SYS_DISPATCH_OFF 0
#endif
#ifndef PR_GET_AUXV
#define PR_GET_AUXV 0x41555856
#endif
#ifndef PR_SET_IO_FLUSHER
#define PR_SET_IO_FLUSHER 57
#define PR_GET_IO_FLUSHER 58
#endif
#ifndef F_GET_RW_HINT
#define F_GET_RW_HINT 1035
#define F_SET_RW_HINT 1036
#endif
#ifndef F_ADD_SEALS
#define F_ADD_SEALS 1033
#define F_GET_SEALS 1034
#define F_SEAL_SEAL 1
#endif
#ifndef F_OFD_GETLK
#define F_OFD_GETLK 36
#define F_OFD_SETLK 37
#define F_OFD_SETLKW 38
#endif
#ifndef F_SETPIPE_SZ
#define F_SETPIPE_SZ 1031
#define F_GETPIPE_SZ 1032
#endif
#ifndef SYS_close_range
#define SYS_close_range 436
#endif
#ifndef CLOSE_RANGE_UNSHARE
#define CLOSE_RANGE_UNSHARE 2
#define CLOSE_RANGE_CLOEXEC 4
#endif
#define MS 1000000l

static const char *ename(int e) {
    static char other[24];
    switch (e) {
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case ESRCH: return "ESRCH";
    case EINTR: return "EINTR";
    case ENXIO: return "ENXIO";
    case EBADF: return "EBADF";
    case ECHILD: return "ECHILD";
    case EAGAIN: return "EAGAIN";
    case ENOMEM: return "ENOMEM";
    case EACCES: return "EACCES";
    case EFAULT: return "EFAULT";
    case EBUSY: return "EBUSY";
    case EEXIST: return "EEXIST";
    case ENOTDIR: return "ENOTDIR";
    case EINVAL: return "EINVAL";
    case EMFILE: return "EMFILE";
    case ENOTTY: return "ENOTTY";
    case ESPIPE: return "ESPIPE";
    case ERANGE: return "ERANGE";
    case EDEADLK: return "EDEADLK";
    case ENOLCK: return "ENOLCK";
    case ENOSYS: return "ENOSYS";
    case ENODEV: return "ENODEV";
    case EOVERFLOW: return "EOVERFLOW";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case ECANCELED: return "ECANCELED";
    }
    snprintf(other, sizeof other, "errno %d", e);
    return other;
}

static long show(const char *label, long r) {
    if (r == -1)
        printf("%s = -%s\n", label, ename(errno));
    else
        printf("%s = %ld\n", label, r);
    return r;
}
#define SYS(label, ...) show(label, syscall(__VA_ARGS__))

static void yes(const char *label, int c) {
    printf("%s: %s\n", label, c ? "yes" : "no");
}

static long ts_ns(struct timespec t) {
    return t.tv_sec * 1000000000l + t.tv_nsec;
}

static long now_ns(clockid_t c) {
    struct timespec t;
    clock_gettime(c, &t);
    return ts_ns(t);
}

static void sleep_ms(long ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * MS};
    while (nanosleep(&t, &t) < 0 && errno == EINTR)
        ;
}

static struct itimerspec its(long value_ns, long interval_ns) {
    struct itimerspec s;
    s.it_value.tv_sec = value_ns / 1000000000l;
    s.it_value.tv_nsec = value_ns % 1000000000l;
    s.it_interval.tv_sec = interval_ns / 1000000000l;
    s.it_interval.tv_nsec = interval_ns % 1000000000l;
    return s;
}

static int readable(int fd, int timeout_ms) {
    struct pollfd p = {fd, POLLIN | POLLOUT, 0};
    int r = poll(&p, 1, timeout_ms);
    return r < 0 ? -errno : p.revents;
}

static long read_ticks(int fd) {
    uint64_t t = 0;
    ssize_t n = read(fd, &t, sizeof t);
    if (n < 0)
        return -errno;
    return n == 8 ? (long)t : -1000 - n;
}

static void ticks(const char *label, long t) {
    if (t < 0)
        printf("%s = -%s\n", label, ename((int)-t));
    else
        printf("%s = %ld\n", label, t);
}

/* ---- timerfd ------------------------------------------------------------------------------------------ */

static void timerfd_errors(void) {
    SYS("create(99)", SYS_timerfd_create, 99, 0);
    SYS("create(MONOTONIC, bad flags)", SYS_timerfd_create, CLOCK_MONOTONIC, 1);
    SYS("create(PROCESS_CPUTIME)", SYS_timerfd_create, CLOCK_PROCESS_CPUTIME_ID, 0);
    SYS("create(MONOTONIC_RAW)", SYS_timerfd_create, CLOCK_MONOTONIC_RAW, 0);
    SYS("create(REALTIME_ALARM) as a user", SYS_timerfd_create, CLOCK_REALTIME_ALARM, 0);
    SYS("create(BOOTTIME_ALARM) as a user", SYS_timerfd_create, CLOCK_BOOTTIME_ALARM, 0);
    int fd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC);
    yes("create(MONOTONIC, NONBLOCK|CLOEXEC) ok", fd >= 0);
    show("  F_GETFD", fcntl(fd, F_GETFD));
    printf("  F_GETFL = %o\n", fcntl(fd, F_GETFL));
    struct stat st;
    fstat(fd, &st);
    printf("  fstat mode = %o\n", st.st_mode);
    int efd = syscall(SYS_eventfd2, 0, 0);
    struct stat st2;
    fstat(efd, &st2);
    yes("  the same anonymous inode as an eventfd", st.st_ino == st2.st_ino && st.st_dev == st2.st_dev);
    char b[8];
    show("  read 4 bytes", read(fd, b, 4));
    show("  read unarmed (nonblocking)", read(fd, b, 8));
    show("  write", write(fd, b, 8));
    show("  lseek", lseek(fd, 5, SEEK_SET));
    show("  pread", pread(fd, b, 8, 0));
    show("  ioctl(FIONREAD)", ioctl(fd, FIONREAD, b));
    struct itimerspec cur;
    show("  gettime unarmed", timerfd_gettime(fd, &cur));
    yes("  all zero", cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec == 0 && cur.it_interval.tv_sec == 0 && cur.it_interval.tv_nsec == 0);
    struct itimerspec v = its(10 * MS, 0);
    SYS("settime(bad flags 4)", SYS_timerfd_settime, fd, 4, &v, NULL);
    struct itimerspec bad = its(0, 0);
    bad.it_value.tv_nsec = 1000000000l;
    SYS("settime(tv_nsec 1e9)", SYS_timerfd_settime, fd, 0, &bad, NULL);
    bad = its(0, 0);
    bad.it_interval.tv_nsec = -1;
    SYS("settime(interval nsec -1)", SYS_timerfd_settime, fd, 0, &bad, NULL);
    bad = its(0, 0);
    bad.it_value.tv_sec = -1;
    SYS("settime(value sec -1)", SYS_timerfd_settime, fd, 0, &bad, NULL);
    SYS("settime(new = kernel address)", SYS_timerfd_settime, fd, 0, KERNEL_ADDR, NULL);
    SYS("settime(new = NULL)", SYS_timerfd_settime, fd, 0, NULL, NULL);
    SYS("settime(fd -1)", SYS_timerfd_settime, -1, 0, &v, NULL);
    SYS("settime(fd 999)", SYS_timerfd_settime, 999, 0, &v, NULL);
    SYS("settime(an eventfd)", SYS_timerfd_settime, efd, 0, &v, NULL);
    SYS("settime(bad flags, eventfd)", SYS_timerfd_settime, efd, 4, &v, NULL);
    SYS("gettime(an eventfd)", SYS_timerfd_gettime, efd, &cur);
    SYS("gettime(fd -1)", SYS_timerfd_gettime, -1, &cur);
    SYS("gettime(kernel address)", SYS_timerfd_gettime, fd, KERNEL_ADDR);
    struct itimerspec h = its(3600l * 1000000000l, 0);
    SYS("settime(1 h, old = kernel address)", SYS_timerfd_settime, fd, 0, &h, KERNEL_ADDR);
    timerfd_gettime(fd, &cur);
    yes("  the timer was set anyway", cur.it_value.tv_sec > 3500);
    struct itimerspec zero = its(0, 5 * MS);
    struct itimerspec old;
    SYS("settime(value 0, interval 5 ms, &old)", SYS_timerfd_settime, fd, 0, &zero, &old);
    yes("  old: about 1 h left, no interval", old.it_value.tv_sec > 3500 && old.it_value.tv_sec <= 3600 && old.it_interval.tv_sec == 0 && old.it_interval.tv_nsec == 0);
    timerfd_gettime(fd, &cur);
    yes("  disarmed: value 0, interval kept", cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec == 0 && cur.it_interval.tv_nsec == 5 * MS);
    ticks("  read", read_ticks(fd));
    show("  ioctl(unknown 0x5400)", ioctl(fd, 0x5400, 0));
    close(efd);
    close(fd);
}

static void timerfd_oneshot(void) {
    int fd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    struct itimerspec v = its(40 * MS, 0), cur;
    show("one-shot 40 ms", timerfd_settime(fd, 0, &v, NULL));
    timerfd_gettime(fd, &cur);
    yes("  gettime: in (0, 40 ms], no interval", ts_ns(cur.it_value) > 0 && ts_ns(cur.it_value) <= 40 * MS && ts_ns(cur.it_interval) == 0);
    printf("  poll at once: %#x\n", readable(fd, 0));
    printf("  poll (5 s): %#x\n", readable(fd, 5000));
    timerfd_gettime(fd, &cur);
    yes("  gettime after: value 0", ts_ns(cur.it_value) == 0);
    ticks("  read", read_ticks(fd));
    ticks("  read again", read_ticks(fd));
    printf("  poll after the read: %#x\n", readable(fd, 0));
    /* Absolute times on CLOCK_MONOTONIC: in the future, in the past, and 0 (= disarm, even with ABSTIME). */
    long t0 = now_ns(CLOCK_MONOTONIC);
    v = its(t0 + 30 * MS, 0);
    show("absolute +30 ms", timerfd_settime(fd, TFD_TIMER_ABSTIME, &v, NULL));
    printf("  poll (5 s): %#x\n", readable(fd, 5000));
    yes("  not before the time", now_ns(CLOCK_MONOTONIC) >= t0 + 30 * MS);
    ticks("  read", read_ticks(fd));
    v = its(1, 0);
    show("absolute 1 ns (past)", timerfd_settime(fd, TFD_TIMER_ABSTIME, &v, NULL));
    printf("  poll (5 s): %#x\n", readable(fd, 5000));
    ticks("  read", read_ticks(fd));
    v = its(0, 0);
    show("absolute 0", timerfd_settime(fd, TFD_TIMER_ABSTIME, &v, NULL));
    ticks("  read", read_ticks(fd));
    /* Re-arming before expiry replaces the old setting and its ticks. */
    v = its(1 * MS, 0);
    timerfd_settime(fd, 0, &v, NULL);
    readable(fd, 5000);
    v = its(3600l * 1000000000l, 0);
    timerfd_settime(fd, 0, &v, NULL);
    ticks("expired, then re-armed for 1 h: read", read_ticks(fd));
    close(fd);
}

static void timerfd_periodic(void) {
    int fd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    struct itimerspec v = its(10 * MS, 10 * MS), cur;
    long start = now_ns(CLOCK_MONOTONIC);
    show("periodic 10 ms", timerfd_settime(fd, 0, &v, NULL));
    sleep_ms(105);
    long t = read_ticks(fd);
    long elapsed = now_ns(CLOCK_MONOTONIC) - start;
    yes("  after 105 ms: at least 5 expiries in one read", t >= 5);
    yes("  and no more than the periods that passed", t <= elapsed / (10 * MS) + 1);
    timerfd_gettime(fd, &cur);
    yes("  gettime: next in (0, 10 ms], interval 10 ms", ts_ns(cur.it_value) > 0 && ts_ns(cur.it_value) <= 10 * MS && ts_ns(cur.it_interval) == 10 * MS);
    sleep_ms(35);
    timerfd_gettime(fd, &cur);
    yes("  gettime of an expired periodic timer moves it on", ts_ns(cur.it_value) > 0 && ts_ns(cur.it_value) <= 10 * MS);
    t = read_ticks(fd);
    yes("  the periods gettime saw are still counted (>= 3)", t >= 3);
    struct itimerspec old;
    v = its(0, 0);
    timerfd_settime(fd, 0, &v, &old);
    yes("  settime returns the old interval", ts_ns(old.it_interval) == 10 * MS);
    ticks("  read after the stop", read_ticks(fd));
    close(fd);
}

static void timerfd_clocks(void) {
    static const struct { int id; const char *name; } cl[] = {
        {CLOCK_REALTIME, "REALTIME"}, {CLOCK_BOOTTIME, "BOOTTIME"}, {CLOCK_MONOTONIC, "MONOTONIC"}};
    for (unsigned i = 0; i < sizeof cl / sizeof cl[0]; i++) {
        char label[64];
        int fd = timerfd_create(cl[i].id, TFD_NONBLOCK);
        snprintf(label, sizeof label, "%s relative 20 ms", cl[i].name);
        struct itimerspec v = its(20 * MS, 0);
        show(label, timerfd_settime(fd, 0, &v, NULL));
        printf("  poll (5 s): %#x\n", readable(fd, 5000));
        ticks("  read", read_ticks(fd));
        snprintf(label, sizeof label, "%s absolute +20 ms", cl[i].name);
        v = its(now_ns(cl[i].id) + 20 * MS, 0);
        show(label, timerfd_settime(fd, TFD_TIMER_ABSTIME, &v, NULL));
        printf("  poll (5 s): %#x\n", readable(fd, 5000));
        ticks("  read", read_ticks(fd));
        snprintf(label, sizeof label, "%s ABSTIME|CANCEL_ON_SET +1 h", cl[i].name);
        v = its(now_ns(cl[i].id) + 3600l * 1000000000l, 0);
        show(label, timerfd_settime(fd, TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET, &v, NULL));
        ticks("  read", read_ticks(fd));
        snprintf(label, sizeof label, "%s CANCEL_ON_SET without ABSTIME", cl[i].name);
        v = its(3600l * 1000000000l, 0);
        show(label, timerfd_settime(fd, TFD_TIMER_CANCEL_ON_SET, &v, NULL));
        close(fd);
    }
}

static void timerfd_waits(void) {
    /* A blocking read waits for the expiry. */
    int fd = timerfd_create(CLOCK_MONOTONIC, 0);
    struct itimerspec v = its(30 * MS, 0);
    timerfd_settime(fd, 0, &v, NULL);
    long t0 = now_ns(CLOCK_MONOTONIC);
    ticks("blocking read of a 30 ms timer", read_ticks(fd));
    yes("  it waited", now_ns(CLOCK_MONOTONIC) - t0 >= 25 * MS);
    /* readv takes the whole iovec (read_iter since Linux 6.8): 4 + 4 bytes hold the count, 2 + 4 do not. */
    uint32_t lo = 9, hi = 9;
    char two[2];
    struct iovec iov[2] = {{&lo, sizeof lo}, {&hi, sizeof hi}};
    struct iovec iov2[2] = {{two, sizeof two}, {&hi, sizeof hi}};
    v = its(1, 0);
    timerfd_settime(fd, 0, &v, NULL);
    readable(fd, 5000);
    show("readv(2 + 4 bytes)", readv(fd, iov2, 2));
    show("readv(4 + 4 bytes)", readv(fd, iov, 2));
    yes("  the count split over both: 1 and 0", lo == 1 && hi == 0);
    close(fd);

    /* epoll, edge-triggered: one edge per expiry; a periodic timer that is not read gives no new edge. */
    int ep = epoll_create1(0);
    fd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    struct epoll_event e = {.events = EPOLLIN | EPOLLET, .data.u64 = 7};
    show("epoll_ctl(ADD, EPOLLIN|EPOLLET)", epoll_ctl(ep, EPOLL_CTL_ADD, fd, &e));
    struct epoll_event got[2];
    show("  epoll_wait before arming (0 ms)", epoll_wait(ep, got, 2, 0));
    v = its(10 * MS, 10 * MS);
    timerfd_settime(fd, 0, &v, NULL);
    int n = epoll_wait(ep, got, 2, 5000);
    printf("  epoll_wait (5 s): %d events %#x data %lu\n", n, n > 0 ? got[0].events : 0, n > 0 ? (unsigned long)got[0].data.u64 : 0);
    sleep_ms(50);
    show("  50 ms later, nothing read: epoll_wait (0 ms)", epoll_wait(ep, got, 2, 0));
    long t = read_ticks(fd);
    yes("  read: several expiries", t >= 3);
    n = epoll_wait(ep, got, 2, 5000);
    printf("  after the read the next expiry is an edge again: %d events %#x\n", n, n > 0 ? got[0].events : 0);
    v = its(0, 0);
    timerfd_settime(fd, 0, &v, NULL);
    close(fd);
    close(ep);

    /* TFD_IOC_SET_TICKS (CONFIG_CHECKPOINT_RESTORE). */
    fd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
    uint64_t five = 5, none = 0;
    show("ioctl(TFD_IOC_SET_TICKS, 5)", ioctl(fd, TFD_IOC_SET_TICKS, &five));
    printf("  poll: %#x\n", readable(fd, 0));
    ticks("  read", read_ticks(fd));
    show("ioctl(TFD_IOC_SET_TICKS, 0)", ioctl(fd, TFD_IOC_SET_TICKS, &none));
    show("ioctl(TFD_IOC_SET_TICKS, kernel address)", ioctl(fd, TFD_IOC_SET_TICKS, KERNEL_ADDR));
    int efd = syscall(SYS_eventfd2, 0, 0);
    show("ioctl(TFD_IOC_SET_TICKS) on an eventfd", ioctl(efd, TFD_IOC_SET_TICKS, &five));
    close(efd);
    close(fd);

    /* A child inherits the timer (one open file): its expiries are read by whoever reads first. */
    fd = timerfd_create(CLOCK_MONOTONIC, 0);
    v = its(20 * MS, 0);
    timerfd_settime(fd, 0, &v, NULL);
    pid_t c = fork();
    if (c == 0) {
        _exit(read_ticks(fd) == 1 ? 0 : 1);
    }
    int st = 0;
    waitpid(c, &st, 0);
    yes("fork: the child read the shared timer's expiry", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    struct itimerspec cur;
    timerfd_gettime(fd, &cur);
    yes("  and the parent sees it expired", ts_ns(cur.it_value) == 0);
    close(fd);
}

/* ---- prctl -------------------------------------------------------------------------------------------- */

static void prctl_basics(void) {
    char name[32];
    memset(name, 'x', sizeof name);
    show("PR_SET_NAME(\"abi-ab5-thread-name-long\")", prctl(PR_SET_NAME, "abi-ab5-thread-name-long", 0, 0, 0));
    show("PR_GET_NAME", prctl(PR_GET_NAME, name, 0, 0, 0));
    printf("  name = \"%s\", byte 15 is 0: %s, byte 16 untouched: %s\n", name, name[15] == 0 ? "yes" : "no", name[16] == 'x' ? "yes" : "no");
    prctl(PR_SET_NAME, "short", 0, 0, 0);
    prctl(PR_GET_NAME, name, 0, 0, 0);
    printf("  after \"short\": \"%s\"\n", name);
    SYS("PR_SET_NAME(kernel address)", SYS_prctl, PR_SET_NAME, KERNEL_ADDR, 0, 0, 0);
    SYS("PR_GET_NAME(kernel address)", SYS_prctl, PR_GET_NAME, KERNEL_ADDR, 0, 0, 0);
    pid_t c = fork();
    if (c == 0) {
        char n[16];
        prctl(PR_GET_NAME, n, 0, 0, 0);
        _exit(strcmp(n, "short") == 0 ? 0 : 1);
    }
    int st;
    waitpid(c, &st, 0);
    yes("  a forked child has the same name", WIFEXITED(st) && WEXITSTATUS(st) == 0);

    SYS("PR_GET_DUMPABLE", SYS_prctl, PR_GET_DUMPABLE, 0, 0, 0, 0);
    SYS("PR_SET_DUMPABLE(2)", SYS_prctl, PR_SET_DUMPABLE, 2, 0, 0, 0);
    SYS("PR_SET_DUMPABLE(0)", SYS_prctl, PR_SET_DUMPABLE, 0, 0, 0, 0);
    SYS("PR_GET_DUMPABLE", SYS_prctl, PR_GET_DUMPABLE, 0, 0, 0, 0);
    c = fork();
    if (c == 0)
        _exit(prctl(PR_GET_DUMPABLE, 0, 0, 0, 0));
    waitpid(c, &st, 0);
    printf("  a forked child's dumpable: %d\n", WEXITSTATUS(st));
    SYS("PR_SET_DUMPABLE(1)", SYS_prctl, PR_SET_DUMPABLE, 1, 0, 0, 0);

    int sig = -1;
    SYS("PR_SET_PDEATHSIG(65)", SYS_prctl, PR_SET_PDEATHSIG, 65, 0, 0, 0);
    SYS("PR_SET_PDEATHSIG(64)", SYS_prctl, PR_SET_PDEATHSIG, 64, 0, 0, 0);
    SYS("PR_SET_PDEATHSIG(-1)", SYS_prctl, PR_SET_PDEATHSIG, -1l, 0, 0, 0);
    SYS("PR_SET_PDEATHSIG(SIGUSR2)", SYS_prctl, PR_SET_PDEATHSIG, SIGUSR2, 0, 0, 0);
    SYS("PR_GET_PDEATHSIG", SYS_prctl, PR_GET_PDEATHSIG, &sig, 0, 0, 0);
    printf("  signal = %d\n", sig);
    c = fork();
    if (c == 0) {
        int s = -1;
        prctl(PR_GET_PDEATHSIG, &s, 0, 0, 0);
        _exit(s);
    }
    waitpid(c, &st, 0);
    printf("  a forked child's pdeath signal: %d\n", WEXITSTATUS(st));
    SYS("PR_GET_PDEATHSIG(kernel address)", SYS_prctl, PR_GET_PDEATHSIG, KERNEL_ADDR, 0, 0, 0);
    SYS("PR_SET_PDEATHSIG(0)", SYS_prctl, PR_SET_PDEATHSIG, 0, 0, 0, 0);

    SYS("PR_GET_TIMING", SYS_prctl, PR_GET_TIMING, 0, 0, 0, 0);
    SYS("PR_SET_TIMING(1)", SYS_prctl, PR_SET_TIMING, 1, 0, 0, 0);
    SYS("PR_SET_TIMING(0)", SYS_prctl, PR_SET_TIMING, 0, 0, 0, 0);
    SYS("PR_GET_UNALIGN", SYS_prctl, PR_GET_UNALIGN, &sig, 0, 0, 0);
    SYS("PR_GET_FPEMU", SYS_prctl, PR_GET_FPEMU, &sig, 0, 0, 0);
    SYS("PR_GET_ENDIAN", SYS_prctl, PR_GET_ENDIAN, &sig, 0, 0, 0);
    SYS("PR_GET_FP_MODE", SYS_prctl, PR_GET_FP_MODE, 0, 0, 0, 0);
    SYS("PR_GET_TSC", SYS_prctl, PR_GET_TSC, &sig, 0, 0, 0);
    printf("  tsc = %d\n", sig);
    SYS("PR_SET_TSC(PR_TSC_ENABLE)", SYS_prctl, PR_SET_TSC, PR_TSC_ENABLE, 0, 0, 0);
    SYS("PR_SET_TSC(7)", SYS_prctl, PR_SET_TSC, 7, 0, 0, 0);
    SYS("PR_TASK_PERF_EVENTS_DISABLE", SYS_prctl, PR_TASK_PERF_EVENTS_DISABLE, 0, 0, 0, 0);
    SYS("PR_TASK_PERF_EVENTS_ENABLE", SYS_prctl, PR_TASK_PERF_EVENTS_ENABLE, 0, 0, 0, 0);
    SYS("PR_MPX_ENABLE_MANAGEMENT", SYS_prctl, PR_MPX_ENABLE_MANAGEMENT, 0, 0, 0, 0);
    SYS("PR_SVE_GET_VL", SYS_prctl, PR_SVE_GET_VL, 0, 0, 0, 0);
    SYS("PR_GET_TAGGED_ADDR_CTRL", SYS_prctl, PR_GET_TAGGED_ADDR_CTRL, 0, 0, 0, 0);
    SYS("unknown option 0x7fffffff", SYS_prctl, 0x7fffffff, 0, 0, 0, 0);
    SYS("option 0", SYS_prctl, 0, 0, 0, 0, 0);
    SYS("option -1", SYS_prctl, -1, 0, 0, 0, 0);
}

static void prctl_more(void) {
    SYS("PR_GET_NO_NEW_PRIVS", SYS_prctl, PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
    SYS("PR_GET_NO_NEW_PRIVS(1)", SYS_prctl, PR_GET_NO_NEW_PRIVS, 1, 0, 0, 0);
    SYS("PR_SET_NO_NEW_PRIVS(2)", SYS_prctl, PR_SET_NO_NEW_PRIVS, 2, 0, 0, 0);
    SYS("PR_SET_NO_NEW_PRIVS(1, 1)", SYS_prctl, PR_SET_NO_NEW_PRIVS, 1, 1, 0, 0);
    pid_t c = fork();
    if (c == 0) {
        /* In a child: the flag cannot be cleared again. */
        long a = syscall(SYS_prctl, PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        long b = syscall(SYS_prctl, PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
        pid_t g = fork();
        if (g == 0)
            _exit(prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0));
        int s;
        waitpid(g, &s, 0);
        _exit((a == 0 ? 1 : 0) | (b == 1 ? 2 : 0) | (WEXITSTATUS(s) == 1 ? 4 : 0));
    }
    int st;
    waitpid(c, &st, 0);
    printf("PR_SET_NO_NEW_PRIVS(1) in a child: set %s, reads 1 %s, inherited %s\n", WEXITSTATUS(st) & 1 ? "yes" : "no", WEXITSTATUS(st) & 2 ? "yes" : "no", WEXITSTATUS(st) & 4 ? "yes" : "no");
    SYS("  PR_GET_NO_NEW_PRIVS here still", SYS_prctl, PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);

    long slack0 = prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0);
    yes("PR_GET_TIMERSLACK > 0", slack0 > 0);
    SYS("PR_SET_TIMERSLACK(1000)", SYS_prctl, PR_SET_TIMERSLACK, 1000, 0, 0, 0);
    SYS("PR_GET_TIMERSLACK", SYS_prctl, PR_GET_TIMERSLACK, 0, 0, 0, 0);
    c = fork();
    if (c == 0) {
        long a = prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0);
        prctl(PR_SET_TIMERSLACK, 77, 0, 0, 0);
        prctl(PR_SET_TIMERSLACK, 0, 0, 0, 0);
        long b = prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0);
        _exit((a == 1000 ? 1 : 0) | (b == 1000 ? 2 : 0));
    }
    waitpid(c, &st, 0);
    printf("  a child starts with 1000 and resets to it: %s %s\n", WEXITSTATUS(st) & 1 ? "yes" : "no", WEXITSTATUS(st) & 2 ? "yes" : "no");
    /* arg2 is an unsigned long: only 0 resets; a value with the top bit set is stored and read back as a
     * negative long (review AB-5 finding 4, Linux's `if (arg2 <= 0)` on an unsigned). */
    SYS("PR_SET_TIMERSLACK(-1)", SYS_prctl, PR_SET_TIMERSLACK, -1l, 0, 0, 0);
    printf("  PR_GET_TIMERSLACK raw = %ld\n", syscall(SYS_prctl, PR_GET_TIMERSLACK, 0, 0, 0, 0));
    SYS("PR_SET_TIMERSLACK(1 << 63)", SYS_prctl, PR_SET_TIMERSLACK, 1ul << 63, 0, 0, 0);
    printf("  PR_GET_TIMERSLACK raw = %ld\n", syscall(SYS_prctl, PR_GET_TIMERSLACK, 0, 0, 0, 0));
    SYS("PR_SET_TIMERSLACK(0)", SYS_prctl, PR_SET_TIMERSLACK, 0, 0, 0, 0);
    yes("  back to the start value", prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0) == slack0);

    SYS("PR_MCE_KILL_GET", SYS_prctl, PR_MCE_KILL_GET, 0, 0, 0, 0);
    SYS("PR_MCE_KILL_GET(1)", SYS_prctl, PR_MCE_KILL_GET, 1, 0, 0, 0);
    SYS("PR_MCE_KILL(SET, EARLY)", SYS_prctl, PR_MCE_KILL, PR_MCE_KILL_SET, PR_MCE_KILL_EARLY, 0, 0);
    SYS("PR_MCE_KILL_GET", SYS_prctl, PR_MCE_KILL_GET, 0, 0, 0, 0);
    SYS("PR_MCE_KILL(SET, LATE)", SYS_prctl, PR_MCE_KILL, PR_MCE_KILL_SET, PR_MCE_KILL_LATE, 0, 0);
    SYS("PR_MCE_KILL_GET", SYS_prctl, PR_MCE_KILL_GET, 0, 0, 0, 0);
    SYS("PR_MCE_KILL(SET, 9)", SYS_prctl, PR_MCE_KILL, PR_MCE_KILL_SET, 9, 0, 0);
    SYS("PR_MCE_KILL_GET", SYS_prctl, PR_MCE_KILL_GET, 0, 0, 0, 0);
    SYS("PR_MCE_KILL(CLEAR, 1)", SYS_prctl, PR_MCE_KILL, PR_MCE_KILL_CLEAR, 1, 0, 0);
    SYS("PR_MCE_KILL(CLEAR, 0)", SYS_prctl, PR_MCE_KILL, PR_MCE_KILL_CLEAR, 0, 0, 0);
    SYS("PR_MCE_KILL(5)", SYS_prctl, PR_MCE_KILL, 5, 0, 0, 0);
    SYS("PR_MCE_KILL(SET, EARLY, arg4)", SYS_prctl, PR_MCE_KILL, PR_MCE_KILL_SET, PR_MCE_KILL_EARLY, 1, 0);
    SYS("PR_MCE_KILL_GET", SYS_prctl, PR_MCE_KILL_GET, 0, 0, 0, 0);

    SYS("PR_GET_THP_DISABLE", SYS_prctl, PR_GET_THP_DISABLE, 0, 0, 0, 0);
    SYS("PR_SET_THP_DISABLE(1)", SYS_prctl, PR_SET_THP_DISABLE, 1, 0, 0, 0);
    SYS("PR_GET_THP_DISABLE", SYS_prctl, PR_GET_THP_DISABLE, 0, 0, 0, 0);
    SYS("PR_SET_THP_DISABLE(1, 1)", SYS_prctl, PR_SET_THP_DISABLE, 1, 1, 0, 0);
    SYS("PR_GET_THP_DISABLE(1)", SYS_prctl, PR_GET_THP_DISABLE, 1, 0, 0, 0);
    SYS("PR_SET_THP_DISABLE(0)", SYS_prctl, PR_SET_THP_DISABLE, 0, 0, 0, 0);

    SYS("PR_CAPBSET_READ(41)", SYS_prctl, PR_CAPBSET_READ, 41, 0, 0, 0);
    SYS("PR_CAPBSET_READ(40)", SYS_prctl, PR_CAPBSET_READ, 40, 0, 0, 0);
    SYS("PR_CAPBSET_DROP(1) as a user", SYS_prctl, PR_CAPBSET_DROP, 1, 0, 0, 0);
    SYS("PR_GET_SECUREBITS", SYS_prctl, PR_GET_SECUREBITS, 0, 0, 0, 0);
    SYS("PR_SET_SECUREBITS(1) as a user", SYS_prctl, PR_SET_SECUREBITS, 1, 0, 0, 0);
    SYS("PR_GET_KEEPCAPS", SYS_prctl, PR_GET_KEEPCAPS, 0, 0, 0, 0);
    SYS("PR_SET_KEEPCAPS(2)", SYS_prctl, PR_SET_KEEPCAPS, 2, 0, 0, 0);
    SYS("PR_SET_KEEPCAPS(1)", SYS_prctl, PR_SET_KEEPCAPS, 1, 0, 0, 0);
    SYS("PR_GET_KEEPCAPS", SYS_prctl, PR_GET_KEEPCAPS, 0, 0, 0, 0);
    SYS("PR_GET_SECUREBITS", SYS_prctl, PR_GET_SECUREBITS, 0, 0, 0, 0);
    SYS("PR_SET_KEEPCAPS(0)", SYS_prctl, PR_SET_KEEPCAPS, 0, 0, 0, 0);
    SYS("PR_CAP_AMBIENT(IS_SET, 0)", SYS_prctl, PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, 0, 0, 0);
    SYS("PR_CAP_AMBIENT(IS_SET, 99)", SYS_prctl, PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, 99, 0, 0);
    SYS("PR_CAP_AMBIENT(RAISE, 0)", SYS_prctl, PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 0, 0, 0);
    SYS("PR_CAP_AMBIENT(LOWER, 0)", SYS_prctl, PR_CAP_AMBIENT, PR_CAP_AMBIENT_LOWER, 0, 0, 0);
    SYS("PR_CAP_AMBIENT(CLEAR_ALL)", SYS_prctl, PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0);
    SYS("PR_CAP_AMBIENT(CLEAR_ALL, 1)", SYS_prctl, PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 1, 0, 0);
    SYS("PR_CAP_AMBIENT(9, 0)", SYS_prctl, PR_CAP_AMBIENT, 9, 0, 0, 0);
    SYS("PR_SET_PTRACER(0)", SYS_prctl, PR_SET_PTRACER, 0, 0, 0, 0);
    SYS("PR_SET_PTRACER(ANY)", SYS_prctl, PR_SET_PTRACER, PR_SET_PTRACER_ANY, 0, 0, 0);
    SYS("PR_SET_PTRACER(no such pid)", SYS_prctl, PR_SET_PTRACER, 4000000, 0, 0, 0);
    SYS("PR_SET_IO_FLUSHER(1) as a user", SYS_prctl, PR_SET_IO_FLUSHER, 1, 0, 0, 0);
    SYS("PR_GET_IO_FLUSHER as a user", SYS_prctl, PR_GET_IO_FLUSHER, 0, 0, 0, 0);
    SYS("PR_GET_SPECULATION_CTRL(99)", SYS_prctl, PR_GET_SPECULATION_CTRL, 99, 0, 0, 0);
    SYS("PR_GET_SPECULATION_CTRL(0, 1)", SYS_prctl, PR_GET_SPECULATION_CTRL, 0, 1, 0, 0);
    SYS("PR_SET_SPECULATION_CTRL(99, 0)", SYS_prctl, PR_SET_SPECULATION_CTRL, 99, 0, 0, 0);
    SYS("PR_SET_SPECULATION_CTRL(0, 0, 1)", SYS_prctl, PR_SET_SPECULATION_CTRL, 0, 0, 1, 0);
    SYS("PR_SET_SYSCALL_USER_DISPATCH(OFF)", SYS_prctl, PR_SET_SYSCALL_USER_DISPATCH, PR_SYS_DISPATCH_OFF, 0, 0, 0);
    SYS("PR_SET_SYSCALL_USER_DISPATCH(OFF, 1)", SYS_prctl, PR_SET_SYSCALL_USER_DISPATCH, PR_SYS_DISPATCH_OFF, 1, 0, 0);
    SYS("PR_SET_SYSCALL_USER_DISPATCH(5)", SYS_prctl, PR_SET_SYSCALL_USER_DISPATCH, 5, 0, 0, 0);
    unsigned size = 0;
    SYS("PR_SET_MM(PR_SET_MM_MAP_SIZE)", SYS_prctl, PR_SET_MM, PR_SET_MM_MAP_SIZE, &size, 0, 0);
    printf("  size = %u\n", size);
    SYS("PR_SET_MM(PR_SET_MM_MAP, size 7)", SYS_prctl, PR_SET_MM, PR_SET_MM_MAP, &size, 7, 0);
    SYS("PR_SET_MM(PR_SET_MM_START_CODE) as a user", SYS_prctl, PR_SET_MM, PR_SET_MM_START_CODE, 0, 0, 0);
    int dummy;
    syscall(SYS_set_tid_address, &dummy);
    int *got = NULL;
    SYS("PR_GET_TID_ADDRESS", SYS_prctl, PR_GET_TID_ADDRESS, &got, 0, 0, 0);
    yes("  it is the address given", got == &dummy);
    SYS("PR_GET_TID_ADDRESS(kernel address)", SYS_prctl, PR_GET_TID_ADDRESS, KERNEL_ADDR, 0, 0, 0);
    unsigned long aux[128];
    memset(aux, 0xff, sizeof aux);
    long n = syscall(SYS_prctl, PR_GET_AUXV, aux, sizeof aux, 0, 0);
    yes("PR_GET_AUXV: the size of saved_auxv, a multiple of 16 of at least 400 bytes", n >= 400 && n % 16 == 0);
    int pagesz = 0, ended = 0;
    for (long i = 0; i + 1 < n / 8; i += 2) {
        if (aux[i] == AT_PAGESZ && aux[i + 1] == 4096)
            pagesz = 1;
        if (aux[i] == AT_NULL) {
            ended = 1;
            break;
        }
    }
    yes("  it holds AT_PAGESZ 4096 before its AT_NULL", pagesz && ended);
    yes("  nothing written past the size", n / 8 >= 128 || aux[n / 8] == ~0ul);
    unsigned long few[2] = {~0ul, ~0ul};
    long n2 = syscall(SYS_prctl, PR_GET_AUXV, few, 8, 0, 0);
    yes("  with len 8: the same answer, one word written", n2 == n && few[0] == aux[0] && few[1] == ~0ul);
    SYS("PR_GET_AUXV(kernel address, 16)", SYS_prctl, PR_GET_AUXV, KERNEL_ADDR, 16, 0, 0);
    yes("PR_GET_AUXV(NULL, 0): the size again", syscall(SYS_prctl, PR_GET_AUXV, 0, 0, 0, 0) == n);
    SYS("PR_GET_AUXV(aux, 16, 1)", SYS_prctl, PR_GET_AUXV, aux, 16, 1, 0);
}

/* PR_SET_CHILD_SUBREAPER with PR_SET_PDEATHSIG: we are a subreaper; a child forks a grandchild that asks for
 * SIGUSR1 when its parent dies, then the child exits. The grandchild is killed by SIGUSR1 (default action)
 * and, orphaned, comes back to us: our wait4 collects it. */
static void prctl_reaper(void) {
    int r = -1;
    SYS("PR_GET_CHILD_SUBREAPER", SYS_prctl, PR_GET_CHILD_SUBREAPER, &r, 0, 0, 0);
    printf("  = %d\n", r);
    SYS("PR_SET_CHILD_SUBREAPER(1)", SYS_prctl, PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    prctl(PR_GET_CHILD_SUBREAPER, &r, 0, 0, 0);
    printf("  now %d\n", r);
    SYS("PR_GET_CHILD_SUBREAPER(kernel address)", SYS_prctl, PR_GET_CHILD_SUBREAPER, KERNEL_ADDR, 0, 0, 0);
    int p[2];
    if (pipe(p) < 0) {
        printf("pipe failed: %s\n", ename(errno));
        return;
    }
    pid_t child = fork();
    if (child == 0) {
        pid_t g = fork();
        if (g == 0) {
            close(p[0]);
            prctl(PR_SET_PDEATHSIG, SIGUSR1, 0, 0, 0);
            if (write(p[1], "r", 1) != 1)
                _exit(3);
            for (;;)
                pause();
        }
        char c;
        close(p[1]);
        if (read(p[0], &c, 1) != 1)
            _exit(4);
        _exit(0);
    }
    close(p[1]);
    close(p[0]);
    int st = 0;
    pid_t w = waitpid(child, &st, 0);
    yes("the child ended with 0", w == child && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    st = 0;
    w = waitpid(-1, &st, 0);
    yes("the orphaned grandchild came to us (the subreaper)", w > 0 && w != child);
    yes("  and was killed by its pdeath signal SIGUSR1", WIFSIGNALED(st) && WTERMSIG(st) == SIGUSR1);
    show("  no more children", waitpid(-1, &st, WNOHANG));
    prctl(PR_SET_CHILD_SUBREAPER, 0, 0, 0, 0);
    prctl(PR_GET_CHILD_SUBREAPER, &r, 0, 0, 0);
    printf("PR_SET_CHILD_SUBREAPER(0): now %d\n", r);
}

/* ---- misc --------------------------------------------------------------------------------------------- */

static void misc_sysinfo(void) {
    struct sysinfo s;
    memset(&s, 0xee, sizeof s);
    show("sysinfo", sysinfo(&s));
    yes("  uptime > 0", s.uptime > 0);
    yes("  loads below 1000.0", s.loads[0] < 1000ul << 16 && s.loads[1] < 1000ul << 16 && s.loads[2] < 1000ul << 16);
    yes("  totalram > 64 MiB", s.totalram > 64ul << 20);
    yes("  freeram <= totalram", s.freeram <= s.totalram);
    yes("  sharedram <= totalram", s.sharedram <= s.totalram);
    yes("  freeswap <= totalswap", s.freeswap <= s.totalswap);
    yes("  procs > 0", s.procs > 0);
    printf("  mem_unit = %u, totalhigh = %lu, freehigh = %lu\n", s.mem_unit, s.totalhigh, s.freehigh);
    SYS("sysinfo(kernel address)", SYS_sysinfo, KERNEL_ADDR);
    SYS("sysinfo(NULL)", SYS_sysinfo, NULL);
    struct sysinfo t;
    sleep_ms(1100);
    sysinfo(&t);
    yes("  uptime grew after 1.1 s", t.uptime > s.uptime);
}

static void misc_getrandom(void) {
    unsigned char b[64];
    SYS("getrandom(16, 0)", SYS_getrandom, b, 16, 0);
    SYS("getrandom(16, GRND_NONBLOCK)", SYS_getrandom, b, 16, 1);
    SYS("getrandom(16, GRND_RANDOM)", SYS_getrandom, b, 16, 2);
    SYS("getrandom(16, GRND_INSECURE)", SYS_getrandom, b, 16, 4);
    SYS("getrandom(16, GRND_INSECURE|GRND_NONBLOCK)", SYS_getrandom, b, 16, 5);
    SYS("getrandom(16, GRND_RANDOM|GRND_INSECURE)", SYS_getrandom, b, 16, 6);
    SYS("getrandom(16, 8)", SYS_getrandom, b, 16, 8);
    SYS("getrandom(16, 1 << 32)", SYS_getrandom, b, 16, 1ul << 32);
    SYS("getrandom(NULL, 0)", SYS_getrandom, NULL, 0, 0);
    SYS("getrandom(kernel address, 1)", SYS_getrandom, KERNEL_ADDR, 1, 0);
    SYS("getrandom(NULL, 1)", SYS_getrandom, NULL, 1, 0);
    long pg = sysconf(_SC_PAGESIZE);
    unsigned char *m = mmap(NULL, 2 * pg, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    munmap(m + pg, pg);
    SYS("getrandom(8 bytes before an unmapped page, 64)", SYS_getrandom, m + pg - 8, 64, 0);
    munmap(m, pg);
    unsigned char a[32], c[32];
    syscall(SYS_getrandom, a, 32, 0);
    syscall(SYS_getrandom, c, 32, 0);
    yes("two 32-byte answers differ", memcmp(a, c, 32) != 0);
    int zeros = 0;
    unsigned char big[4096];
    syscall(SYS_getrandom, big, sizeof big, 0);
    for (unsigned i = 0; i < sizeof big; i++)
        zeros += big[i] == 0;
    yes("4096 bytes: fewer than 64 zero bytes", zeros < 64);
}

static void misc_rlimit(void) {
    struct rlimit r, o;
    /* Only 0, 1 and 2 stay open (a harness may leave more), so the NOFILE cases count the same everywhere. */
    for (int fd = 3; fd < 1024; fd++)
        close(fd);
    SYS("getrlimit(99)", SYS_getrlimit, 99, &r);
    SYS("getrlimit(NOFILE, kernel address)", SYS_getrlimit, RLIMIT_NOFILE, KERNEL_ADDR);
    struct rlimit lim = {64, 128};
    show("setrlimit(NOFILE, 64/128)", setrlimit(RLIMIT_NOFILE, &lim));
    getrlimit(RLIMIT_NOFILE, &r);
    printf("  now %lu/%lu\n", (unsigned long)r.rlim_cur, (unsigned long)r.rlim_max);
    struct rlimit up = {64, 256};
    show("setrlimit(NOFILE, hard up to 256) as a user", setrlimit(RLIMIT_NOFILE, &up));
    struct rlimit inv = {100, 50};
    show("setrlimit(NOFILE, cur above max)", setrlimit(RLIMIT_NOFILE, &inv));
    struct rlimit huge = {64, 2000000};
    show("setrlimit(NOFILE, max above nr_open)", setrlimit(RLIMIT_NOFILE, &huge));
    SYS("setrlimit(NOFILE, kernel address)", SYS_setrlimit, RLIMIT_NOFILE, KERNEL_ADDR);
    struct rlimit ten = {10, 128};
    setrlimit(RLIMIT_NOFILE, &ten);
    int fds[16], n = 0;
    for (int i = 0; i < 16; i++) {
        int fd = open("/dev/null", O_RDONLY);
        if (fd < 0) {
            printf("  open with NOFILE 10: stopped after %d with %s\n", n, ename(errno));
            break;
        }
        fds[n++] = fd;
    }
    show("  dup", dup(0));
    show("  dup2(0, 10)", dup2(0, 10));
    show("  dup2(0, 9)", dup2(0, 9));
    show("  fcntl(0, F_DUPFD, 10)", fcntl(0, F_DUPFD, 10));
    show("  fcntl(0, F_DUPFD, 3)", fcntl(0, F_DUPFD, 3));
    for (int i = 0; i < n; i++)
        close(fds[i]);
    close(9);
    setrlimit(RLIMIT_NOFILE, &lim);
    /* prlimit64 on a child: its limit changes, the copy of the new value comes first (EFAULT before ESRCH). */
    int p[2];
    if (pipe(p) < 0)
        return;
    pid_t c = fork();
    if (c == 0) {
        char x;
        close(p[1]);
        if (read(p[0], &x, 1) != 1)
            _exit(9);
        struct rlimit k;
        getrlimit(RLIMIT_NOFILE, &k);
        _exit(k.rlim_cur == 20 ? 0 : 1);
    }
    close(p[0]);
    struct rlimit twenty = {20, 128};
    show("prlimit64(child, NOFILE, 20/128, &old)", prlimit(c, RLIMIT_NOFILE, &twenty, &o));
    printf("  old = %lu/%lu\n", (unsigned long)o.rlim_cur, (unsigned long)o.rlim_max);
    SYS("prlimit64(no such pid, NOFILE, NULL, &old)", SYS_prlimit64, 4000000, RLIMIT_NOFILE, NULL, &o);
    SYS("prlimit64(no such pid, NOFILE, kernel address, NULL)", SYS_prlimit64, 4000000, RLIMIT_NOFILE, KERNEL_ADDR, NULL);
    SYS("prlimit64(0, 99, NULL, &old)", SYS_prlimit64, 0, 99, NULL, &o);
    SYS("prlimit64(0, NOFILE, NULL, kernel address)", SYS_prlimit64, 0, RLIMIT_NOFILE, NULL, KERNEL_ADDR);
    SYS("prlimit64(-1, NOFILE, NULL, &old)", SYS_prlimit64, -1, RLIMIT_NOFILE, NULL, &o);
    if (write(p[1], "g", 1) != 1)
        printf("pipe write failed\n");
    int st;
    waitpid(c, &st, 0);
    yes("  the child sees NOFILE 20", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    close(p[1]);
}

static void misc_dup(void) {
    int fd = open("/dev/null", O_RDWR);
    show("dup(-1)", dup(-1));
    show("dup(999)", dup(999));
    show("dup2(999, 50)", dup2(999, 50));
    show("dup2(fd, fd)", dup2(fd, fd) == fd ? 0 : -2);
    show("dup2(999, 999)", dup2(999, 999));
    SYS("dup2(fd, -1)", SYS_dup2, fd, -1);
    SYS("dup3(fd, fd, 0)", SYS_dup3, fd, fd, 0);
    SYS("dup3(fd, 50, 1)", SYS_dup3, fd, 50, 1);
    SYS("dup3(fd, 50, O_CLOEXEC)", SYS_dup3, fd, 50, O_CLOEXEC);
    show("  F_GETFD", fcntl(50, F_GETFD));
    SYS("dup2(fd, 50) over it", SYS_dup2, fd, 50);
    show("  F_GETFD (dup2 clears close-on-exec)", fcntl(50, F_GETFD));
    int d = dup(fd);
    yes("dup shares the open file (status flags)", fcntl(fd, F_SETFL, O_NONBLOCK) == 0 && (fcntl(d, F_GETFL) & O_NONBLOCK));
    show("  and not the close-on-exec bit", fcntl(d, F_GETFD));
    close(d);
    close(50);
    close(fd);
    SYS("sched_yield", SYS_sched_yield);
}

static void misc_pipe_size(void) {
    int p[2];
    if (pipe(p) < 0)
        return;
    show("F_GETPIPE_SZ of a new pipe", fcntl(p[0], F_GETPIPE_SZ));
    show("F_SETPIPE_SZ(0): one page", fcntl(p[1], F_SETPIPE_SZ, 0));
    show("F_SETPIPE_SZ(5000) rounds up", fcntl(p[0], F_SETPIPE_SZ, 5000));
    show("  F_GETPIPE_SZ on the other end", fcntl(p[1], F_GETPIPE_SZ));
    show("F_SETPIPE_SZ(65536)", fcntl(p[0], F_SETPIPE_SZ, 65536));
    show("F_SETPIPE_SZ(2 MiB) above pipe-max-size", fcntl(p[0], F_SETPIPE_SZ, 2 << 20));
    SYS("F_SETPIPE_SZ(0x80000001)", SYS_fcntl, p[0], F_SETPIPE_SZ, 0x80000001ul);
    SYS("F_SETPIPE_SZ(-1)", SYS_fcntl, p[0], F_SETPIPE_SZ, -1l);
    SYS("F_SETPIPE_SZ(1 << 32): the int argument", SYS_fcntl, p[0], F_SETPIPE_SZ, 1ul << 32);
    show("  F_GETPIPE_SZ", fcntl(p[0], F_GETPIPE_SZ));
    show("F_SETPIPE_SZ(65536) again", fcntl(p[0], F_SETPIPE_SZ, 65536));
    static char buf[12288];
    memset(buf, 'p', sizeof buf);
    show("write 12288 bytes", write(p[1], buf, sizeof buf));
    show("F_SETPIPE_SZ(4096) with three pages queued", fcntl(p[1], F_SETPIPE_SZ, 4096));
    show("F_SETPIPE_SZ(16384) with three pages queued", fcntl(p[1], F_SETPIPE_SZ, 16384));
    show("  read them back", read(p[0], buf, sizeof buf));
    close(p[0]);
    close(p[1]);
    int nul = open("/dev/null", O_RDWR);
    show("F_GETPIPE_SZ on /dev/null", fcntl(nul, F_GETPIPE_SZ));
    show("F_SETPIPE_SZ on /dev/null", fcntl(nul, F_SETPIPE_SZ, 4096));
    close(nul);
    int efd = syscall(SYS_eventfd2, 0, 0);
    show("F_GETPIPE_SZ on an eventfd", fcntl(efd, F_GETPIPE_SZ));
    close(efd);
    show("F_GETPIPE_SZ(999)", fcntl(999, F_GETPIPE_SZ));
}

static void misc_close_range(void) {
    int fd = open("/dev/null", O_RDWR);
    dup2(fd, 60);
    dup2(fd, 61);
    dup2(fd, 62);
    SYS("close_range(5, 3, 0)", SYS_close_range, 5, 3, 0);
    SYS("close_range(60, 60, 8): unknown flag", SYS_close_range, 60, 60, 8);
    SYS("close_range(60, 61, 0)", SYS_close_range, 60, 61, 0);
    yes("  60 and 61 closed, 62 open", fcntl(60, F_GETFD) < 0 && fcntl(61, F_GETFD) < 0 && fcntl(62, F_GETFD) == 0);
    SYS("close_range(62, ~0, CLOSE_RANGE_CLOEXEC)", SYS_close_range, 62, ~0u, CLOSE_RANGE_CLOEXEC);
    show("  F_GETFD(62)", fcntl(62, F_GETFD));
    SYS("close_range(62, ~0, CLOSE_RANGE_UNSHARE)", SYS_close_range, 62, ~0u, CLOSE_RANGE_UNSHARE);
    show("  F_GETFD(62)", fcntl(62, F_GETFD));
    SYS("close_range(4000, ~0, 0): nothing open", SYS_close_range, 4000, ~0u, 0);
    SYS("close_range(~0, ~0, 0)", SYS_close_range, ~0u, ~0u, 0);
    yes("  fd itself still open", fcntl(fd, F_GETFD) == 0);
    close(fd);
}

static long sched_cell[3];

static void *sched_thread(void *arg) {
    (void)arg;
    __atomic_store_n(&sched_cell[0], syscall(SYS_gettid), __ATOMIC_RELEASE);
    while (__atomic_load_n(&sched_cell[1], __ATOMIC_ACQUIRE) == 0)
        sched_yield();
    errno = 0;
    sched_cell[2] = getpriority(PRIO_PROCESS, 0);
    return 0;
}

/* nice and affinity are per thread and named by thread id; fork copies the calling thread's. Last in misc:
 * the process keeps a raised nice. */
static void misc_sched(void) {
    /* Relative to the start value (the host's session may run at another nice); 19 and 18 can always be set. */
    errno = 0;
    int n0 = getpriority(PRIO_PROCESS, 0);
    yes("getpriority(0) works", errno == 0 && n0 <= 18);
    pthread_t t;
    if (pthread_create(&t, 0, sched_thread, 0) != 0) {
        printf("pthread_create failed\n");
        return;
    }
    while (__atomic_load_n(&sched_cell[0], __ATOMIC_ACQUIRE) == 0)
        sched_yield();
    long tid = sched_cell[0];
    cpu_set_t s;
    CPU_ZERO(&s);
    sched_getaffinity(0, sizeof s, &s);
    int main_cpus = CPU_COUNT(&s);
    show("setpriority(PRIO_PROCESS, the thread's tid, 19)", setpriority(PRIO_PROCESS, tid, 19));
    show("  getpriority(the thread's tid)", getpriority(PRIO_PROCESS, tid));
    yes("  the main thread's nice is unchanged", getpriority(PRIO_PROCESS, 0) == n0);
    CPU_ZERO(&s);
    CPU_SET(0, &s);
    show("sched_setaffinity(the thread's tid, CPU 0)", sched_setaffinity(tid, sizeof s, &s));
    CPU_ZERO(&s);
    show("  sched_getaffinity(the thread's tid)", sched_getaffinity(tid, sizeof s, &s) < 0 ? -1 : 0);
    yes("  only CPU 0", CPU_COUNT(&s) == 1 && CPU_ISSET(0, &s));
    CPU_ZERO(&s);
    sched_getaffinity(0, sizeof s, &s);
    yes("  the main thread's mask is unchanged", CPU_COUNT(&s) == main_cpus);
    __atomic_store_n(&sched_cell[1], 1, __ATOMIC_RELEASE);
    pthread_join(t, 0);
    printf("  the thread's own getpriority(0): %ld\n", sched_cell[2]);
    show("setpriority(PRIO_PROCESS, own tid, 18)", setpriority(PRIO_PROCESS, syscall(SYS_gettid), 18));
    CPU_ZERO(&s);
    CPU_SET(0, &s);
    show("sched_setaffinity(0, CPU 0)", sched_setaffinity(0, sizeof s, &s));
    pid_t c = fork();
    if (c == 0) {
        cpu_set_t m;
        CPU_ZERO(&m);
        errno = 0;
        int n = getpriority(PRIO_PROCESS, 0);
        int one = sched_getaffinity(0, sizeof m, &m) == 0 && CPU_COUNT(&m) == 1 && CPU_ISSET(0, &m);
        _exit((n == 18 ? 1 : 0) | (one ? 2 : 0));
    }
    int st;
    waitpid(c, &st, 0);
    yes("  the child has nice 18", WIFEXITED(st) && (WEXITSTATUS(st) & 1));
    yes("  the child runs on CPU 0 only", WIFEXITED(st) && (WEXITSTATUS(st) & 2));
}

/* ---- flock -------------------------------------------------------------------------------------------- */

static char lock_path[64];

static void flock_cases(void) {
    snprintf(lock_path, sizeof lock_path, "/tmp/abi-ab5-flock-%d", (int)getpid());
    int a = open(lock_path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    int b = open(lock_path, O_RDWR);
    int a2 = dup(a);
    show("flock(a, LOCK_SH)", flock(a, LOCK_SH));
    show("flock(b, LOCK_SH)", flock(b, LOCK_SH));
    show("flock(b, LOCK_EX|LOCK_NB) while a shares", flock(b, LOCK_EX | LOCK_NB));
    show("flock(b, LOCK_SH|LOCK_NB) after its failed conversion", flock(b, LOCK_SH | LOCK_NB));
    show("flock(a, LOCK_EX|LOCK_NB) while b shares", flock(a, LOCK_EX | LOCK_NB));
    show("flock(b, LOCK_UN)", flock(b, LOCK_UN));
    show("flock(a, LOCK_EX|LOCK_NB) alone", flock(a, LOCK_EX | LOCK_NB));
    show("flock(a2 = dup(a), LOCK_EX|LOCK_NB): the same open file", flock(a2, LOCK_EX | LOCK_NB));
    show("flock(b, LOCK_SH|LOCK_NB) against it", flock(b, LOCK_SH | LOCK_NB));
    close(a);
    show("  after close(a), a2 keeps it: flock(b, LOCK_SH|LOCK_NB)", flock(b, LOCK_SH | LOCK_NB));
    close(a2);
    show("  after the last close: flock(b, LOCK_EX|LOCK_NB)", flock(b, LOCK_EX | LOCK_NB));
    show("flock(b, LOCK_UN) twice", flock(b, LOCK_UN) | flock(b, LOCK_UN));
    show("flock(b, LOCK_SH|LOCK_EX)", flock(b, LOCK_SH | LOCK_EX));
    show("flock(b, 0)", flock(b, 0));
    show("flock(b, LOCK_NB)", flock(b, LOCK_NB));
    show("flock(b, LOCK_MAND|LOCK_EX)", flock(b, 32 | LOCK_EX));
    show("flock(999, LOCK_SH)", flock(999, LOCK_SH));
    int pa = open(lock_path, O_PATH);
    show("flock(O_PATH fd, LOCK_SH)", flock(pa, LOCK_SH));
    close(pa);
    int p[2];
    if (pipe(p) == 0) {
        show("flock(pipe, LOCK_EX)", flock(p[0], LOCK_EX));
        close(p[0]);
        close(p[1]);
    }
    /* Between processes: a child holds LOCK_EX; we wait for it (blocking flock); the child's exit releases. */
    int sync[2];
    if (pipe(sync) < 0)
        return;
    pid_t c = fork();
    if (c == 0) {
        int f = open(lock_path, O_RDWR);
        flock(f, LOCK_EX);
        if (write(sync[1], "l", 1) != 1)
            _exit(2);
        sleep_ms(100);
        _exit(0);
    }
    char x;
    close(sync[1]);
    if (read(sync[0], &x, 1) != 1)
        printf("sync read failed\n");
    show("child holds LOCK_EX: flock(b, LOCK_SH|LOCK_NB)", flock(b, LOCK_SH | LOCK_NB));
    long t0 = now_ns(CLOCK_MONOTONIC);
    show("flock(b, LOCK_SH) waits for the child's exit", flock(b, LOCK_SH));
    yes("  it waited", now_ns(CLOCK_MONOTONIC) - t0 >= 50 * MS);
    int st;
    waitpid(c, &st, 0);
    close(sync[0]);
    /* A forked child shares b's open file and so its lock. */
    c = fork();
    if (c == 0)
        _exit(flock(b, LOCK_SH | LOCK_NB) == 0 ? 0 : 1);
    waitpid(c, &st, 0);
    yes("a forked child shares the parent's lock (same open file)", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    close(b);
    unlink(lock_path);
}

/* ---- fcntl -------------------------------------------------------------------------------------------- */

static void showlk(const char *label, int fd, int cmd, short type, short whence, off_t start, off_t len) {
    struct flock l;
    memset(&l, 0, sizeof l);
    l.l_type = type;
    l.l_whence = whence;
    l.l_start = start;
    l.l_len = len;
    long r = fcntl(fd, cmd, &l);
    if (r < 0) {
        printf("%s = -%s\n", label, ename(errno));
        return;
    }
    if (cmd == F_GETLK || cmd == F_OFD_GETLK)
        printf("%s = %ld: type %d whence %d start %ld len %ld pid %s\n", label, r, l.l_type, l.l_whence, (long)l.l_start, (long)l.l_len, l.l_type == F_UNLCK ? "-" : l.l_pid == -1 ? "-1" : l.l_pid > 0 ? "a process" : "0");
    else
        printf("%s = %ld\n", label, r);
}

static void fcntl_locks(void) {
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-ab5-lock-%d", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (write(fd, "0123456789", 10) != 10)
        printf("write failed\n");
    showlk("F_GETLK nothing locked", fd, F_GETLK, F_WRLCK, SEEK_SET, 0, 0);
    showlk("F_GETLK type F_UNLCK", fd, F_GETLK, F_UNLCK, SEEK_SET, 0, 0);
    showlk("F_GETLK type 7", fd, F_GETLK, 7, SEEK_SET, 0, 0);
    showlk("F_SETLK whence 9", fd, F_SETLK, F_WRLCK, 9, 0, 0);
    showlk("F_SETLK start -1", fd, F_SETLK, F_WRLCK, SEEK_SET, -1, 1);
    showlk("F_SETLK len -20 at 10", fd, F_SETLK, F_WRLCK, SEEK_SET, 10, -20);
    showlk("F_SETLK start LLONG_MAX len 2", fd, F_SETLK, F_WRLCK, SEEK_SET, 0x7fffffffffffffffl, 2);
    showlk("F_SETLK SEEK_END start LLONG_MAX", fd, F_SETLK, F_WRLCK, SEEK_END, 0x7fffffffffffffffl, 1);
    showlk("F_SETLK type 7", fd, F_SETLK, 7, SEEK_SET, 0, 1);
    SYS("F_SETLK kernel address", SYS_fcntl, fd, F_SETLK, KERNEL_ADDR);
    SYS("F_GETLK kernel address", SYS_fcntl, fd, F_GETLK, KERNEL_ADDR);
    int ro = open(path, O_RDONLY);
    showlk("F_SETLK F_WRLCK on a read-only fd", ro, F_SETLK, F_WRLCK, SEEK_SET, 0, 1);
    showlk("F_SETLK F_RDLCK on it", ro, F_SETLK, F_RDLCK, SEEK_SET, 0, 1);
    showlk("F_SETLK F_UNLCK on it", ro, F_SETLK, F_UNLCK, SEEK_SET, 0, 1);
    int wo = open(path, O_WRONLY);
    showlk("F_SETLK F_RDLCK on a write-only fd", wo, F_SETLK, F_RDLCK, SEEK_SET, 0, 1);
    close(wo);
    showlk("F_SETLK F_WRLCK 0..4", fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 5);
    showlk("F_SETLK F_RDLCK 10..", fd, F_SETLK, F_RDLCK, SEEK_SET, 10, 0);
    showlk("  own locks never conflict: F_GETLK", fd, F_GETLK, F_WRLCK, SEEK_SET, 0, 0);
    /* A child sees our locks as conflicts and waits; EDEADLK when two processes wait for each other. */
    int up[2], down[2];
    if (pipe(up) < 0 || pipe(down) < 0)
        return;
    pid_t c = fork();
    if (c == 0) {
        char x;
        close(up[0]);
        close(down[1]);
        int f = open(path, O_RDWR);
        showlk("child: F_GETLK F_WRLCK 2..2", f, F_GETLK, F_WRLCK, SEEK_SET, 2, 1);
        showlk("child: F_GETLK F_RDLCK 3..", f, F_GETLK, F_RDLCK, SEEK_SET, 3, 0);
        showlk("child: F_GETLK F_WRLCK 5..9 (the gap)", f, F_GETLK, F_WRLCK, SEEK_SET, 5, 5);
        showlk("child: F_GETLK F_WRLCK 9..10", f, F_GETLK, F_WRLCK, SEEK_SET, 9, 2);
        showlk("child: F_SETLK F_RDLCK 12..", f, F_SETLK, F_RDLCK, SEEK_SET, 12, 0);
        showlk("child: F_SETLK F_WRLCK 0..0", f, F_SETLK, F_WRLCK, SEEK_SET, 0, 1);
        showlk("child: F_SETLK F_WRLCK 5..9", f, F_SETLK, F_WRLCK, SEEK_SET, 5, 5);
        fflush(stdout);
        if (write(up[1], "1", 1) != 1)
            _exit(2);
        /* Waits for our lock on 0..4, which the parent drops after its EDEADLK test. */
        showlk("child: F_SETLKW F_WRLCK 0..0 (waits)", f, F_SETLKW, F_WRLCK, SEEK_SET, 0, 1);
        fflush(stdout);
        if (write(up[1], "2", 1) != 1)
            _exit(2);
        if (read(down[0], &x, 1) != 1)
            _exit(2);
        _exit(0);
    }
    close(up[1]);
    close(down[0]);
    char x;
    if (read(up[0], &x, 1) != 1)
        printf("sync failed\n");
    fflush(stdout);
    sleep_ms(200);
    /* The child waits for 0..0 (ours); we now wait for 5..9 (the child's): a circle. */
    showlk("parent: F_SETLKW F_WRLCK 5..5 would wait for the waiting child", fd, F_SETLKW, F_WRLCK, SEEK_SET, 5, 1);
    showlk("parent: F_GETLK F_WRLCK 5..", fd, F_GETLK, F_WRLCK, SEEK_SET, 5, 0);
    /* Any close of the file drops all our POSIX locks on it: the child gets 0..0. */
    close(ro);
    if (read(up[0], &x, 1) != 1)
        printf("sync 2 failed\n");
    fflush(stdout);
    showlk("parent: after our close(ro) the child holds 0..0: F_GETLK", fd, F_GETLK, F_RDLCK, SEEK_SET, 0, 1);
    if (write(down[1], "x", 1) != 1)
        printf("sync 3 failed\n");
    int st;
    waitpid(c, &st, 0);
    showlk("parent: after the child's exit: F_GETLK", fd, F_GETLK, F_WRLCK, SEEK_SET, 0, 0);
    close(up[0]);
    close(down[1]);

    /* Splits and merges of our own locks (seen from a child). */
    showlk("F_SETLK F_WRLCK 0..9", fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 10);
    showlk("F_SETLK F_UNLCK 3..5", fd, F_SETLK, F_UNLCK, SEEK_SET, 3, 3);
    showlk("F_SETLK F_RDLCK 8..8", fd, F_SETLK, F_RDLCK, SEEK_SET, 8, 1);
    c = fork();
    if (c == 0) {
        int f = open(path, O_RDWR);
        showlk("child: F_GETLK F_RDLCK 0..", f, F_GETLK, F_RDLCK, SEEK_SET, 0, 0);
        showlk("child: F_GETLK F_RDLCK 3..", f, F_GETLK, F_RDLCK, SEEK_SET, 3, 0);
        showlk("child: F_GETLK F_WRLCK 7..", f, F_GETLK, F_WRLCK, SEEK_SET, 7, 0);
        showlk("child: F_GETLK F_WRLCK 8..", f, F_GETLK, F_WRLCK, SEEK_SET, 8, 0);
        showlk("child: F_GETLK F_RDLCK 8..8", f, F_GETLK, F_RDLCK, SEEK_SET, 8, 1);
        showlk("child: F_GETLK F_WRLCK 10..", f, F_GETLK, F_WRLCK, SEEK_SET, 10, 0);
        fflush(stdout);
        _exit(0);
    }
    waitpid(c, &st, 0);
    showlk("F_SETLK F_UNLCK all", fd, F_SETLK, F_UNLCK, SEEK_SET, 0, 0);

    /* OFD locks: owned by the open file, so two fds of one process conflict, a dup does not. */
    int o1 = open(path, O_RDWR), o2 = open(path, O_RDWR);
    showlk("F_OFD_SETLK F_WRLCK 0..0 on o1", o1, F_OFD_SETLK, F_WRLCK, SEEK_SET, 0, 1);
    showlk("F_OFD_SETLK F_WRLCK 0..0 on o2", o2, F_OFD_SETLK, F_WRLCK, SEEK_SET, 0, 1);
    showlk("F_OFD_GETLK on o2", o2, F_OFD_GETLK, F_RDLCK, SEEK_SET, 0, 0);
    showlk("F_GETLK (POSIX) on fd sees the OFD lock", fd, F_GETLK, F_WRLCK, SEEK_SET, 0, 0);
    int od = dup(o1);
    showlk("F_OFD_SETLK on a dup of o1", od, F_OFD_SETLK, F_WRLCK, SEEK_SET, 0, 1);
    struct flock l = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 1, .l_pid = 5};
    show("F_OFD_SETLK with l_pid 5", fcntl(o2, F_OFD_SETLK, &l));
    show("F_OFD_GETLK with l_pid 5", fcntl(o2, F_OFD_GETLK, &l));
    close(o1);
    showlk("  after close(o1) the dup keeps it: o2", o2, F_OFD_SETLK, F_WRLCK, SEEK_SET, 0, 1);
    close(od);
    showlk("  after the last close: o2", o2, F_OFD_SETLK, F_WRLCK, SEEK_SET, 0, 1);
    close(o2);
    close(fd);
    unlink(path);
}

/* Whether another process sees a write lock on 0..9 of `path`. */
static void held(const char *label, const char *path) {
    pid_t c = fork();
    if (c == 0) {
        int f = open(path, O_RDWR);
        struct flock l = {.l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 10};
        _exit(fcntl(f, F_GETLK, &l) == 0 && l.l_type != F_UNLCK ? 1 : 0);
    }
    int st;
    waitpid(c, &st, 0);
    yes(label, WIFEXITED(st) && WEXITSTATUS(st) == 1);
}

static void fcntl_release(void) {
    char path[64], other[64];
    snprintf(path, sizeof path, "/tmp/abi-ab5-rel-%d", (int)getpid());
    snprintf(other, sizeof other, "/tmp/abi-ab5-rel2-%d", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    int o = open(other, O_RDWR | O_CREAT | O_TRUNC, 0600);
    showlk("F_SETLK F_WRLCK 0..9", fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 10);
    held("  held", path);
    show("dup2(fd, fd)", dup2(fd, fd) == fd ? 0 : -2);
    held("  held after dup2(fd, fd)", path);
    int d = dup(fd);
    show("dup2(fd, d) over a dup of the same open file", dup2(fd, d) == d ? 0 : -2);
    held("  held after it", path);
    showlk("F_SETLK F_WRLCK 0..9", fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 10);
    show("dup3(other, d, O_CLOEXEC) over an fd of the file", dup3(o, d, O_CLOEXEC) == d ? 0 : -2);
    held("  held after it", path);
    showlk("F_SETLK F_WRLCK 0..9", fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 10);
    show("dup2(fd, 70) into a free fd", dup2(fd, 70) == 70 ? 0 : -2);
    held("  held after it", path);
    SYS("close_range(70, 70, CLOSE_RANGE_CLOEXEC)", SYS_close_range, 70, 70, CLOSE_RANGE_CLOEXEC);
    held("  held after it", path);
    SYS("close_range(70, 70, 0)", SYS_close_range, 70, 70, 0);
    held("  held after it", path);
    close(d);
    close(o);
    close(fd);
    unlink(other);
    unlink(path);
}

static void fcntl_owner(void) {
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-ab5-own-%d", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    show("F_GETOWN fresh", fcntl(fd, F_GETOWN));
    struct f_owner_ex ex = {7, 7};
    show("F_GETOWN_EX fresh", fcntl(fd, F_GETOWN_EX, &ex));
    printf("  type %d pid %d\n", ex.type, ex.pid);
    show("F_SETOWN(getpid())", fcntl(fd, F_SETOWN, getpid()));
    yes("  F_GETOWN is our pid", fcntl(fd, F_GETOWN) == getpid());
    fcntl(fd, F_GETOWN_EX, &ex);
    printf("  F_GETOWN_EX type %d, pid is ours: %s\n", ex.type, ex.pid == getpid() ? "yes" : "no");
    show("F_SETOWN(-getpgrp())", fcntl(fd, F_SETOWN, -getpgrp()));
    yes("  F_GETOWN is -pgrp", fcntl(fd, F_GETOWN) == -getpgrp());
    fcntl(fd, F_GETOWN_EX, &ex);
    printf("  F_GETOWN_EX type %d\n", ex.type);
    show("F_SETOWN(no such pid)", fcntl(fd, F_SETOWN, 4000000));
    show("F_SETOWN(INT_MIN)", fcntl(fd, F_SETOWN, -2147483647 - 1));
    show("F_SETOWN(0)", fcntl(fd, F_SETOWN, 0));
    show("  F_GETOWN", fcntl(fd, F_GETOWN));
    fcntl(fd, F_GETOWN_EX, &ex);
    printf("  F_GETOWN_EX type %d pid %d\n", ex.type, ex.pid);
    ex.type = F_OWNER_TID;
    ex.pid = (pid_t)syscall(SYS_gettid);
    show("F_SETOWN_EX(TID, gettid())", fcntl(fd, F_SETOWN_EX, &ex));
    yes("  F_GETOWN is the tid", fcntl(fd, F_GETOWN) == (int)syscall(SYS_gettid));
    ex.type = 9;
    show("F_SETOWN_EX(type 9)", fcntl(fd, F_SETOWN_EX, &ex));
    ex.type = F_OWNER_PID;
    ex.pid = 4000000;
    show("F_SETOWN_EX(PID, no such pid)", fcntl(fd, F_SETOWN_EX, &ex));
    ex.pid = -5;
    show("F_SETOWN_EX(PID, -5)", fcntl(fd, F_SETOWN_EX, &ex));
    SYS("F_SETOWN_EX(kernel address)", SYS_fcntl, fd, F_SETOWN_EX, KERNEL_ADDR);
    SYS("F_GETOWN_EX(kernel address)", SYS_fcntl, fd, F_GETOWN_EX, KERNEL_ADDR);
    unsigned uids[2] = {7, 7};
    show("F_GETOWNER_UIDS", fcntl(fd, F_GETOWNER_UIDS, uids));
    yes("  both our uid", uids[0] == getuid() && uids[1] == getuid());
    SYS("F_GETOWNER_UIDS(kernel address)", SYS_fcntl, fd, F_GETOWNER_UIDS, KERNEL_ADDR);
    /* An owner process that is gone reads as 0. */
    pid_t c = fork();
    if (c == 0)
        _exit(0);
    fcntl(fd, F_SETOWN, c);
    int st;
    waitpid(c, &st, 0);
    show("F_GETOWN after the owner was reaped", fcntl(fd, F_GETOWN));
    /* The owner belongs to the open file: a dup sees it, another open does not. */
    fcntl(fd, F_SETOWN, getpid());
    int d = dup(fd), other = open(path, O_RDWR);
    yes("a dup shares the owner", fcntl(d, F_GETOWN) == getpid());
    show("another open of the file", fcntl(other, F_GETOWN));
    show("F_GETSIG fresh", fcntl(fd, F_GETSIG));
    show("F_SETSIG(SIGUSR1)", fcntl(fd, F_SETSIG, SIGUSR1));
    show("  F_GETSIG on the dup", fcntl(d, F_GETSIG));
    show("F_SETSIG(65)", fcntl(fd, F_SETSIG, 65));
    show("F_SETSIG(-1)", fcntl(fd, F_SETSIG, -1));
    show("F_SETSIG(64)", fcntl(fd, F_SETSIG, 64));
    show("F_SETSIG(0)", fcntl(fd, F_SETSIG, 0));
    close(d);
    close(other);

    /* Leases, write hints, F_DUPFD_QUERY, F_SETFL refusals, seals on a regular file. */
    show("F_GETLEASE", fcntl(fd, F_GETLEASE));
    show("F_SETLEASE(F_UNLCK) without a lease", fcntl(fd, F_SETLEASE, F_UNLCK));
    show("F_SETLEASE(7)", fcntl(fd, F_SETLEASE, 7));
    int nul = open("/dev/null", O_RDWR);
    show("F_SETLEASE(F_RDLCK) on /dev/null", fcntl(nul, F_SETLEASE, F_RDLCK));
    show("F_GETLEASE on /dev/null", fcntl(nul, F_GETLEASE));
    uint64_t h = 99;
    show("F_GET_RW_HINT fresh", fcntl(fd, F_GET_RW_HINT, &h));
    printf("  hint %lu\n", (unsigned long)h);
    h = 3;
    show("F_SET_RW_HINT(3)", fcntl(fd, F_SET_RW_HINT, &h));
    h = 0;
    int again = open(path, O_RDONLY);
    fcntl(again, F_GET_RW_HINT, &h);
    printf("  another open reads hint %lu (it is the inode's)\n", (unsigned long)h);
    h = 6;
    show("F_SET_RW_HINT(6)", fcntl(fd, F_SET_RW_HINT, &h));
    SYS("F_SET_RW_HINT(kernel address)", SYS_fcntl, fd, F_SET_RW_HINT, KERNEL_ADDR);
    SYS("F_GET_RW_HINT(kernel address)", SYS_fcntl, fd, F_GET_RW_HINT, KERNEL_ADDR);
    close(again);
    d = dup(fd);
    SYS("F_DUPFD_QUERY(fd, its dup)", SYS_fcntl, fd, 1027, d);
    SYS("F_DUPFD_QUERY(fd, /dev/null)", SYS_fcntl, fd, 1027, nul);
    SYS("F_DUPFD_QUERY(fd, 999)", SYS_fcntl, fd, 1027, 999);
    SYS("F_DUPFD_QUERY(fd, -1)", SYS_fcntl, fd, 1027, -1);
    int pa = open(path, O_PATH);
    struct flock lp = {.l_type = F_RDLCK, .l_whence = SEEK_SET};
    SYS("F_GETOWN on an O_PATH fd", SYS_fcntl, pa, F_GETOWN);
    SYS("F_GETLK on an O_PATH fd", SYS_fcntl, pa, F_GETLK, &lp);
    close(pa);
    close(d);
    int sp[2];
    if (pipe(sp) == 0) {
        SYS("F_ADD_SEALS on a pipe", SYS_fcntl, sp[1], F_ADD_SEALS, F_SEAL_SEAL);
        SYS("F_GET_SEALS on a pipe", SYS_fcntl, sp[0], F_GET_SEALS);
        close(sp[0]);
        close(sp[1]);
    }
    SYS("F_SETFL(O_DIRECT) on /dev/null", SYS_fcntl, nul, F_SETFL, O_DIRECT);
    printf("  F_GETFL %o\n", fcntl(nul, F_GETFL));
    SYS("F_SETFL(O_DIRECT) on a tmpfs file", SYS_fcntl, fd, F_SETFL, O_DIRECT);
    printf("  F_GETFL %o\n", fcntl(fd, F_GETFL));
    SYS("F_SETFL(0)", SYS_fcntl, fd, F_SETFL, 0);
    int efd = syscall(SYS_eventfd2, 0, 0);
    SYS("F_SETFL(O_DIRECT) on an eventfd", SYS_fcntl, efd, F_SETFL, O_DIRECT);
    SYS("F_GET_SEALS on an eventfd", SYS_fcntl, efd, F_GET_SEALS);
    close(efd);
    SYS("F_CANCELLK", SYS_fcntl, fd, 1029, 0);
    SYS("unknown command 9999", SYS_fcntl, fd, 9999, 0);
    SYS("F_GETOWN(999)", SYS_fcntl, 999, F_GETOWN);
    close(nul);
    close(fd);
    unlink(path);
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "";
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (strcmp(m, "timerfd") == 0) {
        timerfd_errors();
        timerfd_oneshot();
        timerfd_periodic();
        timerfd_clocks();
        timerfd_waits();
    } else if (strcmp(m, "prctl") == 0) {
        prctl_basics();
        prctl_more();
        prctl_reaper();
    } else if (strcmp(m, "misc") == 0) {
        misc_sysinfo();
        misc_getrandom();
        misc_rlimit();
        misc_dup();
        misc_pipe_size();
        misc_close_range();
        misc_sched();
    } else if (strcmp(m, "flock") == 0) {
        flock_cases();
    } else if (strcmp(m, "fcntl") == 0) {
        fcntl_locks();
        fcntl_release();
        fcntl_owner();
    } else {
        printf("usage: abi-ab5 timerfd|prctl|misc|flock|fcntl\n");
        return 2;
    }
    printf("ab5 %s: done\n", m);
    return 0;
}
