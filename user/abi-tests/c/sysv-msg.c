/* SX-IPC: message queues against linux-7.3.1/ipc/msg.c. Modes: basic, block, plain. */
#include "sysv.h"

struct m { long type; char text[9000]; };
static struct m b;

static long snd(int id, long type, const char *t, size_t n, int fl) {
    b.type = type;
    memcpy(b.text, t, n);
    return syscall(SYS_msgsnd, id, &b, n, fl);
}

/* "label = n type T text ..." */
static void rcv(const char *label, int id, size_t sz, long type, int fl) {
    memset(&b, 0, sizeof b);
    long r = syscall(SYS_msgrcv, id, &b, sz, type, fl);
    if (r < 0)
        printf("%s = -%s\n", label, sysv_err(errno));
    else
        printf("%s = %ld type %ld text %.*s\n", label, r, b.type, (int)r, b.text);
    fflush(stdout);
}

static void q_stat(const char *label, int id) {
    struct msq64 q;
    memset(&q, 0x55, sizeof q);
    if (syscall(SYS_msgctl, id, IPC_STAT, &q)) {
        printf("%s: stat %s\n", label, sysv_err(errno));
        return;
    }
    printf("%s: cbytes %lu qnum %lu qbytes %lu mode %o key %x uid %u gid %u stime %d rtime %d lspid me %d lrpid me %d\n", label, q.cbytes, q.qnum, q.qbytes, q.p.mode, q.p.key, q.p.uid, q.p.gid, q.stime > 0, q.rtime > 0, q.lspid == getpid(), q.lrpid == getpid());
}

static void basic(void) {
    fresh_ns();
    no_caps();
    S("msgget no key", SYS_msgget, K, 0600);
    int id = S("msgget create", SYS_msgget, K, IPC_CREAT | 0640);
    S("msgget again", SYS_msgget, K, 0);
    S("msgget excl", SYS_msgget, K, IPC_CREAT | IPC_EXCL | 0640);
    S("private", SYS_msgget, IPC_PRIVATE, 0600);
    q_stat("new", id);
    S("stat fault", SYS_msgctl, id, IPC_STAT, KERNEL_ADDR);
    S("stat negative", SYS_msgctl, -1, IPC_STAT, &b);
    S("cmd negative", SYS_msgctl, id, -1, &b);
    S("bad cmd", SYS_msgctl, id, 77, &b);
    S("stale id", SYS_msgctl, id + (1 << 15), IPC_STAT, &b);
    int mi[8];
    long max = S("IPC_INFO", SYS_msgctl, 0, IPC_INFO, mi);
    printf("info %d %d %d %d %d %d %d %u (%ld)\n", mi[0], mi[1], mi[2], mi[3], mi[4], mi[5], mi[6], ((unsigned short *)mi)[14], max);

    errno = 0;
    printf("type 0 = %ld %s\n", snd(id, 0, "x", 1, 0), sysv_err(errno));
    errno = 0;
    printf("negative type = %ld %s\n", snd(id, -1, "x", 1, 0), sysv_err(errno));
    errno = 0;
    printf("over msgmax = %ld %s\n", snd(id, 1, "x", 8193, 0), sysv_err(errno));
    S("negative size", SYS_msgsnd, id, &b, -1L, 0);
    S("negative id", SYS_msgsnd, -1, &b, 1, 0);
    S("snd fault", SYS_msgsnd, id, KERNEL_ADDR, 1, 0);
    S("snd bad text", SYS_msgsnd, id, (char *)mmap(0, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) + PAGE - 8, 100, 0);
    S("bad id", SYS_msgsnd, 4242, &b, 1, 0);

    snd(id, 5, "five", 4, 0);
    snd(id, 3, "three", 5, 0);
    snd(id, 7, "seven", 5, 0);
    snd(id, 3, "again", 5, 0);
    snd(id, 1, "", 0, 0);
    q_stat("five queued", id);
    struct { int a, b, c, d, e, f, g; unsigned short h; } info;
    S("MSG_INFO", SYS_msgctl, 0, MSG_INFO_, &info);
    printf("msg_info pool %d map %d tql %d\n", info.a, info.b, info.g);
    struct msq64 q;
    S("MSG_STAT index 0", SYS_msgctl, 0, MSG_STAT_, &q);
    S("MSG_STAT free", SYS_msgctl, 7, MSG_STAT_, &q);
    S("MSG_STAT_ANY", SYS_msgctl, 1, MSG_STAT_ANY_, &q);

    rcv("copy 0", id, 100, 0, MSG_COPY_ | IPC_NOWAIT);
    rcv("copy 2", id, 100, 2, MSG_COPY_ | IPC_NOWAIT);
    rcv("copy 9", id, 100, 9, MSG_COPY_ | IPC_NOWAIT);
    rcv("copy needs NOWAIT", id, 100, 0, MSG_COPY_);
    rcv("copy with EXCEPT", id, 100, 0, MSG_COPY_ | IPC_NOWAIT | MSG_EXCEPT_);
    rcv("copy small", id, 2, 0, MSG_COPY_ | IPC_NOWAIT);
    rcv("copy small NOERROR", id, 2, 0, MSG_COPY_ | IPC_NOWAIT | MSG_NOERROR_);
    S("copy bad buffer", SYS_msgrcv, id, KERNEL_ADDR, 8, 0, MSG_COPY_ | IPC_NOWAIT);
    q_stat("copies change nothing", id);
    rcv("too small", id, 2, 5, IPC_NOWAIT);
    rcv("cut", id, 2, 5, IPC_NOWAIT | MSG_NOERROR_);
    rcv("type 9 none", id, 100, 9, IPC_NOWAIT);
    rcv("except 3 first is seven? no: three first", id, 100, 3, IPC_NOWAIT | MSG_EXCEPT_);
    rcv("lowest up to 3", id, 100, -3, IPC_NOWAIT);
    rcv("lowest up to 3 again", id, 100, -3, IPC_NOWAIT);
    rcv("LONG_MIN", id, 100, (long)0x8000000000000000ul, IPC_NOWAIT);
    rcv("empty", id, 100, 0, IPC_NOWAIT);
    q_stat("empty", id);
    S("rcv negative id", SYS_msgrcv, -1, &b, 1, 0, 0);
    S("rcv negative size", SYS_msgrcv, id, &b, -1L, 0, 0);
    snd(id, 2, "lost", 4, 0);
    S("rcv fault loses it", SYS_msgrcv, id, KERNEL_ADDR, 8, 0, IPC_NOWAIT);
    q_stat("after the fault", id);
    rcv("type 2", id, 100, 2, IPC_NOWAIT);

    q.p.mode = 0200;
    S("IPC_STAT", SYS_msgctl, id, IPC_STAT, &q);
    q.p.mode = 0200;
    q.qbytes = 100;
    S("IPC_SET", SYS_msgctl, id, IPC_SET, &q);
    S("stat wants rights", SYS_msgctl, id, IPC_STAT, &q);
    rcv("receive wants rights", id, 100, 0, IPC_NOWAIT);
    q.p.mode = 0600;
    q.qbytes = 16385;
    S("qbytes over msgmnb", SYS_msgctl, id, IPC_SET, &q);
    q.qbytes = 10;
    S("qbytes down", SYS_msgctl, id, IPC_SET, &q);
    q_stat("set", id);
    printf("fits = %ld\n", snd(id, 1, "0123456789", 10, IPC_NOWAIT));
    errno = 0;
    printf("full = %ld %s\n", snd(id, 1, "x", 1, IPC_NOWAIT), sysv_err(errno));
    S("IPC_SET fault", SYS_msgctl, id, IPC_SET, KERNEL_ADDR);
    S("IPC_RMID", SYS_msgctl, id, IPC_RMID, 0);
    S("IPC_RMID again", SYS_msgctl, id, IPC_RMID, 0);
    S("MSG_INFO", SYS_msgctl, 0, MSG_INFO_, &info);
    printf("msg_info pool %d map %d tql %d\n", info.a, info.b, info.g);
}

static int qid;

static void note(const char *what, long r) {
    pthread_mutex_lock(&log_lock);
    char one[64];
    snprintf(one, sizeof one, "%s=%ld ", what, r == -1 ? -(long)errno : r);
    strcat(logline, one);
    pthread_mutex_unlock(&log_lock);
}

struct rx { const char *name; long type; int flg; size_t sz; };

static void *receiver(void *a) {
    struct rx *r = a;
    struct m one;
    note(r->name, syscall(SYS_msgrcv, qid, &one, r->sz, r->type, r->flg));
    return 0;
}

struct tx { const char *name; size_t n; };

static void *sender(void *a) {
    struct tx *t = a;
    struct m one;
    one.type = 1;
    memset(one.text, 'x', t->n);
    note(t->name, syscall(SYS_msgsnd, qid, &one, t->n, 0));
    return 0;
}

static void on_usr1(int s) { (void)s; }

static int main_tid;

static void *kicker(void *a) {
    wait_asleep(main_tid);
    pthread_kill(*(pthread_t *)a, SIGUSR1);
    return 0;
}

static void block(void) {
    fresh_ns();
    qid = syscall(SYS_msgget, IPC_PRIVATE, 0600);
    main_tid = syscall(SYS_gettid);
    pthread_t t[6];
    /* Receivers in order, by type: a message goes directly to the first that fits. */
    struct rx r1 = { "r1", 2, 0, 100 }, r2 = { "r2", 1, 0, 100 }, r3 = { "r3", 0, 0, 100 };
    go(&t[0], receiver, &r1);
    go(&t[1], receiver, &r2);
    go(&t[2], receiver, &r3);
    snd(qid, 1, "ab", 2, 0);
    flush_log("type 1 goes to r2", 1);
    snd(qid, 9, "abc", 3, 0);
    flush_log("type 9 goes to r3", 1);
    q_stat("nothing queued", qid);
    snd(qid, 2, "abcd", 4, 0);
    flush_log("type 2 goes to r1", 1);
    for (int i = 0; i < 3; i++) pthread_join(t[i], 0);

    /* A receiver with a small buffer gets E2BIG and the next one the message. */
    struct rx s1 = { "small", 0, 0, 1 }, s2 = { "big", 0, 0, 100 };
    go(&t[0], receiver, &s1);
    go(&t[1], receiver, &s2);
    snd(qid, 1, "abc", 3, 0);
    flush_log("too big for the first", 2);
    pthread_join(t[0], 0); pthread_join(t[1], 0);

    /* Senders wait for room in order, and a smaller one behind a big one may pass. */
    struct msq64 q;
    syscall(SYS_msgctl, qid, IPC_STAT, &q);
    q.qbytes = 8;
    syscall(SYS_msgctl, qid, IPC_SET, &q);
    snd(qid, 1, "1234", 4, 0);
    snd(qid, 1, "5678", 4, 0);
    struct tx big = { "big", 8 }, small = { "small", 2 };
    go(&t[0], sender, &big);
    go(&t[1], sender, &small);
    flush_log("both wait", 0);
    struct m got;
    long n = syscall(SYS_msgrcv, qid, &got, 100, 0, IPC_NOWAIT);
    printf("took %ld\n", n);
    flush_log("room for the small one only", 1);
    n = syscall(SYS_msgrcv, qid, &got, 100, 0, IPC_NOWAIT);
    printf("took %ld\n", n);
    flush_log("still not for the big one", 0);
    n = syscall(SYS_msgrcv, qid, &got, 100, 0, IPC_NOWAIT);
    printf("took %ld\n", n);
    flush_log("room for the big one", 1);
    pthread_join(t[0], 0); pthread_join(t[1], 0);
    q_stat("sent", qid);
    /* IPC_SET wakes the senders; here the room grows. */
    struct tx wait2 = { "w2", 8 };
    go(&t[0], sender, &wait2);
    flush_log("waits", 0);
    syscall(SYS_msgctl, qid, IPC_STAT, &q);
    q.qbytes = 100;
    syscall(SYS_msgctl, qid, IPC_SET, &q);
    flush_log("room by IPC_SET", 1);
    pthread_join(t[0], 0);
    while (syscall(SYS_msgrcv, qid, &got, 100, 0, IPC_NOWAIT) >= 0) {}

    /* IPC_SET wakes receivers to look again: they keep waiting. */
    struct rx again = { "again", 0, 0, 100 };
    go(&t[0], receiver, &again);
    syscall(SYS_msgctl, qid, IPC_STAT, &q);
    syscall(SYS_msgctl, qid, IPC_SET, &q);
    flush_log("still waiting", 0);
    snd(qid, 1, "z", 1, 0);
    flush_log("gets it", 1);
    pthread_join(t[0], 0);

    /* Signals: EINTR with a handler, SA_RESTART restarts. */
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, 0);
    pthread_t me = pthread_self(), k;
    pthread_create(&k, 0, kicker, &me);
    S("msgrcv interrupted", SYS_msgrcv, qid, &got, 100, 0, 0);
    pthread_join(k, 0);
    sa.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &sa, 0);
    pthread_create(&k, 0, kicker, &me);
    S("msgrcv with SA_RESTART: EINTR all the same (ERESTARTNOHAND)", SYS_msgrcv, qid, &got, 100, 0, 0);
    pthread_join(k, 0);
    /* A stop and continue is no handler: the call goes on and gets the message. */
    fflush(stdout);
    pid_t me_pid = getpid(), kid = fork();
    if (kid == 0) {
        char path[64], st[512];
        snprintf(path, sizeof path, "/proc/%d/stat", me_pid);
        for (int i = 0; i < 5000; i++) {
            rd(path, st, sizeof st);
            char *p = strrchr(st, ')');
            if (p && p[2] == 'S')
                break;
            msleep(1);
        }
        msleep(10);
        kill(me_pid, SIGSTOP);
        msleep(30);
        kill(me_pid, SIGCONT);
        msleep(30);
        struct m late;
        late.type = 1;
        syscall(SYS_msgsnd, qid, &late, 3, 0);
        _exit(0);
    }
    S("msgrcv through stop and continue", SYS_msgrcv, qid, &got, 100, 0, 0);
    waitpid(kid, 0, 0);
    sa.sa_flags = 0;
    sigaction(SIGUSR1, &sa, 0);
    syscall(SYS_msgctl, qid, IPC_STAT, &q);
    q.qbytes = 1;
    syscall(SYS_msgctl, qid, IPC_SET, &q);
    snd(qid, 1, "z", 1, 0);
    pthread_create(&k, 0, kicker, &me);
    struct m one;
    one.type = 1;
    S("msgsnd interrupted", SYS_msgsnd, qid, &one, 1, 0);
    pthread_join(k, 0);

    /* Removal: EIDRM for sleepers on both sides. */
    struct tx lastt = { "tx", 1 };
    go(&t[0], sender, &lastt);
    syscall(SYS_msgctl, qid, IPC_RMID, 0);
    flush_log("sender at removal", 1);
    pthread_join(t[0], 0);
    qid = syscall(SYS_msgget, IPC_PRIVATE, 0600);
    struct rx lastr = { "rx", 0, 0, 100 };
    go(&t[0], receiver, &lastr);
    syscall(SYS_msgctl, qid, IPC_RMID, 0);
    flush_log("receiver at removal", 1);
    pthread_join(t[0], 0);
}

static void plain(void) {
    int id = syscall(SYS_msgget, IPC_PRIVATE, 0600);
    yes("msgget works", id >= 0);
    printf("send %ld\n", snd(id, 4, "hello", 5, 0));
    rcv("receive", id, 100, 4, 0);
    S("remove", SYS_msgctl, id, IPC_RMID, 0);
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "plain";
    setvbuf(stdout, 0, _IOLBF, 0);
    if (!strcmp(m, "basic")) basic();
    else if (!strcmp(m, "block")) block();
    else plain();
    return 0;
}
