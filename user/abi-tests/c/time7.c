/* abi-time7 (L7-SYNC): Linux 7.3.1's new prctl options, auxiliary clocks and POSIX timer rules, for the ABI diff
 * (CachyOS is the oracle). Modes (argv[1]):
 *   prctl    PR_TIMER_CREATE_RESTORE_IDS, PR_FUTEX_HASH, PR_RSEQ_SLICE_EXTENSION (the rseq area musl never
 *            registers), and the x86-less shadow stack and CFI options
 *   clocks   CLOCK_AUX .. CLOCK_AUX_LAST (16..23): clock_gettime, clock_getres, clock_nanosleep, timer_create
 *   restore  timer_create with PR_TIMER_CREATE_RESTORE_IDS: the requested id, its errors, the counter after it,
 *            two threads asking at once
 *   timers   a periodic timer whose signal is ignored is parked and re-armed after the signal is unignored;
 *            timer_settime and timer_delete void a signal that is still queued; an expiry already past at
 *            timer_settime queues the signal before the call returns
 * Raw syscalls (musl adds nothing); results, errno names and yes/no relations only. */
#include "abi.h"
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <sys/prctl.h>
#include <sys/wait.h>

#define P_RESTORE 77
#define P_FUTEX_HASH 78
#define P_RSEQ_SLICE 79

static long pr(long o, long a2, long a3, long a4, long a5) {
    return syscall(SYS_prctl, o, a2, a3, a4, a5);
}

static void *idle_thread(void *arg) {
    pause();
    return arg;
}

static void prctls(void) {
    puts("== PR_TIMER_CREATE_RESTORE_IDS");
    show("GET", pr(P_RESTORE, 2, 0, 0, 0));
    show("ON", pr(P_RESTORE, 1, 0, 0, 0));
    show("GET", pr(P_RESTORE, 2, 0, 0, 0));
    show("ON, arg3", pr(P_RESTORE, 1, 1, 0, 0));
    show("ON, arg5", pr(P_RESTORE, 1, 0, 0, 1));
    show("3", pr(P_RESTORE, 3, 0, 0, 0));
    pid_t c = fork();
    if (c == 0)
        _exit((int)pr(P_RESTORE, 2, 0, 0, 0));
    int st;
    waitpid(c, &st, 0);
    printf("a fork child reads %d\n", WEXITSTATUS(st));
    show("OFF", pr(P_RESTORE, 0, 0, 0, 0));
    show("GET", pr(P_RESTORE, 2, 0, 0, 0));

    puts("== PR_FUTEX_HASH");
    show("GET, one thread", pr(P_FUTEX_HASH, 2, 0, 0, 0));
    show("GET, arg3 is not looked at", pr(P_FUTEX_HASH, 2, 7, 7, 7));
    show("3", pr(P_FUTEX_HASH, 3, 0, 0, 0));
    show("0", pr(P_FUTEX_HASH, 0, 0, 0, 0));
    show("SET 3", pr(P_FUTEX_HASH, 1, 3, 0, 0));
    show("SET 1", pr(P_FUTEX_HASH, 1, 1, 0, 0));
    show("SET 16, arg4", pr(P_FUTEX_HASH, 1, 16, 1, 0));
    pthread_t t;
    pthread_create(&t, 0, idle_thread, 0);
    show("GET, two threads", pr(P_FUTEX_HASH, 2, 0, 0, 0));
    show("SET 32", pr(P_FUTEX_HASH, 1, 32, 0, 0));
    show("GET", pr(P_FUTEX_HASH, 2, 0, 0, 0));
    show("SET 1 << 32 | 64 (low 32 bits: 64)", pr(P_FUTEX_HASH, 1, (1l << 32) | 64, 0, 0));
    show("GET", pr(P_FUTEX_HASH, 2, 0, 0, 0));
    show("SET 0 (the global hash)", pr(P_FUTEX_HASH, 1, 0, 0, 0));
    show("GET", pr(P_FUTEX_HASH, 2, 0, 0, 0));
    show("SET 16 after it", pr(P_FUTEX_HASH, 1, 16, 0, 0));
    show("SET 0 again", pr(P_FUTEX_HASH, 1, 0, 0, 0));
    c = fork();
    if (c == 0)
        _exit((int)pr(P_FUTEX_HASH, 2, 0, 0, 0));
    waitpid(c, &st, 0);
    printf("a fork child (one thread) reads %d\n", WEXITSTATUS(st));

    puts("== PR_RSEQ_SLICE_EXTENSION");
    show("GET", pr(P_RSEQ_SLICE, 1, 0, 0, 0));
    show("GET, arg3", pr(P_RSEQ_SLICE, 1, 1, 0, 0));
    show("GET, arg4", pr(P_RSEQ_SLICE, 1, 0, 1, 0));
    show("SET 2", pr(P_RSEQ_SLICE, 2, 2, 0, 0));
    show("SET 1, no rseq area", pr(P_RSEQ_SLICE, 2, 1, 0, 0));
    show("SET 0, no rseq area", pr(P_RSEQ_SLICE, 2, 0, 0, 0));
    show("3", pr(P_RSEQ_SLICE, 3, 0, 0, 0));

    puts("== options without x86 support");
    for (long o = 71; o <= 81; o++) {
        if (o >= 77 && o <= 79)
            continue;
        char l[32];
        snprintf(l, sizeof l, "option %ld", o);
        show(l, pr(o, 0, 0, 0, 0));
    }
}

static void clocks(void) {
    puts("== aux clocks");
    struct timespec ts = {5, 5};
    for (int id = 15; id <= 24; id++) {
        char l[64];
        snprintf(l, sizeof l, "clock_gettime(%d)", id);
        show(l, syscall(SYS_clock_gettime, id, &ts));
        ts.tv_sec = 5;
        ts.tv_nsec = 5;
        snprintf(l, sizeof l, "clock_getres(%d)", id);
        long r = show(l, syscall(SYS_clock_getres, id, &ts));
        if (r == 0)
            printf("  resolution %ld s %ld ns\n", (long)ts.tv_sec, ts.tv_nsec);
        snprintf(l, sizeof l, "clock_getres(%d, NULL)", id);
        show(l, syscall(SYS_clock_getres, id, 0));
    }
    struct timespec rq = {0, 1000};
    show("clock_nanosleep(16)", syscall(SYS_clock_nanosleep, 16, 0, &rq, 0));
    show("clock_nanosleep(16, bad request: EOPNOTSUPP first)", syscall(SYS_clock_nanosleep, 16, 0, KERNEL_ADDR, 0));
    show("clock_nanosleep(24)", syscall(SYS_clock_nanosleep, 24, 0, &rq, 0));
    int id = -1;
    show("timer_create(16)", syscall(SYS_timer_create, 16, 0, &id));
    show("timer_create(23)", syscall(SYS_timer_create, 23, 0, &id));
    show("timer_create(24)", syscall(SYS_timer_create, 24, 0, &id));
    show("clock_gettime(16, kernel address)", syscall(SYS_clock_gettime, 16, KERNEL_ADDR));
    show("clock_getres(16, kernel address)", syscall(SYS_clock_getres, 16, KERNEL_ADDR));
}

#define RACE_N 2000
static int race_wrong[2], race_failed[2];
static atomic_int race_go;

/* Thread `arg` asks for ids 5000 + 2i + arg: the other thread's calls run in between. */
static void *race_thread(void *arg) {
    long k = (long)arg;
    atomic_fetch_add(&race_go, 1);
    while (atomic_load(&race_go) < 2) {
    }
    for (int i = 0; i < RACE_N; i++) {
        int want = 5000 + 2 * i + (int)k, id = want;
        if (syscall(SYS_timer_create, CLOCK_MONOTONIC, 0, &id))
            race_failed[k]++;
        else if (id != want)
            race_wrong[k]++;
    }
    return 0;
}

/* Two threads in restore mode at once: every call gets exactly the id it asked for. */
static void restore_race(void) {
    puts("== two threads restoring ids at once");
    show("RESTORE_IDS on", pr(P_RESTORE, 1, 0, 0, 0));
    pthread_t a, b;
    pthread_create(&a, 0, race_thread, (void *)0);
    pthread_create(&b, 0, race_thread, (void *)1);
    pthread_join(a, 0);
    pthread_join(b, 0);
    pr(P_RESTORE, 0, 0, 0, 0);
    printf("  calls with another id than asked: %d, failed: %d\n", race_wrong[0] + race_wrong[1], race_failed[0] + race_failed[1]);
    for (int i = 0; i < 2 * RACE_N; i++)
        syscall(SYS_timer_delete, 5000 + i);
}

static void restore(void) {
    puts("== timer_create restoring ids");
    int id = 1234, got;
    show("RESTORE_IDS on", pr(P_RESTORE, 1, 0, 0, 0));
    show("timer_create(id 1234)", syscall(SYS_timer_create, CLOCK_MONOTONIC, 0, &id));
    printf("  id %d\n", id);
    id = 1234;
    show("timer_create(id 1234 again)", syscall(SYS_timer_create, CLOCK_MONOTONIC, 0, &id));
    id = -1;
    show("timer_create(id -1)", syscall(SYS_timer_create, CLOCK_MONOTONIC, 0, &id));
    show("timer_create(id unreadable)", syscall(SYS_timer_create, CLOCK_MONOTONIC, 0, KERNEL_ADDR));
    id = 7;
    show("timer_create(id 7, bad clock: EINVAL first)", syscall(SYS_timer_create, 99, 0, &id));
    struct sigevent ev = {0};
    ev.sigev_notify = 77;
    id = 40;
    show("timer_create(id 40, bad sigevent)", syscall(SYS_timer_create, CLOCK_MONOTONIC, &ev, &id));
    show("RESTORE_IDS off", pr(P_RESTORE, 0, 0, 0, 0));
    got = -1;
    show("timer_create", syscall(SYS_timer_create, CLOCK_MONOTONIC, 0, &got));
    printf("  id %d (after the last requested one)\n", got);
    got = -1;
    show("timer_create", syscall(SYS_timer_create, CLOCK_MONOTONIC, 0, &got));
    printf("  id %d\n", got);
    restore_race();
}

#define SIG (SIGRTMIN + 2)

static volatile int caught;
static void on_sig(int s) {
    (void)s;
    caught++;
}

static int new_timer(int periodic_ms, int first_ms) {
    struct sigevent ev = {0};
    ev.sigev_notify = SIGEV_SIGNAL;
    ev.sigev_signo = SIG;
    int id = -1;
    syscall(SYS_timer_create, CLOCK_MONOTONIC, &ev, &id);
    struct itimerspec its = {{0, periodic_ms * 1000000l}, {0, first_ms * 1000000l}};
    syscall(SYS_timer_settime, id, 0, &its, 0);
    return id;
}

static int pending(void) {
    sigset_t s;
    sigpending(&s);
    return sigismember(&s, SIG);
}

static long take(siginfo_t *si) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIG);
    struct timespec zero = {0, 0};
    return syscall(SYS_rt_sigtimedwait, &s, si, &zero, 8);
}

static void timers(void) {
    sigset_t only;
    sigemptyset(&only);
    sigaddset(&only, SIG);
    siginfo_t si;

    puts("== ignored periodic timer");
    signal(SIG, SIG_IGN);
    int id = new_timer(10, 10);
    usleep(45000);
    struct itimerspec cur;
    syscall(SYS_timer_gettime, id, &cur);
    yes("timer_gettime: still counts down", cur.it_value.tv_sec == 0 && cur.it_value.tv_nsec > 0 && cur.it_interval.tv_nsec == 10000000);
    sigprocmask(SIG_BLOCK, &only, 0);
    signal(SIG, on_sig);
    yes("pending as soon as the signal is unignored", pending());
    long r = take(&si);
    printf("taken: %s\n", r == SIG ? "yes" : ename(errno));
    yes("  si_overrun counts the periods while parked (>= 2)", r == SIG && si.si_overrun >= 2);
    usleep(25000);
    yes("re-armed by the delivery: pending again", pending());
    syscall(SYS_timer_delete, id);
    while (take(&si) == SIG) {
    }

    puts("== settime and delete void a queued signal");
    id = new_timer(0, 5);
    usleep(20000);
    yes("one-shot queued", pending());
    struct itimerspec off = {{0, 0}, {0, 0}};
    syscall(SYS_timer_settime, id, 0, &off, 0);
    yes("  still pending after timer_settime", pending());
    r = take(&si);
    printf("  sigtimedwait: %s\n", r == SIG ? "delivered" : ename(errno));
    struct itimerspec again = {{0, 0}, {0, 5000000}};
    syscall(SYS_timer_settime, id, 0, &again, 0);
    usleep(20000);
    yes("fired again", pending());
    syscall(SYS_timer_settime, id, 0, &again, 0);
    usleep(20000);
    r = take(&si);
    printf("  set again and fired again while queued: %s, si_overrun %d\n", r == SIG ? "delivered" : ename(errno), r == SIG ? si.si_overrun : -1);
    r = take(&si);
    printf("  a second one: %s\n", r == SIG ? "delivered" : ename(errno));
    syscall(SYS_timer_settime, id, 0, &again, 0);
    usleep(20000);
    syscall(SYS_timer_delete, id);
    yes("  pending after timer_delete", pending());
    r = take(&si);
    printf("  sigtimedwait after timer_delete: %s\n", r == SIG ? "delivered" : ename(errno));

    puts("== expiry already past at timer_settime");
    id = new_timer(0, 0);
    struct itimerspec past = {{0, 0}, {0, 1}};
    syscall(SYS_timer_settime, id, TIMER_ABSTIME, &past, 0);
    yes("  pending when timer_settime returns", pending());
    r = take(&si);
    printf("  sigtimedwait at once: %s\n", r == SIG ? "delivered" : ename(errno));
    syscall(SYS_timer_delete, id);
    printf("caught by the handler: %d\n", caught);
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    const char *m = argc > 1 ? argv[1] : "";
    if (!strcmp(m, "prctl"))
        prctls();
    else if (!strcmp(m, "clocks"))
        clocks();
    else if (!strcmp(m, "restore"))
        restore();
    else if (!strcmp(m, "timers"))
        timers();
    else {
        puts("mode: prctl, clocks, restore or timers");
        return 2;
    }
    puts("== end");
    return 0;
}
