/* AB-6 fixer: the signal calls end to end (gVisor-style; the ABI diff against CachyOS). argv[1]:
 *   signalfd   signalfd4/signalfd records, blocking and non-blocking reads, poll/epoll readiness, mask change
 *   restart    SA_RESTART against EINTR: nanosleep, futex, wait4, pause; a stop/continue restarts sleeps
 *   suspend    rt_sigsuspend restores the mask after the handler
 *   timedwait  rt_sigtimedwait: delivery, timeout (EAGAIN), a signal not in the set, a blocking wait
 *   queueinfo  rt_sigqueueinfo/rt_tgsigqueueinfo spoofing rules, also from a second thread
 *   stopcont   SIGSTOP/SIGCONT with waitid/wait4 job-control status and the CLD_* SIGCHLDs
 *   itimer     alarm, setitimer/getitimer (REAL, VIRTUAL, PROF), POSIX timers (overrun, SIGEV_NONE, errors)
 *   fsintr     FS waits with signals (needs the AB-6 integration patch on V2): pipe read EINTR/SA_RESTART,
 *              SIGPIPE to a handler, ppoll's temporary mask
 * Signals between processes come from a forked child after a pause (100 ms) so the parent is already
 * waiting; only relations are printed, never times or ids of the host. */
#include "abi.h"
#include <fcntl.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/signalfd.h>
#include <sys/wait.h>

#define MS 1000000L

static volatile int hits, last_sig, last_code;
static volatile unsigned long mask_in_handler;

static unsigned long blocked_now(void) {
    unsigned long m = 0;
    syscall(SYS_rt_sigprocmask, SIG_BLOCK, 0, &m, 8);
    return m;
}

static unsigned long bit(int sig) {
    return 1ul << (sig - 1);
}

static void handler(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    hits++;
    last_sig = sig;
    last_code = si->si_code;
    mask_in_handler = blocked_now();
}

static void on(int sig, int flags) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO | flags;
    sigaction(sig, &sa, 0);
}

static void dfl(int sig) {
    signal(sig, SIG_DFL);
}

static void set_mask(unsigned long m) {
    syscall(SYS_rt_sigprocmask, SIG_SETMASK, &m, 0, 8);
}

static void nap(long ns) {
    struct timespec t = {ns / 1000000000L, ns % 1000000000L};
    while (nanosleep(&t, &t) == -1 && errno == EINTR) {
    }
}

/* A child that sends `sig` to the parent after `delay` ns, then (for sig2 > 0) `sig2` after another `delay`,
 * then exits with `code`. */
static pid_t sender(long delay, int sig, int sig2, int code) {
    pid_t parent = getpid();
    pid_t c = fork();
    if (c == 0) {
        nap(delay);
        kill(parent, sig);
        if (sig2 > 0) {
            nap(delay);
            kill(parent, sig2);
        }
        _exit(code);
    }
    return c;
}

static void reap(pid_t c) {
    int st;
    while (waitpid(c, &st, 0) == -1 && errno == EINTR) {
    }
}

static void err(const char *label, long r) {
    if (r == -1)
        printf("%s = -%s\n", label, ename(errno));
    else
        printf("%s = %ld\n", label, r);
}

/* ---------------------------------------------------------------------------------------------------- */

static void show_ssi(const char *label, struct signalfd_siginfo *s) {
    printf("%s: signo %u code %d pid-self %d uid-self %d int %d\n", label, s->ssi_signo, s->ssi_code,
           s->ssi_pid == (unsigned)getpid(), s->ssi_uid == getuid(), s->ssi_int);
}

static void sfd_mode(void) {
    set_mask(bit(SIGUSR1) | bit(SIGUSR2) | bit(SIGRTMIN + 1));
    sigset_t m;
    sigemptyset(&m);
    sigaddset(&m, SIGUSR1);
    sigaddset(&m, SIGKILL);
    int fd = signalfd(-1, &m, SFD_NONBLOCK | SFD_CLOEXEC);
    yes("signalfd made", fd >= 0);
    yes("F_GETFL is O_RDWR|O_NONBLOCK", fcntl(fd, F_GETFL) == (O_RDWR | O_NONBLOCK));
    yes("FD_CLOEXEC", fcntl(fd, F_GETFD) == FD_CLOEXEC);
    struct signalfd_siginfo s[2];
    err("read empty", read(fd, s, sizeof s[0]));
    err("read short buffer", read(fd, s, 100));
    struct pollfd p = {fd, POLLIN, 0};
    err("poll empty", poll(&p, 1, 0));
    kill(getpid(), SIGUSR1);
    err("poll after kill", poll(&p, 1, 0));
    printf("revents %#x\n", p.revents);
    err("read", read(fd, s, sizeof s));
    show_ssi("kill", &s[0]);
    union sigval v = {.sival_int = 42};
    sigqueue(getpid(), SIGUSR1, v);
    err("read", read(fd, s, sizeof s));
    show_ssi("sigqueue", &s[0]);
    /* SIGUSR2 is not in the mask: it stays pending for the thread. */
    kill(getpid(), SIGUSR2);
    err("read (USR2 not in mask)", read(fd, s, sizeof s));
    sigemptyset(&m);
    sigaddset(&m, SIGUSR2);
    sigaddset(&m, SIGRTMIN + 1);
    err("signalfd mask change (same fd)", signalfd(fd, &m, 0) == fd);
    err("read", read(fd, s, sizeof s));
    show_ssi("usr2", &s[0]);
    /* Two queued RT signals in one read. */
    sigqueue(getpid(), SIGRTMIN + 1, v);
    v.sival_int = 43;
    sigqueue(getpid(), SIGRTMIN + 1, v);
    err("read two", read(fd, s, sizeof s));
    printf("ints %d %d\n", s[0].ssi_int, s[1].ssi_int);
    int ep = epoll_create1(0);
    struct epoll_event e = {.events = EPOLLIN, .data.u32 = 7}, out;
    epoll_ctl(ep, EPOLL_CTL_ADD, fd, &e);
    err("epoll empty", epoll_wait(ep, &out, 1, 0));
    syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGUSR2);
    err("epoll after tgkill", epoll_wait(ep, &out, 1, 0));
    printf("data %u\n", out.data.u32);
    err("read", read(fd, s, sizeof s));
    show_ssi("tgkill", &s[0]);
    /* A blocking signalfd: the child's signal ends the read. */
    sigemptyset(&m);
    sigaddset(&m, SIGUSR1);
    int bfd = signalfd(-1, &m, 0);
    pid_t c = sender(100 * MS, SIGUSR1, 0, 0);
    err("blocking read", read(bfd, s, sizeof s[0]));
    yes("from the child", s[0].ssi_pid == (unsigned)c);
    reap(c);
    /* Errors. */
    err("bad flags", syscall(SYS_signalfd4, -1, &m, 8, 1));
    err("bad size", syscall(SYS_signalfd4, -1, &m, 4, 0));
    int pf[2];
    if (pipe(pf) != 0)
        return;
    err("not a signalfd", signalfd(pf[0], &m, 0));
    err("old signalfd", syscall(SYS_signalfd, -1, &m, 8) >= 0);
    err("bad mask pointer", syscall(SYS_signalfd4, -1, KERNEL_ADDR, 8, 0));
}

/* ---------------------------------------------------------------------------------------------------- */

static int *word;

static pid_t futex_poker(long delay) {
    pid_t parent = getpid();
    pid_t c = fork();
    if (c == 0) {
        nap(delay);
        kill(parent, SIGUSR1);
        nap(delay);
        __atomic_store_n(word, 1, __ATOMIC_SEQ_CST);
        syscall(SYS_futex, word, FUTEX_WAKE, 1, 0, 0, 0);
        _exit(0);
    }
    return c;
}

static void restart_mode(void) {
    word = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    for (int r = 0; r < 2; r++) {
        int flags = r ? SA_RESTART : 0;
        printf("-- %s\n", r ? "SA_RESTART" : "no SA_RESTART");
        on(SIGUSR1, flags);
        hits = 0;
        pid_t c = sender(100 * MS, SIGUSR1, 0, 0);
        struct timespec t = {1, 0}, rem = {0, 0};
        err("nanosleep", nanosleep(&t, &rem));
        yes("time left in (0, 1 s)", rem.tv_sec == 0 && rem.tv_nsec > 0);
        printf("handler ran %d\n", hits);
        reap(c);
        hits = 0;
        *word = 0;
        c = futex_poker(100 * MS);
        err("futex wait", syscall(SYS_futex, word, FUTEX_WAIT, 0, 0, 0, 0));
        printf("handler ran %d\n", hits);
        reap(c);
        hits = 0;
        *word = 0;
        c = sender(100 * MS, SIGUSR1, 0, 0);
        struct timespec two = {2, 0};
        err("timed futex wait", syscall(SYS_futex, word, FUTEX_WAIT, 0, &two, 0, 0));
        printf("handler ran %d\n", hits);
        reap(c);
        hits = 0;
        c = fork();
        if (c == 0) {
            nap(100 * MS);
            kill(getppid(), SIGUSR1);
            nap(100 * MS);
            _exit(5);
        }
        int st = 0;
        long w = wait4(c, &st, 0, 0);
        if (w == c)
            printf("wait4 = child, status %d\n", WEXITSTATUS(st));
        else
            err("wait4", w);
        printf("handler ran %d\n", hits);
        if (w != c)
            reap(c);
        hits = 0;
        c = sender(100 * MS, SIGUSR1, 0, 0);
        err("pause", pause());
        printf("handler ran %d\n", hits);
        reap(c);
    }
    /* No handler runs (stop, then continue): the calls go on to their end. */
    printf("-- stop/continue\n");
    pid_t c = sender(100 * MS, SIGSTOP, SIGCONT, 0);
    uint64_t t0 = ns_of(CLOCK_MONOTONIC);
    struct timespec t = {0, 400 * MS};
    err("nanosleep", nanosleep(&t, 0));
    yes("slept the whole time", ns_of(CLOCK_MONOTONIC) - t0 >= 400 * MS);
    reap(c);
    c = sender(100 * MS, SIGSTOP, SIGCONT, 0);
    *word = 0;
    t0 = ns_of(CLOCK_MONOTONIC);
    err("timed futex wait", syscall(SYS_futex, word, FUTEX_WAIT, 0, &t, 0, 0));
    yes("waited the whole time", ns_of(CLOCK_MONOTONIC) - t0 >= 400 * MS);
    reap(c);
    /* restart_syscall with no restart block answers EINTR. */
    err("restart_syscall", syscall(SYS_restart_syscall));
}

/* ---------------------------------------------------------------------------------------------------- */

static void suspend_mode(void) {
    on(SIGUSR1, 0);
    on(SIGUSR2, 0);
    set_mask(bit(SIGUSR1) | bit(SIGTERM));
    pid_t c = sender(100 * MS, SIGUSR1, 0, 0);
    unsigned long temp = bit(SIGTERM);
    long r = syscall(SYS_rt_sigsuspend, &temp, 8);
    err("sigsuspend", r);
    printf("handler ran %d sig %d\n", hits, last_sig);
    yes("handler mask = temporary + the signal", mask_in_handler == (bit(SIGTERM) | bit(SIGUSR1)));
    yes("mask restored after", blocked_now() == (bit(SIGUSR1) | bit(SIGTERM)));
    reap(c);
    /* A blocked signal already pending: sigsuspend returns at once. */
    set_mask(bit(SIGUSR2));
    kill(getpid(), SIGUSR2);
    hits = 0;
    unsigned long none = 0;
    err("sigsuspend with one pending", syscall(SYS_rt_sigsuspend, &none, 8));
    printf("handler ran %d sig %d\n", hits, last_sig);
    yes("mask restored", blocked_now() == bit(SIGUSR2));
    err("bad size", syscall(SYS_rt_sigsuspend, &none, 4));
    err("bad pointer", syscall(SYS_rt_sigsuspend, KERNEL_ADDR, 8));
}

/* ---------------------------------------------------------------------------------------------------- */

static void timedwait_mode(void) {
    set_mask(bit(SIGUSR1) | bit(SIGUSR2));
    unsigned long set = bit(SIGUSR1);
    siginfo_t si;
    struct timespec zero = {0, 0}, short_wait = {0, 50 * MS}, bad = {0, 1000000000L}, two = {2, 0};
    err("nothing pending", syscall(SYS_rt_sigtimedwait, &set, &si, &zero, 8));
    uint64_t t0 = ns_of(CLOCK_MONOTONIC);
    err("50 ms", syscall(SYS_rt_sigtimedwait, &set, &si, &short_wait, 8));
    yes("waited 50 ms", ns_of(CLOCK_MONOTONIC) - t0 >= 50 * MS);
    kill(getpid(), SIGUSR1);
    memset(&si, 0, sizeof si);
    err("pending", syscall(SYS_rt_sigtimedwait, &set, &si, &zero, 8));
    printf("si_code %d pid-self %d\n", si.si_code, si.si_pid == getpid());
    kill(getpid(), SIGUSR2);
    err("USR2 not in the set", syscall(SYS_rt_sigtimedwait, &set, &si, &zero, 8));
    unsigned long p = 0;
    syscall(SYS_rt_sigpending, &p, 8);
    yes("USR2 still pending", p == bit(SIGUSR2));
    err("bad timeout", syscall(SYS_rt_sigtimedwait, &set, &si, &bad, 8));
    err("bad size", syscall(SYS_rt_sigtimedwait, &set, &si, &zero, 4));
    pid_t c = sender(100 * MS, SIGUSR1, 0, 0);
    memset(&si, 0, sizeof si);
    err("blocking wait", syscall(SYS_rt_sigtimedwait, &set, &si, &two, 8));
    yes("from the child", si.si_pid == c && si.si_code == SI_USER);
    reap(c);
    set = bit(SIGUSR2);
    err("NULL info", syscall(SYS_rt_sigtimedwait, &set, 0, &zero, 8));
    /* An RT signal with its value. */
    set = bit(SIGRTMIN + 3);
    set_mask(set);
    union sigval v = {.sival_int = 99};
    sigqueue(getpid(), SIGRTMIN + 3, v);
    err("rt", syscall(SYS_rt_sigtimedwait, &set, &si, &zero, 8) == SIGRTMIN + 3);
    printf("si_code %d value %d\n", si.si_code, si.si_value.sival_int);
}

/* ---------------------------------------------------------------------------------------------------- */

static long queueinfo(int pid, int sig, int code) {
    siginfo_t si;
    memset(&si, 0, sizeof si);
    si.si_code = code;
    si.si_pid = getpid();
    si.si_uid = getuid();
    return syscall(SYS_rt_sigqueueinfo, pid, sig, &si);
}

static long tgqueueinfo(int tgid, int tid, int sig, int code) {
    siginfo_t si;
    memset(&si, 0, sizeof si);
    si.si_code = code;
    return syscall(SYS_rt_tgsigqueueinfo, tgid, tid, sig, &si);
}

static pid_t main_tid;

static void *second(void *a) {
    (void)a;
    err("thread: sigqueueinfo(own pid, SI_USER)", queueinfo(getpid(), SIGUSR1, SI_USER));
    err("thread: sigqueueinfo(own pid, SI_QUEUE)", queueinfo(getpid(), SIGUSR1, SI_QUEUE));
    err("thread: tgsigqueueinfo(main thread, SI_USER)", tgqueueinfo(getpid(), main_tid, SIGUSR1, SI_USER));
    err("thread: tgsigqueueinfo(itself, SI_USER)", tgqueueinfo(getpid(), syscall(SYS_gettid), SIGUSR1, SI_USER));
    return 0;
}

static void queueinfo_mode(void) {
    set_mask(bit(SIGUSR1));
    main_tid = syscall(SYS_gettid);
    err("self, SI_USER", queueinfo(getpid(), SIGUSR1, SI_USER));
    err("self, SI_KERNEL", queueinfo(getpid(), SIGUSR1, SI_KERNEL));
    err("self, SI_TKILL", queueinfo(getpid(), SIGUSR1, SI_TKILL));
    err("self, SI_QUEUE", queueinfo(getpid(), SIGUSR1, SI_QUEUE));
    err("tg self, SI_USER", tgqueueinfo(getpid(), main_tid, SIGUSR1, SI_USER));
    pid_t c = fork();
    if (c == 0) {
        pause();
        _exit(0);
    }
    err("child, SI_USER", queueinfo(c, SIGUSR1, SI_USER));
    err("child, SI_TKILL", queueinfo(c, SIGUSR1, SI_TKILL));
    err("child, SI_QUEUE (default: ends it)", queueinfo(c, SIGTERM, SI_QUEUE));
    int st = 0;
    waitpid(c, &st, 0);
    yes("child ended by SIGTERM", WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM);
    err("bad signal", queueinfo(getpid(), 65, SI_QUEUE));
    err("no such pid", queueinfo(0x3fffffff, SIGUSR1, SI_QUEUE));
    err("bad pointer", syscall(SYS_rt_sigqueueinfo, getpid(), SIGUSR1, KERNEL_ADDR));
    pthread_t t;
    pthread_create(&t, 0, second, 0);
    pthread_join(t, 0);
    /* What arrived: count the SIGUSR1 entries (a legacy signal: one at a time). */
    unsigned long set = bit(SIGUSR1);
    struct timespec zero = {0, 0};
    siginfo_t si;
    int n = 0;
    while (syscall(SYS_rt_sigtimedwait, &set, &si, &zero, 8) == SIGUSR1)
        n++;
    printf("SIGUSR1 taken %d time(s), last code %d\n", n, si.si_code);
}

/* ---------------------------------------------------------------------------------------------------- */

static void take_chld(const char *what) {
    unsigned long set = bit(SIGCHLD);
    struct timespec two = {2, 0};
    siginfo_t si;
    memset(&si, 0, sizeof si);
    long r = syscall(SYS_rt_sigtimedwait, &set, &si, &two, 8);
    if (r != SIGCHLD) {
        err(what, r);
        return;
    }
    printf("%s: SIGCHLD code %d status %d\n", what, si.si_code, si.si_status);
}

static void stopcont_mode(void) {
    set_mask(bit(SIGCHLD));
    pid_t c = fork();
    if (c == 0) {
        for (;;)
            pause();
    }
    nap(50 * MS);
    siginfo_t si;
    kill(c, SIGSTOP);
    memset(&si, 0, sizeof si);
    err("waitid(WSTOPPED)", waitid(P_PID, c, &si, WSTOPPED));
    printf("code %d status %d pid-child %d\n", si.si_code, si.si_status, si.si_pid == c);
    take_chld("stop");
    err("waitid again (WNOHANG)", waitid(P_PID, c, &si, WSTOPPED | WNOHANG));
    printf("pid after WNOHANG %d\n", si.si_pid == c);
    kill(c, SIGCONT);
    memset(&si, 0, sizeof si);
    err("waitid(WCONTINUED)", waitid(P_PID, c, &si, WCONTINUED));
    printf("code %d status %d\n", si.si_code, si.si_status);
    take_chld("continue");
    /* wait4's view: WUNTRACED and WCONTINUED statuses. */
    kill(c, SIGSTOP);
    int st = 0;
    err("wait4(WUNTRACED)", wait4(c, &st, WUNTRACED, 0) == c);
    printf("stopped %d sig %d raw %#x\n", WIFSTOPPED(st), WSTOPSIG(st), st);
    take_chld("stop 2");
    kill(c, SIGCONT);
    err("wait4(WCONTINUED)", wait4(c, &st, WCONTINUED, 0) == c);
    printf("continued %d raw %#x\n", WIFCONTINUED(st), st);
    take_chld("continue 2");
    /* A stop while SA_NOCLDSTOP is set gives no SIGCHLD. */
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = SIG_DFL;
    sa.sa_flags = SA_NOCLDSTOP;
    sigaction(SIGCHLD, &sa, 0);
    kill(c, SIGSTOP);
    err("waitid(WSTOPPED) with SA_NOCLDSTOP", waitid(P_PID, c, &si, WSTOPPED));
    unsigned long p = 0;
    syscall(SYS_rt_sigpending, &p, 8);
    yes("no SIGCHLD pending", (p & bit(SIGCHLD)) == 0);
    dfl(SIGCHLD);
    /* SIGTERM while stopped waits for the continue (`wants_signal`: a stopped task takes only SIGKILL). */
    kill(c, SIGTERM);
    nap(50 * MS);
    memset(&si, 0, sizeof si);
    err("still stopped (WEXITED|WNOHANG)", waitid(P_PID, c, &si, WEXITED | WNOHANG));
    printf("pid %d\n", si.si_pid == c);
    kill(c, SIGCONT);
    memset(&si, 0, sizeof si);
    err("waitid(WEXITED)", waitid(P_PID, c, &si, WEXITED));
    printf("code %d status %d\n", si.si_code, si.si_status);
    take_chld("end");
}

/* ---------------------------------------------------------------------------------------------------- */

struct ksigevent {
    union sigval value;
    int signo;
    int notify;
    int tid;
    int pad[11];
};

struct kspec {
    struct timespec interval, value;
};

static long tcreate(int clock, struct ksigevent *ev, int *id) {
    return syscall(SYS_timer_create, clock, ev, id);
}

static long tset(int id, int flags, long value_ns, long interval_ns, struct kspec *old) {
    struct kspec s = {{interval_ns / 1000000000L, interval_ns % 1000000000L}, {value_ns / 1000000000L, value_ns % 1000000000L}};
    return syscall(SYS_timer_settime, id, flags, &s, old);
}

static volatile int alarms, vtalrms, profs;

static void count(int sig) {
    if (sig == SIGALRM)
        alarms++;
    else if (sig == SIGVTALRM)
        vtalrms++;
    else
        profs++;
}

/* Burns user-mode CPU (the clock is read rarely: without a vDSO each read is system time) until `flag` or 5 s. */
static void burn_until(volatile int *flag) {
    uint64_t t0 = ns_of(CLOCK_MONOTONIC);
    volatile unsigned long x = 0;
    while (!*flag && ns_of(CLOCK_MONOTONIC) - t0 < 5000 * MS)
        for (int i = 0; i < 1000000 && !*flag; i++)
            x++;
}

static void itimer_mode(void) {
    printf("alarm(5) = %u\n", alarm(5));
    printf("alarm(0) = %u\n", alarm(0));
    printf("alarm(0) again = %u\n", alarm(0));
    signal(SIGALRM, count);
    signal(SIGVTALRM, count);
    signal(SIGPROF, count);
    struct itimerval v, o;
    memset(&v, 0, sizeof v);
    v.it_value.tv_usec = 50000;
    err("setitimer(REAL, 50 ms)", setitimer(ITIMER_REAL, &v, &o));
    yes("old was disarmed", o.it_value.tv_sec == 0 && o.it_value.tv_usec == 0);
    getitimer(ITIMER_REAL, &o);
    yes("getitimer in (0, 50 ms]", o.it_value.tv_sec == 0 && o.it_value.tv_usec > 0 && o.it_value.tv_usec <= 50000);
    while (!alarms)
        pause();
    printf("SIGALRM %d\n", alarms);
    getitimer(ITIMER_REAL, &o);
    yes("one-shot disarmed after", o.it_value.tv_sec == 0 && o.it_value.tv_usec == 0);
    /* Periodic: three SIGALRMs, then off. */
    alarms = 0;
    v.it_interval.tv_usec = 20000;
    v.it_value.tv_usec = 20000;
    setitimer(ITIMER_REAL, &v, 0);
    while (alarms < 3)
        pause();
    memset(&v, 0, sizeof v);
    setitimer(ITIMER_REAL, &v, &o);
    yes("periodic: interval read back", o.it_interval.tv_sec == 0 && o.it_interval.tv_usec == 20000);
    printf("periodic SIGALRMs >= 3: %s\n", alarms >= 3 ? "yes" : "no");
    /* A blocked SIGALRM pauses a periodic ITIMER_REAL: getitimer reads 0 until it is taken. */
    set_mask(bit(SIGALRM));
    v.it_interval.tv_usec = 10000;
    v.it_value.tv_usec = 10000;
    setitimer(ITIMER_REAL, &v, 0);
    nap(50 * MS);
    getitimer(ITIMER_REAL, &o);
    yes("paused while SIGALRM is pending", o.it_value.tv_sec == 0 && o.it_value.tv_usec == 0);
    yes("interval kept", o.it_interval.tv_usec == 10000);
    unsigned long set = bit(SIGALRM);
    struct timespec zero = {0, 0};
    siginfo_t si;
    err("take SIGALRM", syscall(SYS_rt_sigtimedwait, &set, &si, &zero, 8));
    printf("si_code %d\n", si.si_code);
    getitimer(ITIMER_REAL, &o);
    yes("running again", o.it_value.tv_sec == 0 && o.it_value.tv_usec > 0 && o.it_value.tv_usec <= 10000);
    memset(&v, 0, sizeof v);
    setitimer(ITIMER_REAL, &v, 0);
    syscall(SYS_rt_sigtimedwait, &set, &si, &zero, 8);
    set_mask(0);
    /* CPU itimers. */
    getitimer(ITIMER_VIRTUAL, &o);
    yes("VIRTUAL off", o.it_value.tv_sec == 0 && o.it_value.tv_usec == 0);
    v.it_value.tv_usec = 20000;
    setitimer(ITIMER_VIRTUAL, &v, 0);
    getitimer(ITIMER_VIRTUAL, &o);
    yes("VIRTUAL reads up to value + a tick", o.it_value.tv_sec == 0 && o.it_value.tv_usec > 0 && o.it_value.tv_usec <= 21000);
    burn_until(&vtalrms);
    printf("SIGVTALRM %d\n", vtalrms);
    setitimer(ITIMER_PROF, &v, 0);
    burn_until(&profs);
    printf("SIGPROF %d\n", profs);
    err("which 5", setitimer(5, &v, 0));
    err("getitimer which 5", getitimer(5, &o));
    v.it_value.tv_usec = 1000000;
    err("usec 1e6", setitimer(ITIMER_REAL, &v, 0));
    err("getitimer NULL", syscall(SYS_getitimer, ITIMER_REAL, 0));
    err("setitimer bad pointer", syscall(SYS_setitimer, ITIMER_REAL, KERNEL_ADDR, 0));

    /* POSIX timers. */
    int id = -1, id2 = -1;
    err("timer_create(MONOTONIC, NULL)", tcreate(CLOCK_MONOTONIC, 0, &id));
    printf("first id %d\n", id);
    struct kspec cur;
    err("gettime disarmed", syscall(SYS_timer_gettime, id, &cur));
    yes("all zero", !cur.value.tv_sec && !cur.value.tv_nsec && !cur.interval.tv_sec && !cur.interval.tv_nsec);
    set_mask(bit(SIGALRM) | bit(SIGRTMIN + 2));
    err("settime 30 ms", tset(id, 0, 30 * MS, 0, 0));
    set = bit(SIGALRM);
    struct timespec two = {2, 0};
    memset(&si, 0, sizeof si);
    err("default sigevent: SIGALRM", syscall(SYS_rt_sigtimedwait, &set, &si, &two, 8));
    printf("code %d timerid-is-id %d value-is-id %d overrun %d\n", si.si_code, si.si_timerid == id, si.si_value.sival_int == id,
           si.si_overrun);
    struct ksigevent ev;
    memset(&ev, 0, sizeof ev);
    ev.notify = SIGEV_SIGNAL;
    ev.signo = SIGRTMIN + 2;
    ev.value.sival_int = 77;
    err("timer_create(REALTIME, RT)", tcreate(CLOCK_REALTIME, &ev, &id2));
    printf("next id is +1: %d\n", id2 == id + 1);
    err("settime periodic 10 ms", tset(id2, 0, 10 * MS, 10 * MS, 0));
    nap(105 * MS);
    set = bit(SIGRTMIN + 2);
    memset(&si, 0, sizeof si);
    err("take", syscall(SYS_rt_sigtimedwait, &set, &si, &zero, 8) == SIGRTMIN + 2);
    printf("code %d value %d timerid-ok %d overrun >= 9: %s\n", si.si_code, si.si_value.sival_int, si.si_timerid == id2,
           si.si_overrun >= 9 ? "yes" : "no");
    yes("getoverrun = si_overrun", syscall(SYS_timer_getoverrun, id2) == si.si_overrun);
    syscall(SYS_timer_gettime, id2, &cur);
    yes("gettime: running, interval 10 ms", cur.interval.tv_nsec == 10 * MS && cur.value.tv_sec == 0 && cur.value.tv_nsec > 0 &&
                                                 cur.value.tv_nsec <= 10 * MS);
    struct kspec old;
    err("settime 1 s one-shot", tset(id2, 0, 1000 * MS, 0, &old));
    yes("old interval 10 ms", old.interval.tv_nsec == 10 * MS);
    syscall(SYS_timer_gettime, id2, &cur);
    yes("gettime in (0.9, 1] s", (cur.value.tv_sec == 0 && cur.value.tv_nsec > 900 * MS) || (cur.value.tv_sec == 1 && cur.value.tv_nsec == 0));
    err("disarm", tset(id2, 0, 0, 0, 0));
    syscall(SYS_rt_sigtimedwait, &set, &si, &zero, 8);
    /* An absolute time in the past fires at once. */
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    int id3 = -1;
    memset(&ev, 0, sizeof ev);
    ev.notify = SIGEV_SIGNAL;
    ev.signo = SIGRTMIN + 2;
    tcreate(CLOCK_MONOTONIC, &ev, &id3);
    struct kspec past = {{0, 0}, {now.tv_sec > 1 ? now.tv_sec - 1 : 0, 1}};
    err("absolute in the past", syscall(SYS_timer_settime, id3, TIMER_ABSTIME, &past, 0));
    err("fired", syscall(SYS_rt_sigtimedwait, &set, &si, &two, 8) == SIGRTMIN + 2);
    /* SIGEV_NONE: no signal, gettime counts down. */
    int idn = -1;
    memset(&ev, 0, sizeof ev);
    ev.notify = SIGEV_NONE;
    err("timer_create(SIGEV_NONE)", tcreate(CLOCK_MONOTONIC, &ev, &idn));
    tset(idn, 0, 30 * MS, 0, 0);
    syscall(SYS_timer_gettime, idn, &cur);
    yes("SIGEV_NONE counts down", cur.value.tv_sec == 0 && cur.value.tv_nsec > 0 && cur.value.tv_nsec <= 30 * MS);
    nap(40 * MS);
    syscall(SYS_timer_gettime, idn, &cur);
    yes("SIGEV_NONE expired reads 0", cur.value.tv_sec == 0 && cur.value.tv_nsec == 0);
    /* SIGEV_THREAD_ID to this thread; to a process that is not ours: EINVAL. */
    int idt = -1;
    memset(&ev, 0, sizeof ev);
    ev.notify = SIGEV_THREAD_ID;
    ev.signo = SIGRTMIN + 2;
    ev.tid = syscall(SYS_gettid);
    err("SIGEV_THREAD_ID self", tcreate(CLOCK_MONOTONIC, &ev, &idt));
    ev.tid = 1;
    err("SIGEV_THREAD_ID of pid 1", tcreate(CLOCK_MONOTONIC, &ev, &idt));
    /* Errors. */
    memset(&ev, 0, sizeof ev);
    ev.notify = 3;
    ev.signo = SIGUSR1;
    err("notify 3", tcreate(CLOCK_MONOTONIC, &ev, &idt));
    ev.notify = SIGEV_SIGNAL;
    ev.signo = 0;
    err("signo 0", tcreate(CLOCK_MONOTONIC, &ev, &idt));
    ev.signo = 65;
    err("signo 65", tcreate(CLOCK_MONOTONIC, &ev, &idt));
    err("clock 10", tcreate(10, 0, &idt));
    err("MONOTONIC_RAW", tcreate(CLOCK_MONOTONIC_RAW, 0, &idt));
    err("REALTIME_COARSE", tcreate(CLOCK_REALTIME_COARSE, 0, &idt));
    err("bad sigevent pointer", tcreate(CLOCK_MONOTONIC, KERNEL_ADDR, &idt));
    err("PROCESS_CPUTIME", tcreate(CLOCK_PROCESS_CPUTIME_ID, 0, &idt));
    err("THREAD_CPUTIME", tcreate(CLOCK_THREAD_CPUTIME_ID, 0, &idt));
    err("settime NULL", syscall(SYS_timer_settime, id, 0, 0, 0));
    struct kspec badspec = {{0, 0}, {0, 1000000000L}};
    err("settime nsec 1e9", syscall(SYS_timer_settime, id, 0, &badspec, 0));
    err("settime unknown id", tset(12345, 0, MS, 0, 0));
    err("gettime unknown id", syscall(SYS_timer_gettime, 12345, &cur));
    err("getoverrun unknown id", syscall(SYS_timer_getoverrun, 12345));
    err("delete", syscall(SYS_timer_delete, id));
    err("delete again", syscall(SYS_timer_delete, id));
    err("gettime deleted", syscall(SYS_timer_gettime, id, &cur));
    err("delete -1", syscall(SYS_timer_delete, -1));
    /* A CPU-time timer fires as the process burns CPU. */
    int idc = -1;
    memset(&ev, 0, sizeof ev);
    ev.notify = SIGEV_SIGNAL;
    ev.signo = SIGPROF;
    profs = 0;
    set_mask(0);
    err("timer_create(PROCESS_CPUTIME, SIGPROF)", tcreate(CLOCK_PROCESS_CPUTIME_ID, &ev, &idc));
    tset(idc, 0, 20 * MS, 0, 0);
    burn_until(&profs);
    printf("CPU timer SIGPROF %d\n", profs);
}

/* ---------------------------------------------------------------------------------------------------- */

static void fsintr_mode(void) {
    int pf[2];
    char b[8];
    for (int r = 0; r < 2; r++) {
        on(SIGUSR1, r ? SA_RESTART : 0);
        hits = 0;
        if (pipe(pf) != 0)
            return;
        pid_t parent = getpid();
        pid_t c = fork();
        if (c == 0) {
            nap(100 * MS);
            kill(parent, SIGUSR1);
            nap(100 * MS);
            if (write(pf[1], "x", 1) != 1)
                _exit(1);
            _exit(0);
        }
        close(pf[1]);
        err(r ? "pipe read, SA_RESTART" : "pipe read, no SA_RESTART", read(pf[0], b, sizeof b));
        printf("handler ran %d\n", hits);
        reap(c);
        close(pf[0]);
    }
    /* SIGPIPE to a handler: the write answers EPIPE and the handler runs. */
    on(SIGPIPE, 0);
    hits = 0;
    if (pipe(pf) != 0)
        return;
    close(pf[0]);
    err("write to a closed pipe", write(pf[1], "x", 1));
    printf("handler ran %d sig %d\n", hits, last_sig);
    close(pf[1]);
    /* ppoll's temporary mask: USR1 blocked outside, let through while it waits. */
    on(SIGUSR1, 0);
    set_mask(bit(SIGUSR1));
    hits = 0;
    pid_t c = sender(100 * MS, SIGUSR1, 0, 0);
    sigset_t m;
    sigemptyset(&m);
    struct timespec two = {2, 0};
    err("ppoll", ppoll(0, 0, &two, &m));
    printf("handler ran %d\n", hits);
    yes("handler saw USR1 blocked (its own)", (mask_in_handler & bit(SIGUSR1)) != 0);
    yes("mask restored after", blocked_now() == bit(SIGUSR1));
    reap(c);
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "signalfd";
    printf("mode %s\n", m);
    if (!strcmp(m, "signalfd"))
        sfd_mode();
    else if (!strcmp(m, "restart"))
        restart_mode();
    else if (!strcmp(m, "suspend"))
        suspend_mode();
    else if (!strcmp(m, "timedwait"))
        timedwait_mode();
    else if (!strcmp(m, "queueinfo"))
        queueinfo_mode();
    else if (!strcmp(m, "stopcont"))
        stopcont_mode();
    else if (!strcmp(m, "itimer"))
        itimer_mode();
    else if (!strcmp(m, "fsintr"))
        fsintr_mode();
    else
        return 99;
    fflush(stdout);
    printf("done\n");
    return 0;
}
