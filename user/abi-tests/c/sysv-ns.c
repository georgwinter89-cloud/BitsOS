/* SX-IPC: IPC namespaces (linux-7.3.1/ipc/namespace.c, kernel/nsproxy.c, kernel/fork.c) and the /proc files of
 * ipc_sysctl.c and util.c. Modes: ns, proc, plainproc. */
#include "sysv.h"

static void text(const char *path) {
    char b[1024];
    long n = rd(path, b, sizeof b);
    if (n < 0)
        printf("%s: %s\n", path, en(errno));
    else
        printf("%s: %s", path, b);
    fflush(stdout);
}

/* A line's length as if its pid columns had their format's width: real pids may be longer than %5u. */
static long pid_len(long len, int a, int b) {
    char n[16];
    for (int i = 0; i < 2; i++) {
        long d = snprintf(n, sizeof n, "%d", i ? b : a);
        if (d > 5)
            len -= d - 5;
    }
    return len;
}

static void ns(void) {
    fresh_ns();
    int oldfd = open("/proc/self/ns/ipc", O_RDONLY);
    int sem = S("semget in first", SYS_semget, K, 1, IPC_CREAT | 0600);
    int shm = S("shmget in first", SYS_shmget, K, 4096, IPC_CREAT | 0600);
    int msg = S("msgget in first", SYS_msgget, K, IPC_CREAT | 0600);
    syscall(SYS_semctl, sem, 0, SETVAL_, 5);
    char *m = (char *)syscall(SYS_shmat, shm, 0, 0);
    m[0] = 'a';
    struct sb u = { 0, 2, SEM_UNDO_ };
    S("first: +2 undo", SYS_semop, sem, &u, 1);
    S("unshare(CLONE_NEWIPC)", SYS_unshare, CLONE_NEWIPC);
    S("undo applied in the old one is out of reach: id here", SYS_semctl, sem, 0, GETVAL_);
    S("key gone", SYS_semget, K, 1, 0);
    S("key gone", SYS_shmget, K, 4096, 0);
    S("key gone", SYS_msgget, K, 0);
    int sem2 = S("semget in second", SYS_semget, K, 2, IPC_CREAT | 0600);
    syscall(SYS_semctl, sem2, 0, SETVAL_, 9);
    yes("attached segment still mapped", m[0] == 'a');
    m[0] = 'b';
    int newfd = open("/proc/self/ns/ipc", O_RDONLY);
    S("setns back", SYS_setns, oldfd, CLONE_NEWIPC);
    S("first value: undo done by leaving", SYS_semctl, sem, 0, GETVAL_);
    int sh2 = S("first shm key", SYS_shmget, K, 4096, 0);
    char *n = (char *)syscall(SYS_shmat, sh2, 0, 0);
    yes("old memory", n[0] == 'b');
    struct shm64 st;
    syscall(SYS_shmctl, shm, IPC_STAT, &st);
    printf("nattch %lu\n", st.nattch);
    S("first msg key", SYS_msgget, K, 0);
    struct sb u2 = { 0, 3, SEM_UNDO_ };
    S("first: +3 undo", SYS_semop, sem, &u2, 1);
    S("setns to second", SYS_setns, newfd, CLONE_NEWIPC);
    S("second still has its value", SYS_semctl, sem2, 0, GETVAL_);
    S("setns nstype wrong", SYS_setns, oldfd, CLONE_NEWUTS);
    S("setns back", SYS_setns, oldfd, CLONE_NEWIPC);
    S("first: undo went at setns", SYS_semctl, sem, 0, GETVAL_);
    close(newfd);
    S("clone with CLONE_NEWIPC|CLONE_SYSVSEM", SYS_clone, CLONE_NEWIPC | CLONE_SYSVSEM | SIGCHLD, 0, 0, 0, 0);
    /* The second namespace is gone with its last holder; its segment's memory stays while mapped elsewhere. */
    fflush(stdout);
    long c = syscall(SYS_clone, CLONE_NEWIPC | SIGCHLD, 0, 0, 0, 0);
    if (c == 0) {
        S("child in a new namespace, key", SYS_semget, K, 1, 0);
        int cs = S("child makes a key", SYS_semget, K + 9, 1, IPC_CREAT | 0600);
        (void)cs;
        fflush(stdout);
        _exit(0);
    }
    int ws;
    waitpid(c, &ws, 0);
    S("the child's key is not here", SYS_semget, K + 9, 1, 0);
    (void)msg;
}

static void proc(void) {
    fresh_ns();
    const char *names[] = { "sem", "shmmax", "shmall", "shmmni", "msgmax", "msgmnb", "msgmni", "shm_rmid_forced", "auto_msgmni", "sem_next_id", "shm_next_id", "msg_next_id" };
    char path[64];
    for (unsigned i = 0; i < sizeof names / sizeof *names; i++) {
        snprintf(path, sizeof path, "/proc/sys/kernel/%s", names[i]);
        text(path);
        struct stat s;
        if (!stat(path, &s))
            printf("mode %o uid %u\n", s.st_mode & 0777, s.st_uid);
    }
    text("/proc/sysvipc/sem");
    text("/proc/sysvipc/shm");
    text("/proc/sysvipc/msg");
    int sem = syscall(SYS_semget, K, 3, IPC_CREAT | 0640);
    int shm = syscall(SYS_shmget, K, 5000, IPC_CREAT | 0604);
    int msg = syscall(SYS_msgget, K, IPC_CREAT | 0620);
    char m[16] = { 1, 0, 0, 0, 0, 0, 0, 0, 'h', 'i' };
    syscall(SYS_msgsnd, msg, m, 2, 0);
    printf("ids %d %d %d\n", sem, shm, msg);
    char b[1024];
    rd("/proc/sysvipc/sem", b, sizeof b);
    int k, id, nsems, u, g, cu, cg; unsigned mode; long ot, ct;
    sscanf(strchr(b, '\n') + 1, "%d %d %o %d %d %d %d %d %ld %ld", &k, &id, &mode, &nsems, &u, &g, &cu, &cg, &ot, &ct);
    printf("sem %d %d %o %d %d %d %d %d otime0 %d ctime %d\n", k, id, mode, nsems, u, g, cu, cg, ot == 0, ct > 0);
    /* Field widths come from the text itself. */
    char *l = strchr(b, '\n') + 1;
    char *end = strchr(l, '\n');
    printf("sem line len %ld\n", (long)(end - l));
    rd("/proc/sysvipc/shm", b, sizeof b);
    l = strchr(b, '\n') + 1;
    end = strchr(l, '\n');
    int cp, lp, nat; unsigned long size, rss, swp;
    long at, dt;
    sscanf(l, "%d %d %o %lu %d %d %d %d %d %d %d %ld %ld %ld %lu %lu", &k, &id, &mode, &size, &cp, &lp, &nat, &u, &g, &cu, &cg, &at, &dt, &ct, &rss, &swp);
    printf("shm line len %ld\n", pid_len(end - l, cp, lp));
    printf("shm %d %d %o %lu me %d lp %d nat %d %d %d %d %d rss %lu swp %lu\n", k, id, mode, size, cp == getpid(), lp, nat, u, g, cu, cg, rss, swp);
    rd("/proc/sysvipc/msg", b, sizeof b);
    l = strchr(b, '\n') + 1;
    end = strchr(l, '\n');
    unsigned long cb, qn;
    sscanf(l, "%d %d %o %lu %lu %d %d %d %d %d %d", &k, &id, &mode, &cb, &qn, &cp, &lp, &u, &g, &cu, &cg);
    printf("msg line len %ld\n", pid_len(end - l, cp, lp));
    printf("msg %d %d %o %lu %lu me %d lr %d %d %d %d %d\n", k, id, mode, cb, qn, cp == getpid(), lp, u, g, cu, cg);

    /* Writes, as the namespace's root. */
    wr("msgmax", "/proc/sys/kernel/msgmax", "100\n");
    text("/proc/sys/kernel/msgmax");
    char big[200] = { 1 };
    S("over the new msgmax", SYS_msgsnd, msg, big, 101, IPC_NOWAIT);
    wr("msgmax negative", "/proc/sys/kernel/msgmax", "-1\n");
    wr("msgmax junk", "/proc/sys/kernel/msgmax", "abc\n");
    wr("msgmax blank", "/proc/sys/kernel/msgmax", "  \n");
    wr("msgmax too big", "/proc/sys/kernel/msgmax", "2147483648\n");
    wr("msgmni over IPCMNI", "/proc/sys/kernel/msgmni", "32769\n");
    wr("msgmni at IPCMNI", "/proc/sys/kernel/msgmni", "32768\n");
    wr("shmmni over IPCMNI", "/proc/sys/kernel/shmmni", "32769\n");
    wr("msgmnb", "/proc/sys/kernel/msgmnb", "64\n");
    int q2 = syscall(SYS_msgget, IPC_PRIVATE, 0600);
    struct msq64 qs;
    syscall(SYS_msgctl, q2, IPC_STAT, &qs);
    printf("new queue qbytes %lu\n", qs.qbytes);
    wr("sem two values", "/proc/sys/kernel/sem", "11 22\n");
    text("/proc/sys/kernel/sem");
    S("over semmsl", SYS_semget, IPC_PRIVATE, 12, 0600);
    wr("sem four values", "/proc/sys/kernel/sem", "2 4 3 5\n");
    text("/proc/sys/kernel/sem");
    S("over semmns", SYS_semget, IPC_PRIVATE, 2, 0600);
    struct sb many[4] = { { 0, 1, 0 }, { 0, 1, 0 }, { 0, 1, 0 }, { 0, 1, 0 } };
    S("over semopm", SYS_semop, sem, many, 4);
    wr("sem semmni range", "/proc/sys/kernel/sem", "9 9 9 99999\n");
    text("/proc/sys/kernel/sem");
    wr("sem negative", "/proc/sys/kernel/sem", "-5\n");
    text("/proc/sys/kernel/sem");
    wr("sem bad", "/proc/sys/kernel/sem", "x\n");
    wr("forced range", "/proc/sys/kernel/shm_rmid_forced", "2\n");
    wr("auto_msgmni", "/proc/sys/kernel/auto_msgmni", "1\n");
    text("/proc/sys/kernel/auto_msgmni");
    wr("auto_msgmni range", "/proc/sys/kernel/auto_msgmni", "2\n");
    wr("shmmax hex", "/proc/sys/kernel/shmmax", "0x1000\n");
    text("/proc/sys/kernel/shmmax");
    wr("shm_next_id", "/proc/sys/kernel/shm_next_id", "98304\n");
    text("/proc/sys/kernel/shm_next_id");
    S("takes the asked id", SYS_shmget, IPC_PRIVATE, 4096, 0600);
    text("/proc/sys/kernel/shm_next_id");
    wr("sem_next_id bad", "/proc/sys/kernel/sem_next_id", "-1\n");
    /* Another namespace has its own. */
    S("unshare", SYS_unshare, CLONE_NEWIPC);
    text("/proc/sys/kernel/msgmax");
    text("/proc/sysvipc/sem");
    no_caps();
    wr("no-caps root still the owner", "/proc/sys/kernel/msgmax", "200\n");
    wr("next_id wants a capability", "/proc/sys/kernel/msg_next_id", "1\n");
    /* access() and the owner of the files. */
    printf("access W msgmax %d\n", access("/proc/sys/kernel/msgmax", W_OK));
    printf("access X msgmax %d\n", access("/proc/sys/kernel/msgmax", X_OK));
    printf("access W sysvipc %d\n", access("/proc/sysvipc/sem", W_OK));
    wr("sysvipc read only", "/proc/sysvipc/sem", "x");
}

/* The initial namespace as a normal user: read everything, write nothing. */
static void plainproc(void) {
    text("/proc/sys/kernel/sem");
    text("/proc/sys/kernel/shmmax");
    text("/proc/sys/kernel/shmall");
    text("/proc/sys/kernel/shmmni");
    text("/proc/sys/kernel/msgmax");
    text("/proc/sys/kernel/msgmnb");
    text("/proc/sys/kernel/msgmni");
    wr("write refused", "/proc/sys/kernel/msgmax", "100\n");
    wr("write refused", "/proc/sys/kernel/sem", "1\n");
    char b[256];
    rd("/proc/sysvipc/sem", b, sizeof b);
    *(strchr(b, '\n') + 1) = 0;
    printf("%s", b);
    rd("/proc/sysvipc/shm", b, sizeof b);
    *(strchr(b, '\n') + 1) = 0;
    printf("%s", b);
    rd("/proc/sysvipc/msg", b, sizeof b);
    *(strchr(b, '\n') + 1) = 0;
    printf("%s", b);
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "plainproc";
    setvbuf(stdout, 0, _IOLBF, 0);
    if (!strcmp(m, "ns")) ns();
    else if (!strcmp(m, "proc")) proc();
    else plainproc();
    return 0;
}
