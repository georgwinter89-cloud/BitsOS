/* SX-IPC: semaphores against linux-7.3.1/ipc/sem.c: every semctl command and error, atomic lists, wake order, EIDRM,
 * EINTR, timeouts. Modes: basic, order, signals, undo, undoexec, seccomp, plain. */
#include "sysv.h"
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <sys/prctl.h>

static int semget_(int key, int n, int fl) { return syscall(SYS_semget, key, n, fl); }

static void basic(void) {
    fresh_ns();
    no_caps();
    S("semget no key", SYS_semget, K, 1, 0600);
    S("semget negative", SYS_semget, K, -1, IPC_CREAT | 0600);
    S("semget over semmsl", SYS_semget, K, 32001, IPC_CREAT | 0600);
    S("semget zero create", SYS_semget, K, 0, IPC_CREAT | 0600);
    S("semget private zero", SYS_semget, IPC_PRIVATE, 0, 0600);
    int id = S("semget create", SYS_semget, K, 3, IPC_CREAT | 0640);
    S("semget again", SYS_semget, K, 3, 0);
    S("semget zero nsems finds", SYS_semget, K, 0, 0);
    S("semget excl", SYS_semget, K, 3, IPC_CREAT | IPC_EXCL | 0640);
    S("semget more", SYS_semget, K, 4, 0);
    S("semget writable mode ok", SYS_semget, K, 3, 0640);
    S("second id", SYS_semget, IPC_PRIVATE, 1, 0600);

    struct sem64 st;
    memset(&st, 0x55, sizeof st);
    S("IPC_STAT", SYS_semctl, id, 0, IPC_STAT, &st);
    printf("stat key %x mode %o nsems %lu uid %u gid %u cuid %u cgid %u seq %u otime0 %d ctime %d\n", st.p.key, st.p.mode, st.nsems, st.p.uid, st.p.gid, st.p.cuid, st.p.cgid, st.p.seq, st.otime == 0, st.ctime > 0);
    S("IPC_STAT fault", SYS_semctl, id, 0, IPC_STAT, KERNEL_ADDR);
    S("IPC_STAT stale", SYS_semctl, id + (1 << 15), 0, IPC_STAT, &st);
    S("IPC_STAT negative", SYS_semctl, -1, 0, IPC_STAT, &st);
    S("bad cmd", SYS_semctl, id, 0, 77, 0);
    S("old IPC_64 cmd", SYS_semctl, id, 0, IPC_STAT | 0x100, &st);
    int si[10];
    long max = S("IPC_INFO", SYS_semctl, 0, 0, IPC_INFO, si);
    printf("info %d %d %d %d %d %d %d %d %d %d (max %ld)\n", si[0], si[1], si[2], si[3], si[4], si[5], si[6], si[7], si[8], si[9], max);
    S("SEM_INFO", SYS_semctl, 0, 0, SEM_INFO_, si);
    printf("sem_info usz %d aem %d\n", si[7], si[9]);
    S("IPC_INFO fault", SYS_semctl, 0, 0, IPC_INFO, KERNEL_ADDR);
    S("SEM_STAT index 0", SYS_semctl, 0, 0, SEM_STAT_, &st);
    S("SEM_STAT index 2", SYS_semctl, 2, 0, SEM_STAT_, &st);
    S("SEM_STAT_ANY index 1", SYS_semctl, 1, 0, SEM_STAT_ANY_, &st);

    S("SETVAL", SYS_semctl, id, 1, SETVAL_, 7);
    S("GETVAL", SYS_semctl, id, 1, GETVAL_);
    S("GETVAL other", SYS_semctl, id, 0, GETVAL_);
    S("GETVAL bad num", SYS_semctl, id, 3, GETVAL_);
    S("GETVAL negative num", SYS_semctl, id, -1, GETVAL_);
    S("SETVAL range", SYS_semctl, id, 1, SETVAL_, 32768);
    S("SETVAL negative", SYS_semctl, id, 1, SETVAL_, -1);
    S("SETVAL max", SYS_semctl, id, 1, SETVAL_, 32767);
    S("SETVAL bad num", SYS_semctl, id, 9, SETVAL_, 1);
    yes("GETPID is me", syscall(SYS_semctl, id, 1, GETPID_) == getpid());
    S("GETPID untouched", SYS_semctl, id, 0, GETPID_);
    S("GETNCNT", SYS_semctl, id, 0, GETNCNT_);
    S("GETZCNT", SYS_semctl, id, 0, GETZCNT_);
    unsigned short all[3] = { 1, 2, 3 }, got[3];
    S("SETALL", SYS_semctl, id, 0, SETALL_, all);
    S("GETALL", SYS_semctl, id, 0, GETALL_, got);
    printf("all %u %u %u\n", got[0], got[1], got[2]);
    S("GETALL fault", SYS_semctl, id, 0, GETALL_, KERNEL_ADDR);
    S("SETALL fault", SYS_semctl, id, 0, SETALL_, KERNEL_ADDR);
    all[1] = 40000;
    S("SETALL range", SYS_semctl, id, 0, SETALL_, all);
    S("GETVAL after refused SETALL", SYS_semctl, id, 0, GETVAL_);

    struct sb a[3] = { { 0, -1, IPC_NOWAIT }, { 1, -5, IPC_NOWAIT }, { 2, 1, IPC_NOWAIT } };
    S("all or nothing EAGAIN", SYS_semop, id, a, 3);
    S("GETALL unchanged", SYS_semctl, id, 0, GETALL_, got);
    printf("all %u %u %u\n", got[0], got[1], got[2]);
    a[1].op = -2;
    S("list goes", SYS_semop, id, a, 3);
    S("GETALL", SYS_semctl, id, 0, GETALL_, got);
    printf("all %u %u %u\n", got[0], got[1], got[2]);
    S("otime stat", SYS_semctl, id, 0, IPC_STAT, &st);
    printf("otime set %d\n", st.otime > 0);
    S("nsops 0", SYS_semop, id, a, 0);
    S("nsops over", SYS_semop, id, a, 501);
    S("bad array", SYS_semop, id, KERNEL_ADDR, 1);
    S("negative id", SYS_semop, -1, a, 1);
    S("bad id", SYS_semop, 12345, a, 1);
    struct sb f = { 3, 1, 0 };
    S("EFBIG", SYS_semop, id, &f, 1);
    struct sb over = { 0, 32767, 0 };
    S("ERANGE", SYS_semop, id, &over, 1);
    struct sb dup[2] = { { 0, 2, 0 }, { 0, -2, 0 } };
    S("same semaphore twice", SYS_semop, id, dup, 2);
    struct sb dup2[2] = { { 0, 1, IPC_NOWAIT }, { 0, -9, IPC_NOWAIT } };
    S("twice, second blocks", SYS_semop, id, dup2, 2);
    S("value rolled back", SYS_semctl, id, 0, GETVAL_);
    struct sb zero = { 1, 0, IPC_NOWAIT };
    S("zero wait NOWAIT nonzero", SYS_semctl, id, 1, SETVAL_, 1);
    S("zero NOWAIT", SYS_semop, id, &zero, 1);

    struct timespec ts = { 0, 20000000 }, bad = { 0, 2000000000 }, neg = { -1, 0 };
    struct sb w = { 2, -100, 0 };
    S("timed out", SYS_semtimedop, id, &w, 1, &ts);
    S("bad timespec", SYS_semtimedop, id, &w, 1, &bad);
    S("negative timespec", SYS_semtimedop, id, &w, 1, &neg);
    S("timespec fault", SYS_semtimedop, id, &w, 1, KERNEL_ADDR);
    S("bad timespec, nsops first", SYS_semtimedop, id, &w, 0, &bad);
    w.op = 1;
    S("timed, can go", SYS_semtimedop, id, &w, 1, &ts);

    st.p.mode = 0;
    S("IPC_SET mode 0", SYS_semctl, id, 0, IPC_SET, &st);
    S("IPC_SET fault", SYS_semctl, id, 0, IPC_SET, KERNEL_ADDR);
    S("semget no rights", SYS_semget, K, 3, 0600);
    S("semget flag 0 passes", SYS_semget, K, 3, 0);
    S("GETVAL no rights", SYS_semctl, id, 0, GETVAL_);
    S("SETVAL no rights", SYS_semctl, id, 0, SETVAL_, 1);
    S("GETALL no rights", SYS_semctl, id, 0, GETALL_, got);
    S("SETALL no rights", SYS_semctl, id, 0, SETALL_, got);
    S("IPC_STAT no rights", SYS_semctl, id, 0, IPC_STAT, &st);
    S("SEM_STAT no rights", SYS_semctl, 0, 0, SEM_STAT_, &st);
    S("SEM_STAT_ANY no rights", SYS_semctl, 0, 0, SEM_STAT_ANY_, &st);
    S("semop no rights", SYS_semop, id, a, 1);
    st.p.uid = 12345;
    S("IPC_SET unmapped uid", SYS_semctl, id, 0, IPC_SET, &st);
    S("IPC_RMID", SYS_semctl, id, 0, IPC_RMID);
    S("IPC_RMID again", SYS_semctl, id, 0, IPC_RMID);
    S("key free again", SYS_semget, K, 3, 0);
    int nid = S("key made again", SYS_semget, K, 1, IPC_CREAT | 0600);
    printf("new id differs: %d\n", nid != id);
    S("old id after reuse", SYS_semctl, id, 0, GETVAL_);
    /* The index cycles: 64 fresh arrays, then the sequence number shows. */
    int last = 0;
    for (int i = 0; i < 70; i++) {
        last = semget_(IPC_PRIVATE, 1, 0600);
        if (i < 62)
            syscall(SYS_semctl, last, 0, IPC_RMID);
    }
    printf("id after cycling %d\n", last);
}

/* The threads of "order" log what they finished. */
static int semid;

struct job { const char *name; struct sb ops[2]; int n; };

static void *worker(void *a) {
    struct job *j = a;
    long r = syscall(SYS_semop, semid, j->ops, j->n);
    int e = errno;
    pthread_mutex_lock(&log_lock);
    strcat(logline, j->name);
    strcat(logline, r ? (e == 43 ? "!EIDRM " : "!? ") : " ");
    pthread_mutex_unlock(&log_lock);
    return 0;
}

static void order(void) {
    fresh_ns();
    pthread_t t[8];
    semid = semget_(IPC_PRIVATE, 4, 0600);
    /* FIFO on one semaphore. */
    struct job a = { "a", { { 0, -1, 0 } }, 1 }, b = { "b", { { 0, -1, 0 } }, 1 }, c = { "c", { { 0, -1, 0 } }, 1 };
    go(&t[0], worker, &a); go(&t[1], worker, &b); go(&t[2], worker, &c);
    S("GETNCNT 3 waiters", SYS_semctl, semid, 0, GETNCNT_);
    struct sb up = { 0, 1, 0 };
    syscall(SYS_semop, semid, &up, 1);
    flush_log("first +1", 1);
    syscall(SYS_semop, semid, &up, 1);
    flush_log("second +1", 1);
    S("GETNCNT left", SYS_semctl, semid, 0, GETNCNT_);
    syscall(SYS_semop, semid, &up, 1);
    flush_log("third +1", 1);
    for (int i = 0; i < 3; i++) pthread_join(t[i], 0);

    /* Wait for zero goes before altering, and a -2 waiter does not hold back a later -1 of another semaphore. */
    syscall(SYS_semctl, semid, 1, SETVAL_, 1);
    struct job z = { "z", { { 1, 0, 0 } }, 1 }, d = { "d", { { 1, -2, 0 } }, 1 }, e = { "e", { { 2, -1, 0 } }, 1 };
    go(&t[0], worker, &z); go(&t[1], worker, &d); go(&t[2], worker, &e);
    S("GETZCNT", SYS_semctl, semid, 1, GETZCNT_);
    S("GETNCNT", SYS_semctl, semid, 1, GETNCNT_);
    syscall(SYS_semctl, semid, 2, SETVAL_, 1);
    flush_log("sem 2 given", 1);
    syscall(SYS_semctl, semid, 1, SETVAL_, 0);
    flush_log("sem 1 to zero", 1);
    syscall(SYS_semctl, semid, 1, SETVAL_, 2);
    flush_log("sem 1 to two", 1);
    for (int i = 0; i < 3; i++) pthread_join(t[i], 0);

    /* A complex waiter merges the queues: strict arrival order until it has gone. */
    struct job s1 = { "s1", { { 0, -1, 0 } }, 1 }, cx = { "cx", { { 0, -1, 0 }, { 1, -1, 0 } }, 2 }, s2 = { "s2", { { 1, -1, 0 } }, 1 }, s3 = { "s3", { { 0, -1, 0 } }, 1 };
    go(&t[0], worker, &s1); go(&t[1], worker, &cx); go(&t[2], worker, &s2); go(&t[3], worker, &s3);
    S("GETNCNT sem 0 (blocking op of cx)", SYS_semctl, semid, 0, GETNCNT_);
    S("GETNCNT sem 1", SYS_semctl, semid, 1, GETNCNT_);
    syscall(SYS_semctl, semid, 1, SETVAL_, 1);
    flush_log("sem 1 = 1 goes to s2 past blocked cx", 1);
    syscall(SYS_semctl, semid, 0, SETVAL_, 1);
    flush_log("sem 0 = 1", 1);
    syscall(SYS_semctl, semid, 1, SETVAL_, 1);
    flush_log("sem 1 = 1 again: cx wants sem 0 too", 0);
    syscall(SYS_semctl, semid, 0, SETVAL_, 1);
    flush_log("sem 0 = 1: cx before s3", 1);
    syscall(SYS_semctl, semid, 0, SETVAL_, 1);
    flush_log("sem 0 = 1: s3", 1);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);

    /* A completed complex list restarts the queue: waiters before it may run now. */
    struct job g = { "g", { { 3, -1, 0 } }, 1 }, h = { "h", { { 3, 0, 0 }, { 2, 0, 0 } }, 2 };
    syscall(SYS_semctl, semid, 3, SETVAL_, 0);
    syscall(SYS_semctl, semid, 2, SETVAL_, 1);
    go(&t[0], worker, &g); go(&t[1], worker, &h);
    S("GETZCNT sem 2", SYS_semctl, semid, 2, GETZCNT_);
    S("GETZCNT sem 3", SYS_semctl, semid, 3, GETZCNT_);
    syscall(SYS_semctl, semid, 2, SETVAL_, 0);
    flush_log("zero list", 1);
    syscall(SYS_semctl, semid, 3, SETVAL_, 1);
    flush_log("g", 1);
    for (int i = 0; i < 2; i++) pthread_join(t[i], 0);

    /* Removal wakes everybody with EIDRM. */
    struct job x = { "x", { { 0, -5, 0 } }, 1 }, y = { "y", { { 0, 0, 0 } }, 1 }, w = { "w", { { 0, -5, 0 }, { 1, -5, 0 } }, 2 };
    syscall(SYS_semctl, semid, 0, SETVAL_, 1);
    go(&t[0], worker, &x); go(&t[1], worker, &y); go(&t[2], worker, &w);
    S("IPC_RMID with waiters", SYS_semctl, semid, 0, IPC_RMID);
    flush_log("removed", 3);
    for (int i = 0; i < 3; i++) pthread_join(t[i], 0);
}

static void on_usr1(int s) { (void)s; }

static int waiting_sem;

/* Interrupts the main thread once its semop is queued. */
static void *kicker(void *a) {
    while (syscall(SYS_semctl, waiting_sem, 0, GETNCNT_) < 1)
        msleep(1);
    pthread_kill(*(pthread_t *)a, SIGUSR1);
    return 0;
}

static void signals(void) {
    fresh_ns();
    int id = semget_(IPC_PRIVATE, 1, 0600);
    waiting_sem = id;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, 0);
    pthread_t me = pthread_self(), k;
    struct sb w = { 0, -1, 0 };
    pthread_create(&k, 0, kicker, &me);
    S("semop interrupted", SYS_semop, id, &w, 1);
    pthread_join(k, 0);
    sa.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &sa, 0);
    pthread_create(&k, 0, kicker, &me);
    S("semop with SA_RESTART stays EINTR", SYS_semop, id, &w, 1);
    pthread_join(k, 0);
    S("no waiter left", SYS_semctl, id, 0, GETNCNT_);
    /* signal(7): semop is interrupted even by a stop and continue. */
    fflush(stdout);
    pid_t parent = getpid(), kid = fork();
    if (kid == 0) {
        while (syscall(SYS_semctl, id, 0, GETNCNT_) < 1)
            msleep(1);
        kill(parent, SIGSTOP);
        msleep(30);
        kill(parent, SIGCONT);
        _exit(0);
    }
    S("semop through stop and continue", SYS_semop, id, &w, 1);
    waitpid(kid, 0, 0);
    /* A killed waiter leaves the queue. */
    fflush(stdout);
    kid = fork();
    if (kid == 0) {
        syscall(SYS_semop, id, &w, 1);
        _exit(1);
    }
    while (syscall(SYS_semctl, id, 0, GETNCNT_) < 1)
        msleep(1);
    S("killed waiter queued", SYS_semctl, id, 0, GETNCNT_);
    kill(kid, SIGKILL);
    waitpid(kid, 0, 0);
    S("killed waiter gone", SYS_semctl, id, 0, GETNCNT_);
}

static int uid_ops;

static void child_up(void) {
    struct sb o = { 0, 3, SEM_UNDO_ };
    S("child +3 undo", SYS_semop, uid_ops, &o, 1);
    S("seen in child", SYS_semctl, uid_ops, 0, GETVAL_);
}

static void child_killed(void) {
    struct sb o = { 0, 2, SEM_UNDO_ };
    S("child +2 undo", SYS_semop, uid_ops, &o, 1);
    fflush(stdout);
    raise(SIGKILL);
}

static void child_clamp(void) {
    struct sb o = { 0, 5, SEM_UNDO_ };
    S("child +5 undo", SYS_semop, uid_ops, &o, 1);
    struct sb d = { 1, 1, SEM_UNDO_ };
    S("child sem1 +1 undo", SYS_semop, uid_ops, &d, 1);
    S("parent-like take", SYS_semctl, uid_ops, 0, SETVAL_, 3);
    S("sem 1 stays", SYS_semctl, uid_ops, 1, GETVAL_);
}

static void child_inherit(void) {
    struct sb o = { 0, -1, SEM_UNDO_ | IPC_NOWAIT };
    S("fork child takes one with undo", SYS_semop, uid_ops, &o, 1);
}

static void child_unshare(void) {
    struct sb o = { 0, 4, SEM_UNDO_ };
    S("child +4 undo", SYS_semop, uid_ops, &o, 1);
    S("unshare(CLONE_SYSVSEM)", SYS_unshare, CLONE_SYSVSEM);
    S("undone at once", SYS_semctl, uid_ops, 0, GETVAL_);
    o.op = 1;
    S("child +1 undo", SYS_semop, uid_ops, &o, 1);
}

static void *thread_undo(void *a) {
    (void)a;
    struct sb o = { 0, 6, SEM_UNDO_ };
    S("thread +6 undo", SYS_semop, uid_ops, &o, 1);
    return 0;
}

static void child_thread(void) {
    pthread_t t;
    pthread_create(&t, 0, thread_undo, 0);
    pthread_join(t, 0);
    S("after the thread ended (shared list)", SYS_semctl, uid_ops, 0, GETVAL_);
    /* Adjustment by a sibling: same list, so one value goes on. */
    struct sb o = { 0, 1, SEM_UNDO_ };
    S("main +1 undo", SYS_semop, uid_ops, &o, 1);
}

static void undo(void) {
    fresh_ns();
    uid_ops = semget_(IPC_PRIVATE, 2, 0600);
    in_child("child ended", child_up);
    S("undone", SYS_semctl, uid_ops, 0, GETVAL_);
    yes("GETPID is not me", syscall(SYS_semctl, uid_ops, 0, GETPID_) != getpid());
    in_child("child killed", child_killed);
    S("undone after SIGKILL", SYS_semctl, uid_ops, 0, GETVAL_);
    in_child("child with reset", child_clamp);
    S("SETVAL cleared its adjustment", SYS_semctl, uid_ops, 0, GETVAL_);
    S("sem 1 undone", SYS_semctl, uid_ops, 1, GETVAL_);
    syscall(SYS_semctl, uid_ops, 0, SETVAL_, 0);
    struct sb my = { 0, 3, SEM_UNDO_ };
    S("my +3 undo", SYS_semop, uid_ops, &my, 1);
    in_child("fork child", child_inherit);
    S("child's -1 given back, mine stays", SYS_semctl, uid_ops, 0, GETVAL_);
    in_child("unsharing child", child_unshare);
    S("after unshare child", SYS_semctl, uid_ops, 0, GETVAL_);
    syscall(SYS_semctl, uid_ops, 0, SETVAL_, 0);
    in_child("threads", child_thread);
    S("thread's and main's undone", SYS_semctl, uid_ops, 0, GETVAL_);

    /* The range of an adjustment. */
    syscall(SYS_semctl, uid_ops, 1, SETVAL_, 32767);
    struct sb far = { 1, -32767, SEM_UNDO_ };
    S("adj +32767", SYS_semop, uid_ops, &far, 1);
    syscall(SYS_semctl, uid_ops, 1, SETVAL_, 32767);
    struct sb more = { 1, -1, SEM_UNDO_ };
    S("cleared by SETVAL so fine", SYS_semop, uid_ops, &more, 1);
    struct sb plainup = { 1, 1, 0 };
    S("plain +1", SYS_semop, uid_ops, &plainup, 1);
    S("over the range", SYS_semop, uid_ops, &far, 1);
    S("value unchanged", SYS_semctl, uid_ops, 1, GETVAL_);
    struct sb two[2] = { { 0, 1, SEM_UNDO_ }, { 0, -1, SEM_UNDO_ } };
    S("undo twice", SYS_semop, uid_ops, two, 2);
    S("IPC_RMID with adjustments", SYS_semctl, uid_ops, 0, IPC_RMID);
    in_child("nothing left to undo", child_inherit);
}

/* execve keeps the list: the exec'd image's end undoes. */
static void undoexec(char **argv) {
    if (argv[2]) {
        int id = atoi(argv[2]);
        S("after exec", SYS_semctl, id, 0, GETVAL_);
        return;
    }
    fresh_ns();
    int id = semget_(IPC_PRIVATE, 1, 0600);
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        struct sb o = { 0, 9, SEM_UNDO_ };
        syscall(SYS_semop, id, &o, 1);
        char num[16];
        snprintf(num, sizeof num, "%d", id);
        char *a[] = { argv[0], "undoexec", num, 0 };
        execv("/proc/self/exe", a);
        _exit(77);
    }
    int st;
    waitpid(p, &st, 0);
    printf("exec child exit %d\n", WEXITSTATUS(st));
    S("undone after exec's end", SYS_semctl, id, 0, GETVAL_);
}

/* A seccomp filter sees the number like any other: semget answers ENOTTY through the filter, semop stays. */
static void filtered(void) {
    int id = semget_(IPC_PRIVATE, 1, 0600);
    struct sock_filter f[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_semget, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOTTY),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog prog = { sizeof f / sizeof *f, f };
    S("no new privs", SYS_prctl, PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    S("filter", SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog);
    S("semget filtered", SYS_semget, IPC_PRIVATE, 1, 0600);
    struct sb o = { 0, 1, 0 };
    S("semop passes", SYS_semop, id, &o, 1);
    S("IPC_RMID", SYS_semctl, id, 0, IPC_RMID);
}

/* The initial namespace: what Steam does first (no id numbers: other programs use the table). */
static void plain(void) {
    int id = semget_(IPC_PRIVATE, 2, IPC_CREAT | 0600);
    yes("semget works", id >= 0);
    struct sb o = { 0, 1, SEM_UNDO_ };
    S("semop", SYS_semop, id, &o, 1);
    S("GETVAL", SYS_semctl, id, 0, GETVAL_);
    S("IPC_RMID", SYS_semctl, id, 0, IPC_RMID);
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "plain";
    setvbuf(stdout, 0, _IOLBF, 0);
    if (!strcmp(m, "basic")) basic();
    else if (!strcmp(m, "order")) order();
    else if (!strcmp(m, "signals")) signals();
    else if (!strcmp(m, "undo")) undo();
    else if (!strcmp(m, "undoexec")) undoexec(argv);
    else if (!strcmp(m, "seccomp")) filtered();
    else plain();
    return 0;
}
