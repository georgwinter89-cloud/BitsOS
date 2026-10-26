/* AB-13: the credential calls as an ordinary user (CachyOS runs the ABI diff unprivileged; the root paths are
 * the ktests ab.cred.*). Prints only errnos and relations: the user's ids and groups are the host's. */
#define _GNU_SOURCE
#include "abi.h"
#include <grp.h>
#include <fcntl.h>
#include <pthread.h>
#include <sys/auxv.h>
#include <sys/fsuid.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/wait.h>

static int status_line(const char *key, char *out, size_t n) {
    FILE *f = fopen("/proc/self/status", "r");
    if (!f)
        return 0;
    char line[512];
    int found = 0;
    while (!found && fgets(line, sizeof line, f))
        if (!strncmp(line, key, strlen(key))) {
            snprintf(out, n, "%s", line);
            found = 1;
        }
    fclose(f);
    return found;
}

static void user_rules(void) {
    long uid = getuid(), gid = getgid();
    yes("not root (the diff runs as a user)", uid != 0 && geteuid() != 0);
    SYS("setuid(own)", SYS_setuid, uid);
    SYS("setgid(own)", SYS_setgid, gid);
    SYS("setuid(0)", SYS_setuid, 0);
    SYS("setgid(0)", SYS_setgid, 0);
    SYS("setuid(-1)", SYS_setuid, -1);
    SYS("setgid(-1)", SYS_setgid, -1);
    SYS("setreuid(-1, -1)", SYS_setreuid, -1, -1);
    SYS("setreuid(own, own)", SYS_setreuid, uid, uid);
    SYS("setreuid(-1, 0)", SYS_setreuid, -1, 0);
    SYS("setreuid(0, -1)", SYS_setreuid, 0, -1);
    SYS("setregid(-1, -1)", SYS_setregid, -1, -1);
    SYS("setregid(0, -1)", SYS_setregid, 0, -1);
    SYS("setresuid(-1, -1, -1)", SYS_setresuid, -1, -1, -1);
    SYS("setresuid(own, own, own)", SYS_setresuid, uid, uid, uid);
    SYS("setresuid(-1, -1, 0)", SYS_setresuid, -1, -1, 0);
    SYS("setresuid(-2, -1, -1)", SYS_setresuid, -2, -1, -1);
    SYS("setresgid(-1, -1, -1)", SYS_setresgid, -1, -1, -1);
    SYS("setresgid(-1, 0, -1)", SYS_setresgid, -1, 0, -1);
    yes("ids unchanged", getuid() == uid && geteuid() == uid && getgid() == gid && getegid() == gid);
    yes("setuid(own) keeps the memory dumpable", prctl(PR_GET_DUMPABLE) == 1);
}

static void fs_ids(void) {
    long uid = getuid(), gid = getgid();
    yes("setfsuid(-1) returns the fsuid", syscall(SYS_setfsuid, -1) == uid);
    yes("setfsuid(0) refused silently", syscall(SYS_setfsuid, 0) == uid && syscall(SYS_setfsuid, -1) == uid);
    yes("setfsuid(own) returns it", syscall(SYS_setfsuid, uid) == uid);
    yes("setfsgid(-1) returns the fsgid", syscall(SYS_setfsgid, -1) == gid);
    yes("setfsgid(0) refused silently", syscall(SYS_setfsgid, 0) == gid && syscall(SYS_setfsgid, -1) == gid);
}

static void groups(void) {
    SYS("setgroups(0, NULL)", SYS_setgroups, 0, NULL);
    SYS("setgroups(70000, NULL)", SYS_setgroups, 70000, NULL);
    SYS("setgroups(-1, NULL)", SYS_setgroups, -1, NULL);
    SYS("getgroups(-1)", SYS_getgroups, -1, NULL);
    long n = syscall(SYS_getgroups, 0, NULL);
    yes("getgroups(0) >= 0", n >= 0);
    gid_t *g = calloc(n + 1, sizeof *g);
    yes("getgroups(count) == count", syscall(SYS_getgroups, n + 1, g) == n);
    int sorted = 1;
    for (long i = 1; i < n; i++)
        sorted &= g[i - 1] <= g[i];
    yes("the list comes sorted", sorted);
    errno = 0;
    yes("getgroups(count - 1) is EINVAL or there are none", n == 0 || (syscall(SYS_getgroups, n - 1, g) == -1 && errno == EINVAL));
    errno = 0;
    yes("getgroups into the kernel faults or there are none", n == 0 || (syscall(SYS_getgroups, n, KERNEL_ADDR) == -1 && errno == EFAULT));
    free(g);
}

static void status(void) {
    char l[512], want[128];
    long uid = getuid(), gid = getgid();
    snprintf(want, sizeof want, "Uid:\t%ld\t%ld\t%ld\t%ld\n", uid, uid, uid, uid);
    yes("status Uid: real eff saved fs", status_line("Uid:", l, sizeof l) && !strcmp(l, want));
    snprintf(want, sizeof want, "Gid:\t%ld\t%ld\t%ld\t%ld\n", gid, gid, gid, gid);
    yes("status Gid: real eff saved fs", status_line("Gid:", l, sizeof l) && !strcmp(l, want));
    yes("status Groups: ends with a space", status_line("Groups:", l, sizeof l) && strlen(l) >= 9 && !strcmp(l + strlen(l) - 2, " \n"));
    yes("status CapEff is empty", status_line("CapEff:", l, sizeof l) && !strcmp(l, "CapEff:\t0000000000000000\n"));
}

/* The alarm clocks as Linux with an RTC device (AB-13, nplb PosixTimeClockGettime): readable, 1 ns, and a
 * user's sleep needs CAP_WAKE_ALARM. */
static void alarm_clocks(void) {
    struct timespec t = { 0, 1000 };
    for (int id = 8; id <= 9; id++) {
        char l[64];
        snprintf(l, sizeof l, "clock_gettime(%d)", id);
        SYS(l, SYS_clock_gettime, id, &t);
        snprintf(l, sizeof l, "clock_getres(%d)", id);
        SYS(l, SYS_clock_getres, id, &t);
        printf("res %ld.%09ld\n", (long)t.tv_sec, t.tv_nsec);
        t = (struct timespec){ 0, 1000 };
        snprintf(l, sizeof l, "clock_nanosleep(%d) as a user", id);
        SYS(l, SYS_clock_nanosleep, id, 0, &t, NULL);
        snprintf(l, sizeof l, "clock_nanosleep(%d, flags 2)", id);
        SYS(l, SYS_clock_nanosleep, id, 2, &t, NULL);
    }
}

/* capget/capset (linux-6.6/kernel/capability.c) as a user: empty sets, the version handshake, and only
 * unchanged sets may be set. */
static void caps(void) {
    uint32_t h[2] = { 0x20080522, 0 }, d[6] = { 0 };
    SYS("capget(v3, self)", SYS_capget, h, d);
    /* Effective and permitted only: the inheritable set is the host's (a session may pass some down). */
    yes("a user's effective and permitted sets are empty", !d[0] && !d[1] && !d[3] && !d[4]);
    h[0] = 0x12345678;
    SYS("capget(bad version)", SYS_capget, h, d);
    yes("the preferred version comes back", h[0] == 0x20080522);
    h[0] = 0x12345678;
    SYS("capget(bad version, NULL data)", SYS_capget, h, NULL);
    h[1] = -1;
    SYS("capget(pid -1)", SYS_capget, h, d);
    h[1] = 1;
    SYS("capget(pid 1)", SYS_capget, h, d);
    yes("init has capabilities", d[0] != 0);
    h[0] = 0x19980330;
    SYS("capget(pid 1, v1)", SYS_capget, h, d);
    SYS("capget into the kernel", SYS_capget, h, KERNEL_ADDR);
    h[0] = 0x20080522;
    h[1] = 0;
    SYS("capget(self) again", SYS_capget, h, d);
    SYS("capset(the same sets)", SYS_capset, h, d);
    d[1] = 1;
    SYS("capset(raise CAP_CHOWN)", SYS_capset, h, d);
    d[1] = 0;
    h[1] = 1;
    SYS("capset(pid 1)", SYS_capset, h, d);
}

/* Root modes (run as uid 0 by the ktests ab.cred.inherit and ab.cred.files, never by the diff). */
static void ids_of(const char *who) {
    static const char *k[] = { "Uid:", "Gid:", "Groups:" };
    char l[512];
    for (int i = 0; i < 3; i++)
        if (status_line(k[i], l, sizeof l))
            printf("%s %s", who, l);
}

static int stop_pipe[2];

static void *waiter(void *a) {
    char c;
    (void)a;
    while (read(stop_pipe[0], &c, 1) < 0 && errno == EINTR)
        ;
    return NULL;
}

/* fork copies all ids and groups, execve keeps them with saved = effective; with a second thread every
 * set*id goes through musl's __synccall (a failure in the second thread would SIGKILL the process). */
static int root_inherit(const char *self) {
    pthread_t t;
    if (pipe(stop_pipe) || pthread_create(&t, NULL, waiter, NULL))
        return 2;
    gid_t g[] = { 1000, 5 };
    show("setgroups", setgroups(2, g));
    show("setresgid(7, 8, 9)", setresgid(7, 8, 9));
    show("setresuid(2000, 1000, 0)", setresuid(2000, 1000, 0));
    write(stop_pipe[1], "x", 1);
    pthread_join(t, NULL);
    ids_of("self");
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        ids_of("child");
        fflush(stdout);
        _exit(0);
    }
    int st = 0;
    show("child", waitpid(c, &st, 0) == c && WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    fflush(stdout);
    char *argv[] = { "abi-cred", "shown", NULL }, *envp[] = { "PATH=/bin", NULL };
    execve(self, argv, envp);
    show("execve", -1);
    return 1;
}

static int shown(void) {
    ids_of("exec");
    printf("AT_SECURE %lu\n", getauxval(AT_SECURE));
    return 0;
}

static int make(const char *path, mode_t mode, gid_t gid) {
    unlink(path);
    int fd = open(path, O_CREAT | O_EXCL | O_WRONLY, 0600);
    int ok = fd >= 0 && !fchown(fd, 0, gid) && !fchmod(fd, mode);
    if (fd >= 0)
        close(fd);
    return ok;
}

static void try_open(const char *label, const char *path) {
    int fd = open(path, O_RDONLY);
    show(label, fd < 0 ? -1 : 0);
    if (fd >= 0)
        close(fd);
}

/* File access follows fsuid/fsgid and the supplementary groups at once, without an execve. */
static int root_files(void) {
    char x[64], g[64], h[64];
    snprintf(x, sizeof x, "/tmp/abi-cred-%d-x", getpid());
    snprintf(g, sizeof g, "/tmp/abi-cred-%d-g", getpid());
    snprintf(h, sizeof h, "/tmp/abi-cred-%d-h", getpid());
    yes("files made", make(x, 0600, 0) && make(g, 0040, 50) && make(h, 0040, 51));
    setfsuid(1000);
    try_open("fsuid 1000: root's 0600 file", x);
    setfsuid(0);
    try_open("fsuid 0: root's 0600 file", x);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        gid_t gs[] = { 50 };
        int ok = !setgroups(1, gs) && !setgid(1000) && !setuid(1000);
        yes("dropped to 1000 with group 50", ok && geteuid() == 1000);
        try_open("uid 1000: root's 0600 file", x);
        try_open("uid 1000: 0040 file of group 50", g);
        try_open("uid 1000: 0040 file of group 51", h);
        fflush(stdout);
        _exit(0);
    }
    int st = 0;
    waitpid(c, &st, 0);
    unlink(x);
    unlink(g);
    unlink(h);
    return 0;
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "root"))
        return root_inherit(argv[0][0] == '/' ? argv[0] : "/bin/abi-cred");
    if (argc > 1 && !strcmp(argv[1], "shown"))
        return shown();
    if (argc > 1 && !strcmp(argv[1], "files"))
        return root_files();
    caps();
    alarm_clocks();
    user_rules();
    fs_ids();
    groups();
    status();
    return 0;
}
