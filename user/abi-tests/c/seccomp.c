/* SX-1: seccomp-bpf against CachyOS. argv[1] picks the part:
 *   errno    ERRNO answers, data capped at 4095, ERRNO 0, precedence of stacked filters, LOG, /proc status
 *   trap     TRAP with a SIGSYS handler reading siginfo and the registers, the handler's answer, TRAP beating
 *            ERRNO, a blocked or ignored SIGSYS ending the process
 *   kill     KILL_PROCESS, KILL_THREAD (alone and with a sibling), an unknown action, strict mode
 *   tsync    TSYNC across threads (no_new_privs carried), a sibling with its own filter, TSYNC_ESRCH
 *   tsync-race  TSYNC while a sibling creates threads: none may miss the filter
 *   inherit  filters across fork and execve
 *   checks   argument checks, the no_new_privs rule for an unprivileged filter, hostile programs, the
 *            instruction bound of a stack
 * Prints results, errno names and yes/no only. Children are made non-dumpable so a SIGSYS writes no core on
 * CachyOS (V2 writes none). */
#define _GNU_SOURCE
#include "abi.h"
#include <grp.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <ucontext.h>

#define LD_NR BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr))
#define ARCH_CHECK                                                                   \
    BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),        \
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),               \
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS)
#define RULE(n, ret) BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (n), 0, 1), BPF_STMT(BPF_RET | BPF_K, (ret))
#define ALLOW BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW)

static long install(unsigned flags, struct sock_filter *f, unsigned short n) {
    struct sock_fprog p = { n, f };
    return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, flags, &p);
}
#define INSTALL(flags, ...)                                                          \
    ({                                                                               \
        struct sock_filter f_[] = { __VA_ARGS__ };                                   \
        install((flags), f_, sizeof f_ / sizeof f_[0]);                              \
    })
/* One rule over the arch check and ALLOW for the rest. */
#define ONE(flags, n, ret) INSTALL((flags), ARCH_CHECK, LD_NR, RULE((n), (ret)), ALLOW)

static void status_of(const char *path, const char *label) {
    FILE *f = fopen(path, "r");
    char line[256];
    char out[160] = "";
    if (!f) {
        printf("%s: no %s\n", label, path);
        return;
    }
    while (fgets(line, sizeof line, f)) {
        if (!strncmp(line, "NoNewPrivs:", 11) || !strncmp(line, "Seccomp:", 8) || !strncmp(line, "Seccomp_filters:", 16)) {
            line[strcspn(line, "\n")] = 0;
            for (char *c = line; *c; c++)
                if (*c == '\t')
                    *c = ' ';
            strncat(out, line, sizeof out - strlen(out) - 3);
            strcat(out, "; ");
        }
    }
    fclose(f);
    printf("%s: %s\n", label, out);
}
static void status(const char *label) {
    status_of("/proc/self/status", label);
}
static void thread_status(const char *label) {
    char p[64];
    snprintf(p, sizeof p, "/proc/self/task/%ld/status", syscall(SYS_gettid));
    status_of(p, label);
}

static void quiet_dumps(void) {
    struct rlimit r = { 0, 0 };
    setrlimit(RLIMIT_CORE, &r);
    prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
}

static void report(const char *what, pid_t c) {
    int st = 0;
    if (waitpid(c, &st, 0) != c)
        printf("%s: waitpid -%s\n", what, ename(errno));
    else if (WIFSIGNALED(st))
        printf("%s: signal %d%s\n", what, WTERMSIG(st), WCOREDUMP(st) ? " (core)" : "");
    else if (WIFEXITED(st))
        printf("%s: exit %d\n", what, WEXITSTATUS(st));
    else
        printf("%s: status %#x\n", what, st);
}

static void nnp(void) {
    SYS("PR_SET_NO_NEW_PRIVS", SYS_prctl, PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
}

/* ---- errno ------------------------------------------------------------------------------------------- */

static int errno_mode(void) {
    struct utsname u;
    SYS("PR_GET_SECCOMP", SYS_prctl, PR_GET_SECCOMP, 0, 0, 0, 0);
    status("before");
    nnp();
    show("filter 1", INSTALL(0, ARCH_CHECK, LD_NR, RULE(SYS_getppid, SECCOMP_RET_ERRNO | EPERM), RULE(SYS_uname, SECCOMP_RET_ERRNO | EIO),
                             RULE(SYS_getegid, SECCOMP_RET_ERRNO | 0), RULE(SYS_geteuid, SECCOMP_RET_ERRNO | 0xffff), ALLOW));
    SYS("getppid", SYS_getppid);
    SYS("uname", SYS_uname, &u);
    SYS("getegid (ERRNO 0)", SYS_getegid);
    SYS("geteuid (ERRNO 0xffff)", SYS_geteuid);
    yes("getpid runs", syscall(SYS_getpid) > 0);
    status("one filter");
    show("filter 2", INSTALL(0, ARCH_CHECK, LD_NR, RULE(SYS_getppid, SECCOMP_RET_ERRNO | EACCES), RULE(SYS_uname, SECCOMP_RET_ALLOW),
                             RULE(SYS_getuid, SECCOMP_RET_LOG), RULE(SYS_getgid, SECCOMP_RET_ALLOW | 5), ALLOW));
    SYS("getppid (newer filter's data)", SYS_getppid);
    SYS("uname (ERRNO beats ALLOW)", SYS_uname, &u);
    yes("getuid under LOG runs", syscall(SYS_getuid) == (long)getuid());
    yes("getgid under ALLOW with data runs", syscall(SYS_getgid) >= 0);
    SYS("PR_GET_SECCOMP", SYS_prctl, PR_GET_SECCOMP, 0, 0, 0, 0);
    SYS("strict after a filter", SYS_seccomp, SECCOMP_SET_MODE_STRICT, 0, 0);
    SYS("PR_SET_SECCOMP(1) after a filter", SYS_prctl, PR_SET_SECCOMP, 1, 0, 0, 0);
    /* An argument rule: getpgid(12345) answers ESRCH... from the filter (ERRNO 3), others run. */
    show("filter 3", INSTALL(0, ARCH_CHECK, LD_NR, BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getpgid, 0, 3),
                             BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
                             BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 12345, 0, 1), BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | E2BIG), ALLOW));
    SYS("getpgid(12345)", SYS_getpgid, 12345);
    yes("getpgid(0) runs", syscall(SYS_getpgid, 0) > 0);
    status("three filters");
    return 0;
}

/* ---- trap -------------------------------------------------------------------------------------------- */

static volatile int traps;

static void on_sys(int sig, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    printf("SIGSYS: signo %d code %d errno %#x syscall %d arch %s\n", si->si_signo, si->si_code, si->si_errno, si->si_syscall,
           si->si_arch == AUDIT_ARCH_X86_64 ? "x86_64" : "other");
    yes("  si_call_addr is the interrupted RIP", si->si_call_addr == (void *)uc->uc_mcontext.gregs[REG_RIP]);
    yes("  RAX holds the call's number", uc->uc_mcontext.gregs[REG_RAX] == si->si_syscall);
    yes("  RDI holds the first argument", uc->uc_mcontext.gregs[REG_RDI] == 1234);
    yes("  RSI holds the second argument", uc->uc_mcontext.gregs[REG_RSI] == 5678);
    uc->uc_mcontext.gregs[REG_RAX] = 4242 + traps;
    traps++;
    (void)sig;
}

static int trap_mode(void) {
    quiet_dumps();
    struct sigaction sa = { 0 };
    sa.sa_sigaction = on_sys;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSYS, &sa, 0);
    nnp();
    show("filter TRAP getppid", ONE(0, SYS_getppid, SECCOMP_RET_TRAP | 0x42));
    long r = syscall(SYS_getppid, 1234, 5678);
    printf("getppid -> %ld, traps %d\n", r, traps);
    show("filter ERRNO getppid", ONE(0, SYS_getppid, SECCOMP_RET_ERRNO | EPERM));
    r = syscall(SYS_getppid, 1234, 5678);
    printf("getppid (TRAP beats ERRNO) -> %ld, traps %d\n", r, traps);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        sigset_t s;
        sigemptyset(&s);
        sigaddset(&s, SIGSYS);
        sigprocmask(SIG_BLOCK, &s, 0);
        puts("child: SIGSYS blocked");
        syscall(SYS_getppid, 1234, 5678);
        puts("child: not reached");
        _exit(0);
    }
    report("TRAP with SIGSYS blocked", c);
    c = fork();
    if (c == 0) {
        signal(SIGSYS, SIG_IGN);
        puts("child: SIGSYS ignored");
        syscall(SYS_getppid, 1234, 5678);
        puts("child: not reached");
        _exit(0);
    }
    report("TRAP with SIGSYS ignored", c);
    c = fork();
    if (c == 0) {
        signal(SIGSYS, SIG_DFL);
        syscall(SYS_getppid, 1234, 5678);
        _exit(0);
    }
    report("TRAP with SIGSYS default", c);
    return 0;
}

/* ---- kill -------------------------------------------------------------------------------------------- */

static int threads(void) {
    FILE *f = fopen("/proc/self/status", "r");
    char line[256];
    int n = -1;
    while (f && fgets(line, sizeof line, f))
        if (!strncmp(line, "Threads:", 8))
            n = atoi(line + 8);
    if (f)
        fclose(f);
    return n;
}

static void *call_getppid(void *a) {
    (void)a;
    syscall(SYS_getppid);
    puts("thread: not reached");
    return 0;
}

static void kill_child(const char *what, unsigned ret, int threaded) {
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        if (ONE(0, SYS_getppid, ret) != 0) {
            puts("child: filter refused");
            _exit(99);
        }
        if (threaded) {
            /* musl's pthread_join waits for the thread's own exit path, which a killed thread never runs:
             * wait for the kernel's thread count instead (bounded). */
            pthread_t t;
            pthread_create(&t, 0, call_getppid, 0);
            int alone = 0;
            for (int i = 0; i < 500 && !alone; i++) {
                alone = threads() == 1;
                if (!alone)
                    usleep(10000);
            }
            puts(alone ? "child: main thread goes on after its sibling ended" : "child: the sibling did not end");
            _exit(0);
        }
        syscall(SYS_getppid);
        puts("child: not reached");
        _exit(0);
    }
    report(what, c);
}

static int kill_mode(void) {
    quiet_dumps();
    nnp();
    kill_child("KILL_PROCESS", SECCOMP_RET_KILL_PROCESS, 0);
    kill_child("KILL_THREAD, only thread", SECCOMP_RET_KILL_THREAD, 0);
    kill_child("KILL_THREAD in a second thread", SECCOMP_RET_KILL_THREAD, 1);
    kill_child("KILL_PROCESS in a second thread", SECCOMP_RET_KILL_PROCESS, 1);
    kill_child("unknown action 0x10000", 0x00010000, 0);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        /* In strict mode only raw read/write/exit/rt_sigreturn: stdio's writev would be killed. */
        static const char m[] = "strict: prctl answered 0, write works\n";
        if (syscall(SYS_prctl, PR_SET_SECCOMP, 1, KERNEL_ADDR, 0, 0) == 0)
            syscall(SYS_write, 1, m, sizeof m - 1);
        syscall(SYS_getpid);
        syscall(SYS_write, 1, "not reached\n", 12);
        syscall(SYS_exit, 0);
    }
    report("strict mode, getpid", c);
    c = fork();
    if (c == 0) {
        static const char m[] = "strict: seccomp(STRICT) answered 0\n";
        if (syscall(SYS_seccomp, SECCOMP_SET_MODE_STRICT, 0, 0) == 0)
            syscall(SYS_write, 1, m, sizeof m - 1);
        syscall(SYS_exit, 3);
    }
    report("strict mode, exit(3)", c);
    return 0;
}

/* ---- tsync ------------------------------------------------------------------------------------------- */

struct worker {
    int cmd[2], ack[2];
    pthread_t t;
    long tid;
};

static void *work(void *a) {
    struct worker *w = a;
    w->tid = syscall(SYS_gettid);
    for (;;) {
        char c = 0;
        if (read(w->cmd[0], &c, 1) != 1 || c == 'q')
            break;
        if (c == 'n')
            SYS("  worker: PR_GET_NO_NEW_PRIVS", SYS_prctl, PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
        if (c == 'p')
            SYS("  worker: getppid", SYS_getppid);
        if (c == 's')
            thread_status("  worker status");
        if (c == 'f')
            show("  worker: own filter", ONE(0, SYS_getuid, SECCOMP_RET_ERRNO | EACCES));
        write(w->ack[1], &c, 1);
    }
    return 0;
}

static void ask(struct worker *w, char c) {
    write(w->cmd[1], &c, 1);
    read(w->ack[0], &c, 1);
}

static void start(struct worker *w) {
    pipe(w->cmd);
    pipe(w->ack);
    pthread_create(&w->t, 0, work, w);
    ask(w, 'n');
}

static void stop(struct worker *w) {
    char c = 'q';
    write(w->cmd[1], &c, 1);
    pthread_join(w->t, 0);
}

static int tsync_mode(void) {
    struct worker a, b;
    start(&a);
    nnp();
    SYS("main: PR_GET_NO_NEW_PRIVS", SYS_prctl, PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
    ask(&a, 'n');
    show("TSYNC filter", ONE(SECCOMP_FILTER_FLAG_TSYNC, SYS_getppid, SECCOMP_RET_ERRNO | EPERM));
    SYS("main: getppid", SYS_getppid);
    ask(&a, 'n');
    ask(&a, 'p');
    ask(&a, 's');
    start(&b);
    ask(&b, 'f');
    long r = ONE(SECCOMP_FILTER_FLAG_TSYNC, SYS_getpgid, SECCOMP_RET_ERRNO | EIO);
    yes("TSYNC with a sibling's own filter answers that sibling's tid", r == b.tid);
    SYS("TSYNC | TSYNC_ESRCH", SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC | SECCOMP_FILTER_FLAG_TSYNC_ESRCH, 0);
    show("TSYNC | TSYNC_ESRCH filter", ONE(SECCOMP_FILTER_FLAG_TSYNC | SECCOMP_FILTER_FLAG_TSYNC_ESRCH, SYS_getpgid, SECCOMP_RET_ERRNO | EIO));
    status("main after the refused TSYNC");
    show("filter without TSYNC", ONE(0, SYS_getpgid, SECCOMP_RET_ERRNO | EIO));
    status("main");
    ask(&a, 's');
    stop(&b);
    show("TSYNC after that sibling ended", ONE(SECCOMP_FILTER_FLAG_TSYNC, SYS_getsid, SECCOMP_RET_ERRNO | ENOENT));
    ask(&a, 's');
    stop(&a);
    return 0;
}

/* ---- tsync-race: TSYNC while a sibling is cloning --------------------------------------------------- */

/* A thread created while TSYNC runs must end up with the filter: Linux copies the creator's stack under the
 * same lock the TSYNC walk holds. Each round forks a fresh process whose spawner thread creates threads
 * without pause while the main thread syncs a filter; every thread created looks at getppid once the sync
 * returned. The count of threads without the filter must be 0. */
enum { RACE_ROUNDS = 100, RACE_MAX = 64 };
static int race_done, race_bad, race_made;

static void *race_check(void *a) {
    (void)a;
    while (!__atomic_load_n(&race_done, __ATOMIC_ACQUIRE))
        sched_yield();
    if (syscall(SYS_getppid) != -1 || errno != EPERM)
        __atomic_add_fetch(&race_bad, 1, __ATOMIC_RELAXED);
    return 0;
}

static void *race_spawn(void *a) {
    pthread_t *t = a;
    int n = 0;
    while (n < RACE_MAX && !__atomic_load_n(&race_done, __ATOMIC_ACQUIRE))
        if (!pthread_create(&t[n], 0, race_check, 0))
            __atomic_store_n(&race_made, ++n, __ATOMIC_RELEASE);
    for (int i = 0; i < n; i++)
        pthread_join(t[i], 0);
    return 0;
}

static int race_round(void) {
    static pthread_t t[RACE_MAX];
    pthread_t s;
    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) || pthread_create(&s, 0, race_spawn, t))
        return 100;
    while (__atomic_load_n(&race_made, __ATOMIC_ACQUIRE) < 2)
        sched_yield();
    long r = ONE(SECCOMP_FILTER_FLAG_TSYNC, SYS_getppid, SECCOMP_RET_ERRNO | EPERM);
    __atomic_store_n(&race_done, 1, __ATOMIC_RELEASE);
    pthread_join(s, 0);
    return r ? 101 : race_bad;
}

static int tsync_race_mode(void) {
    int bad = 0, failed = 0;
    for (int i = 0; i < RACE_ROUNDS; i++) {
        pid_t c = fork();
        if (c == 0)
            _exit(race_round());
        int st = 0;
        if (c < 0 || waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st) >= 100)
            failed++;
        else
            bad += WEXITSTATUS(st);
    }
    printf("tsync-race: rounds %d, rounds that failed %d, threads without the filter %d\n", RACE_ROUNDS, failed, bad);
    return 0;
}

/* ---- inherit ----------------------------------------------------------------------------------------- */

static int inherit_exec(void) {
    puts("after execve:");
    SYS("  getppid", SYS_getppid);
    SYS("  PR_GET_SECCOMP", SYS_prctl, PR_GET_SECCOMP, 0, 0, 0, 0);
    status("  status");
    return 0;
}

static int inherit_mode(const char *self) {
    nnp();
    show("filter", ONE(0, SYS_getppid, SECCOMP_RET_ERRNO | EPERM));
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        SYS("child: getppid", SYS_getppid);
        status("child");
        show("child: own filter", ONE(0, SYS_getpgid, SECCOMP_RET_ERRNO | EIO));
        status("child");
        _exit(0);
    }
    report("fork child", c);
    status("parent");
    fflush(stdout);
    c = fork();
    if (c == 0) {
        char *argv[] = { (char *)self, "inherit-exec", 0 };
        char *envp[] = { "PATH=/bin", 0 };
        execve(self, argv, envp);
        printf("execve -%s\n", ename(errno));
        _exit(1);
    }
    report("exec child", c);
    return 0;
}

/* ---- checks ------------------------------------------------------------------------------------------ */

static long fprog(unsigned flags, unsigned short len, void *insns) {
    struct sock_fprog p = { len, insns };
    return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, flags, &p);
}

static void hostile(const char *what, struct sock_filter *f, unsigned short n) {
    printf("hostile %s = ", what);
    long r = install(0, f, n);
    if (r == -1)
        printf("-%s\n", ename(errno));
    else
        printf("%ld\n", r);
}
#define HOSTILE(what, ...)                                                           \
    do {                                                                             \
        struct sock_filter h_[] = { __VA_ARGS__ };                                   \
        hostile(what, h_, sizeof h_ / sizeof h_[0]);                                 \
    } while (0)

static int checks_mode(void) {
    if (geteuid() == 0) {
        /* V2 may start the program as root: the no_new_privs rule needs a user without CAP_SYS_ADMIN. */
        int ok = !setgroups(0, 0) && !setgid(1000) && !setuid(1000);
        if (!ok)
            puts("could not drop root");
    }
    struct sock_filter allow[] = { ALLOW };
    static struct sock_filter big[BPF_MAXINSNS + 1];
    for (int i = 0; i < BPF_MAXINSNS; i++)
        big[i] = (struct sock_filter)BPF_STMT(BPF_LD | BPF_IMM, 0);
    big[BPF_MAXINSNS] = (struct sock_filter)ALLOW;
    SYS("PR_GET_SECCOMP", SYS_prctl, PR_GET_SECCOMP, 0, 0, 0, 0);
    SYS("PR_GET_NO_NEW_PRIVS", SYS_prctl, PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
    SYS("FILTER(NULL)", SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, 0);
    SYS("FILTER(TSYNC, NULL)", SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC, 0);
    SYS("FILTER(SPEC_ALLOW, NULL)", SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_SPEC_ALLOW, 0);
    SYS("FILTER(LOG, NULL)", SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_LOG, 0);
    SYS("FILTER(TSYNC_ESRCH, NULL)", SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC_ESRCH, 0);
    SYS("FILTER(flag 0x40, NULL)", SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0x40, 0);
    SYS("FILTER(TSYNC | NEW_LISTENER, NULL)", SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC | SECCOMP_FILTER_FLAG_NEW_LISTENER, 0);
    SYS("FILTER(WAIT_KILLABLE_RECV, NULL)", SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV, 0);
    SYS("FILTER(fprog at a kernel address)", SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, KERNEL_ADDR);
    show("len 0 without no_new_privs", fprog(0, 0, allow));
    show("len 4097 without no_new_privs", fprog(0, BPF_MAXINSNS + 1, big));
    show("len 1, NULL program, without no_new_privs", fprog(0, 1, 0));
    show("valid without no_new_privs", fprog(0, 1, allow));
    SYS("PR_SET_SECCOMP(2) without no_new_privs", SYS_prctl, PR_SET_SECCOMP, 2, &(struct sock_fprog){ 1, allow }, 0, 0);
    SYS("PR_SET_SECCOMP(3)", SYS_prctl, PR_SET_SECCOMP, 3, 0, 0, 0);
    SYS("PR_SET_SECCOMP(2, NULL)", SYS_prctl, PR_SET_SECCOMP, 2, 0, 0, 0);
    SYS("seccomp(op 4)", SYS_seccomp, 4, 0, 0);
    SYS("seccomp(STRICT, flags 1)", SYS_seccomp, SECCOMP_SET_MODE_STRICT, 1, 0);
    SYS("seccomp(STRICT, uargs)", SYS_seccomp, SECCOMP_SET_MODE_STRICT, 0, allow);
    unsigned acts[] = { SECCOMP_RET_KILL_PROCESS, SECCOMP_RET_KILL_THREAD, SECCOMP_RET_TRAP, SECCOMP_RET_ERRNO, SECCOMP_RET_USER_NOTIF,
                        SECCOMP_RET_TRACE, SECCOMP_RET_LOG, SECCOMP_RET_ALLOW, 0x00010000, SECCOMP_RET_ERRNO | 1 };
    for (unsigned i = 0; i < sizeof acts / sizeof acts[0]; i++) {
        char l[64];
        snprintf(l, sizeof l, "ACTION_AVAIL %#x", acts[i]);
        SYS(l, SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, &acts[i]);
    }
    SYS("ACTION_AVAIL flags 1", SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 1, &acts[0]);
    SYS("ACTION_AVAIL NULL", SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, 0);
    struct seccomp_notif_sizes sz = { 0 };
    SYS("NOTIF_SIZES", SYS_seccomp, SECCOMP_GET_NOTIF_SIZES, 0, &sz);
    printf("sizes: notif %u resp %u data %u\n", sz.seccomp_notif, sz.seccomp_notif_resp, sz.seccomp_data);
    SYS("NOTIF_SIZES flags 1", SYS_seccomp, SECCOMP_GET_NOTIF_SIZES, 1, &sz);
    SYS("NOTIF_SIZES NULL", SYS_seccomp, SECCOMP_GET_NOTIF_SIZES, 0, 0);

    nnp();
    show("len 1, NULL program", fprog(0, 1, 0));
    show("program at a kernel address", fprog(0, 1, KERNEL_ADDR));
    HOSTILE("JA past the end", BPF_STMT(BPF_JMP | BPF_JA, 1), ALLOW);
    HOSTILE("JEQ true past the end", BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 0, 1, 0), ALLOW);
    HOSTILE("JEQ false past the end", BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, 200), ALLOW);
    HOSTILE("load at 64", BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 64), ALLOW);
    HOSTILE("load at 2 (unaligned)", BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 2), ALLOW);
    HOSTILE("half-word load", BPF_STMT(BPF_LD | BPF_H | BPF_ABS, 0), ALLOW);
    HOSTILE("byte load", BPF_STMT(BPF_LD | BPF_B | BPF_ABS, 0), ALLOW);
    HOSTILE("indirect load", BPF_STMT(BPF_LD | BPF_W | BPF_IND, 0), ALLOW);
    HOSTILE("ancillary load", BPF_STMT(BPF_LD | BPF_W | BPF_ABS, SKF_AD_OFF + SKF_AD_PROTOCOL), ALLOW);
    HOSTILE("division by constant 0", BPF_STMT(BPF_ALU | BPF_DIV | BPF_K, 0), ALLOW);
    HOSTILE("MOD", BPF_STMT(BPF_ALU | BPF_MOD | BPF_K, 3), ALLOW);
    HOSTILE("shift by 32", BPF_STMT(BPF_ALU | BPF_LSH | BPF_K, 32), ALLOW);
    HOSTILE("scratch cell 16", BPF_STMT(BPF_ST, 16), ALLOW);
    HOSTILE("scratch read before write", BPF_STMT(BPF_LD | BPF_MEM, 3), ALLOW);
    HOSTILE("no return at the end", BPF_STMT(BPF_LD | BPF_IMM, 0));
    HOSTILE("unknown opcode", BPF_STMT(0xffff, 0), ALLOW);
    HOSTILE("RET X", BPF_STMT(BPF_RET | BPF_X, 0));
    status("after the refusals");
    show("division by X = 0 is fine (the program answers 0 then)",
         INSTALL(0, ARCH_CHECK, LD_NR, BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 3), BPF_STMT(BPF_LDX | BPF_IMM, 0),
                 BPF_STMT(BPF_ALU | BPF_DIV | BPF_X, 0), BPF_STMT(BPF_RET | BPF_A, 0), ALLOW));
    show("length loads, scratch cells", INSTALL(0, BPF_STMT(BPF_LD | BPF_W | BPF_LEN, 0), BPF_STMT(BPF_ST, 15), BPF_STMT(BPF_LDX | BPF_MEM, 15),
                                                BPF_STMT(BPF_MISC | BPF_TXA, 0), BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 64, 1, 0),
                                                BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS), ALLOW));
    status("two filters");
    quiet_dumps();
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        int n = 0;
        long r;
        while ((r = fprog(0, BPF_MAXINSNS, big + 1)) == 0)
            n++;
        printf("4096-instruction filters added: %d, then -%s\n", n, ename(errno));
        /* Linux counts the stack in its runtime instructions (a program of n of these is n + 4, each filter
         * below + 4): 32768 - 7 x 4104 - the two filters above (19 + 4, 12 + 4) leaves 4001. */
        show("3998 more", fprog(0, 3998, big + BPF_MAXINSNS - 3997));
        show("3997 more", fprog(0, 3997, big + BPF_MAXINSNS - 3996));
        show("1 more", fprog(0, 1, allow));
        status("full stack");
        _exit(0);
    }
    report("bound child", c);
    fflush(stdout);
    c = fork();
    if (c == 0) {
        INSTALL(0, ARCH_CHECK, LD_NR, BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 3), BPF_STMT(BPF_LDX | BPF_IMM, 0),
                BPF_STMT(BPF_ALU | BPF_DIV | BPF_X, 0), BPF_STMT(BPF_RET | BPF_A, 0), ALLOW);
        syscall(SYS_getppid);
        _exit(0);
    }
    report("division by X = 0 answers KILL_THREAD", c);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    /* The file to re-exec: argv[0] when it is a path, V2's /bin copy, else the kernel's link (CachyOS). */
    const char *self = strchr(argv[0], '/') ? argv[0] : !access("/bin/abi-seccomp", X_OK) ? "/bin/abi-seccomp" : "/proc/self/exe";
    const char *m = argc > 1 ? argv[1] : "";
    if (!strcmp(m, "errno"))
        return errno_mode();
    if (!strcmp(m, "trap"))
        return trap_mode();
    if (!strcmp(m, "kill"))
        return kill_mode();
    if (!strcmp(m, "tsync"))
        return tsync_mode();
    if (!strcmp(m, "tsync-race"))
        return tsync_race_mode();
    if (!strcmp(m, "inherit"))
        return inherit_mode(self);
    if (!strcmp(m, "inherit-exec"))
        return inherit_exec();
    if (!strcmp(m, "checks"))
        return checks_mode();
    puts("modes: errno trap kill tsync tsync-race inherit checks");
    return 0;
}
