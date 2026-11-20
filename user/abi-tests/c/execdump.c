/* L7-RESTE-AB 1: execve of a program the caller may not read (linux-7.3.1/fs/exec.c:1359 would_dump). This file
 * runs itself from a copy in /tmp: mode 0755 (readable) and 0111 (run-only: the owner's bits win, no read).
 * The run-only child is not dumpable, so its parent (same uid) gets no /proc/<pid>/mem, environ or exe and no
 * process_vm_readv, and the files of /proc/<pid> belong to root. With a user namespace whose gid map is missing, the exec
 * user namespace climbs to the initial one, so the namespace's owner is refused as well. Run as root, it first
 * becomes nobody (root reads everything). */
#include "abi.h"
#include <fcntl.h>
#include <sched.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>

static char self[64], copy[64];

static int copy_self(int mode) {
    unlink(copy);
    int in = open("/proc/self/exe", O_RDONLY), out = open(copy, O_WRONLY | O_CREAT | O_TRUNC, 0700);
    if (in < 0 || out < 0)
        return -1;
    char b[65536];
    long n;
    while ((n = read(in, b, sizeof b)) > 0)
        if (write(out, b, n) != n)
            return -1;
    close(in);
    close(out);
    return chmod(copy, mode);
}

static void put(int fd, const char *s) {
    if (write(fd, s, strlen(s)) < 0)
        _exit(9);
}

/* The new program: tells whether it is dumpable for its owner (SUID_DUMP_USER; the value otherwise is the
 * host's fs.suid_dumpable) and where the parent may read, waits for the parent's look, ends. */
static int child(int ready, int go) {
    struct { char dumpable; void *at; } m = {prctl(PR_GET_DUMPABLE) == 1 ? 'y' : 'n', self};
    char c;
    if (write(ready, &m, sizeof m) != sizeof m || read(go, &c, 1) < 0)
        return 9;
    return 0;
}

/* Writes the user namespace maps of a fresh namespace: uid always, gid only when asked. */
static int maps(int with_gid, unsigned uid, unsigned gid) {
    char t[64];
    int fd = open("/proc/self/setgroups", O_WRONLY);
    if (fd < 0 || write(fd, "deny", 4) != 4)
        return -1;
    close(fd);
    snprintf(t, sizeof t, "0 %u 1", uid);
    if ((fd = open("/proc/self/uid_map", O_WRONLY)) < 0 || write(fd, t, strlen(t)) < 0)
        return -1;
    close(fd);
    if (!with_gid)
        return 0;
    snprintf(t, sizeof t, "0 %u 1", gid);
    if ((fd = open("/proc/self/gid_map", O_WRONLY)) < 0 || write(fd, t, strlen(t)) < 0)
        return -1;
    close(fd);
    return 0;
}

static void look(const char *what, int mode, int userns) {
    int r[2], g[2];
    if (copy_self(mode) || pipe(r) || pipe(g)) {
        printf("%s: setup failed\n", what);
        return;
    }
    unsigned uid = getuid(), gid = getgid();
    pid_t pid = fork();
    if (pid == 0) {
        if (userns && (unshare(CLONE_NEWUSER) || maps(userns == 2, uid, gid)))
            _exit(8);
        char rs[16], gs[16];
        snprintf(rs, sizeof rs, "%d", r[1]);
        snprintf(gs, sizeof gs, "%d", g[0]);
        char *av[] = {copy, "child", rs, gs, 0};
        execv(copy, av);
        _exit(7);
    }
    close(r[1]);
    close(g[0]);
    struct { char dumpable; void *at; } m;
    if (read(r[0], &m, sizeof m) != sizeof m) {
        int st;
        waitpid(pid, &st, 0);
        printf("%s: child failed, status 0x%x\n", what, st);
        return;
    }
    printf("%s: child dumpable for its owner = %s\n", what, m.dumpable == 'y' ? "yes" : "no");
    char p[64], l[64];
    snprintf(p, sizeof p, "/proc/%d/mem", pid);
    int fd = open(p, O_RDONLY);
    printf("%s: open mem = %s\n", what, fd >= 0 ? "ok" : ename(errno));
    if (fd >= 0)
        close(fd);
    snprintf(p, sizeof p, "/proc/%d/environ", pid);
    fd = open(p, O_RDONLY);
    printf("%s: open environ = %s\n", what, fd >= 0 ? "ok" : ename(errno));
    if (fd >= 0)
        close(fd);
    snprintf(p, sizeof p, "/proc/%d/exe", pid);
    long n = readlink(p, l, sizeof l);
    printf("%s: readlink exe = %s\n", what, n >= 0 ? "ok" : ename(errno));
    char buf[16];
    struct iovec li = {buf, sizeof buf}, ri = {m.at, sizeof buf};
    long v = syscall(SYS_process_vm_readv, pid, &li, 1, &ri, 1, 0);
    printf("%s: process_vm_readv = %s\n", what, v >= 0 ? "ok" : ename(errno));
    struct stat st;
    snprintf(p, sizeof p, "/proc/%d/status", pid);
    printf("%s: /proc/<pid>/status owner = %s\n", what, stat(p, &st) ? ename(errno) : st.st_uid == uid ? "caller" : st.st_uid == 0 ? "root" : "other");
    put(g[1], "q");
    int ws;
    waitpid(pid, &ws, 0);
    printf("%s: exit status 0x%x\n", what, ws);
    close(r[0]);
    close(g[1]);
}

/* "nodac" (root only, ktest ab.l7.execdump): uid 0 without CAP_DAC_READ_SEARCH and CAP_DAC_OVERRIDE may not read a
 * run-only file of another owner (the mode bits decide, generic_permission), so the image it runs is not dumpable;
 * with the capabilities it reads and stays dumpable. */
static void nodac_case(const char *what, int drop) {
    int r[2], g[2];
    if (copy_self(0711) || chown(copy, 1000, 1000) || pipe(r) || pipe(g)) {
        printf("%s: setup failed\n", what);
        return;
    }
    pid_t pid = fork();
    if (pid == 0) {
        struct { unsigned version; int pid; } h = {0x20080522, 0};
        struct { unsigned eff, perm, inh; } d[2];
        if (syscall(SYS_capget, &h, d))
            _exit(8);
        unsigned keep = drop ? ~(1u << 1 | 1u << 2) : ~0u; /* CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH */
        d[0].eff &= keep;
        d[0].perm &= keep;
        d[0].inh &= keep;
        if (syscall(SYS_capset, &h, d))
            _exit(8);
        char rs[16], gs[16];
        snprintf(rs, sizeof rs, "%d", r[1]);
        snprintf(gs, sizeof gs, "%d", g[0]);
        char *av[] = {copy, "child", rs, gs, 0};
        execv(copy, av);
        _exit(7);
    }
    close(r[1]);
    close(g[0]);
    struct { char dumpable; void *at; } m;
    int ws;
    if (read(r[0], &m, sizeof m) != sizeof m) {
        waitpid(pid, &ws, 0);
        printf("%s: child failed, status 0x%x\n", what, ws);
        return;
    }
    printf("%s: child dumpable = %s\n", what, m.dumpable == 'y' ? "yes" : "no");
    put(g[1], "q");
    waitpid(pid, &ws, 0);
    printf("%s: exit status 0x%x\n", what, ws);
    close(r[0]);
    close(g[1]);
}

static int nodac(void) {
    if (geteuid() != 0) {
        printf("nodac: needs root\n");
        return 0;
    }
    snprintf(copy, sizeof copy, "/tmp/abi-execdump-%d", getpid());
    nodac_case("root, run-only file of uid 1000", 0);
    nodac_case("root without DAC capabilities, run-only file of uid 1000", 1);
    unlink(copy);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 4 && !strcmp(argv[1], "child"))
        return child(atoi(argv[2]), atoi(argv[3]));
    setvbuf(stdout, 0, _IONBF, 0);
    if (argc == 2 && !strcmp(argv[1], "nodac"))
        return nodac();
    if (geteuid() == 0 && (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))) {
        printf("cannot drop root\n");
        return 1;
    }
    snprintf(copy, sizeof copy, "/tmp/abi-execdump-%d", getpid());
    look("readable", 0755, 0);
    look("run-only", 0111, 0);
    look("run-only, user namespace with both maps", 0111, 2);
    look("run-only, user namespace without gid map", 0111, 1);
    unlink(copy);
    return 0;
}
