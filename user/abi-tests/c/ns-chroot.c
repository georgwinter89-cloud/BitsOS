/* SX-2: chroot(2) (linux-7.3.1/fs/open.c:591) and Chromium's ChrootToSafeEmptyDir (sandbox/linux/services/
 * credentials.cc, its observable calls): a CLONE_VM|CLONE_FS|CLONE_VFORK child chroots into
 * /proc/self/fdinfo/ and ends, so the parent keeps a root that is the fdinfo directory of a task that is gone:
 * every later lookup below it must fail (mode `trick`: ESRCH from the fdinfo directory's permission op,
 * linux-7.3.1/fs/proc/fd.c:85 proc_fdinfo_permission, also for chdir). Default mode: the errno corners and a plain chroot inside a
 * user namespace. Both: a chrooted process makes no user namespace (kernel/user_namespace.c create_user_ns,
 * current_chrooted). */
#include "ns.h"
#include <sys/mman.h>
#include <sys/prctl.h>

static int trick_child(void *a) {
    (void)a;
    if (syscall(SYS_chroot, "/proc/self/fdinfo/"))
        syscall(SYS_exit_group, 10 + errno);
    if (syscall(SYS_chdir, "/"))
        syscall(SYS_exit_group, 50 + errno);
    syscall(SYS_exit_group, 0);
    return 0;
}

static void lookups(const char *who) {
    static const char *paths[] = { "/proc/self/maps", "/proc/self/cmdline", "/proc/stat", "/sys/devices/system/cpu/online",
                                   "/sys/devices/system/cpu/cpu0/tsc_freq_khz", "/bin/sh", "/0", "/../proc" };
    for (unsigned i = 0; i < sizeof paths / sizeof *paths; i++) {
        char l[128];
        snprintf(l, sizeof l, "%s: open(%s)", who, paths[i]);
        int fd = open(paths[i], O_RDONLY);
        R(l, fd < 0 ? -1 : 0);
        if (fd >= 0)
            close(fd);
    }
    struct stat st;
    char l[96];
    snprintf(l, sizeof l, "%s: stat(/proc)", who);
    R(l, stat("/proc", &st));
}

static int chromium(void) {
    R("unshare(CLONE_NEWUSER)", unshare(CLONE_NEWUSER));
    size_t sz = 64 * 1024;
    char *stack = mmap(0, sz, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
    pid_t p = clone(trick_child, stack + sz, CLONE_VM | CLONE_FS | CLONE_VFORK | SIGCHLD, 0);
    printf("clone(CLONE_VM|CLONE_FS|CLONE_VFORK) = %s\n", yn(p > 0));
    reap("chroot child", p);
    lookups("after the trick");
    char *argv[] = { "/bin/sh", "-c", ":", 0 };
    R("execve(/bin/sh)", execve("/bin/sh", argv, 0));
    R("unshare(CLONE_NEWUSER) chrooted", unshare(CLONE_NEWUSER));
    R("chdir(/..)", chdir("/.."));
    return 0;
}

static int plain(void) {
    R("unshare(CLONE_NEWUSER)", unshare(CLONE_NEWUSER));
    R("chdir(/proc/self)", chdir("/proc/self"));
    R("chroot(/proc)", chroot("/proc"));
    struct stat st;
    R("stat(/self/status)", stat("/self/status", &st));
    R("stat(/../self)", stat("/../self", &st));
    char b[256];
    long n = readlink("/self/root", b, sizeof b - 1);
    if (n >= 0)
        b[n] = 0;
    printf("readlink(/self/root) = %s\n", n < 0 ? en(errno) : b);
    char cwd[256];
    char want[32];
    snprintf(want, sizeof want, "/%d", (int)getpid());
    printf("getcwd before chdir(/) is /<pid> below the new root = %s\n", yn(getcwd(cwd, sizeof cwd) && !strcmp(cwd, want)));
    R("chdir(/)", chdir("/"));
    printf("getcwd = %s\n", getcwd(cwd, sizeof cwd) ? cwd : en(errno));
    R("open(/self/status) relative to the new root", open("/self/status", O_RDONLY) >= 0 ? 0 : -1);
    R("unshare(CLONE_NEWUSER) chrooted", unshare(CLONE_NEWUSER));
    R("chroot(/) again", chroot("/"));
    return 0;
}

/* Mode "files" (FS-16): CLONE_FILES and CLONE_FS are independent (kernel/fork.c copy_files, copy_fs): a chroot
 * reaches a sibling that shares the fd table only when it shares the fs state too (fs/open.c set_fs_root). */
static int gate2[2];

static int files_child(void *a) {
    const char *who = a;
    char c;
    if (read(gate2[0], &c, 1) != 1)
        syscall(SYS_exit_group, 1);
    struct stat st;
    char l[128];
    snprintf(l, sizeof l, "%s sibling: stat(/self/status) (below /proc)", who);
    R(l, stat("/self/status", &st));
    snprintf(l, sizeof l, "%s sibling: stat(/proc/self/status) (below /)", who);
    R(l, stat("/proc/self/status", &st));
    syscall(SYS_exit_group, 0);
    return 0;
}

static int files_case(int flags, const char *who) {
    R("unshare(CLONE_NEWUSER)", unshare(CLONE_NEWUSER));
    if (pipe(gate2))
        return 1;
    size_t sz = 64 * 1024;
    char *stack = mmap(0, sz, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_STACK, -1, 0);
    pid_t p = clone(files_child, stack + sz, flags | SIGCHLD, (void *)who);
    printf("clone(%s) = %s\n", who, yn(p > 0));
    char l[128];
    snprintf(l, sizeof l, "chroot(/proc) with a %s sibling", who);
    R(l, chroot("/proc"));
    if (write(gate2[1], "g", 1) != 1)
        return 1;
    snprintf(l, sizeof l, "%s sibling", who);
    reap(l, p);
    return 0;
}

static int files(void) {
    static const struct { int flags; const char *who; } c[] = { { CLONE_FILES, "CLONE_FILES" }, { CLONE_FILES | CLONE_FS, "CLONE_FILES|CLONE_FS" } };
    for (unsigned i = 0; i < sizeof c / sizeof *c; i++) {
        pid_t p = fork();
        if (p == 0)
            _exit(files_case(c[i].flags, c[i].who));
        reap(c[i].who, p);
    }
    return 0;
}

/* fchroot (linux-7.3.1/fs/open.c:621, syscall 472): an open directory, or FD_FAILFS_ROOT (-10004), whose
 * every operation fails with EOPNOTSUPP; unprivileged only with no_new_privs and an unshared fs state. */
static int fchroot_cases(void) {
    int tmp = open("/tmp", O_RDONLY | O_DIRECTORY), file = open("/proc/self/status", O_RDONLY);
    int path = open("/tmp", O_PATH | O_DIRECTORY);
    R("fchroot(-1)", syscall(472, -1, 0));
    R("fchroot(dir, 1)", syscall(472, tmp, 1));
    R("fchroot(a file)", syscall(472, file, 0));
    R("fchroot(dir) unprivileged", syscall(472, tmp, 0));
    R("fchroot(failfs) without no_new_privs", syscall(472, -10004, 0));
    pid_t p = fork();
    if (p == 0) {
        R("child: unshare(CLONE_NEWUSER)", unshare(CLONE_NEWUSER));
        R("child: fchroot(O_PATH dir)", syscall(472, path, 0));
        struct stat st;
        R("child: stat(/proc) under /tmp", stat("/proc", &st));
        _exit(0);
    }
    reap("fchroot child", p);
    p = fork();
    if (p == 0) {
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        R("nnp child: fchroot(failfs)", syscall(472, -10004, 0));
        R("nnp child: open(/bin/sh)", open("/bin/sh", O_RDONLY));
        R("nnp child: open(/)", open("/", O_RDONLY));
        struct stat st;
        R("nnp child: stat(/)", stat("/", &st));
        R("nnp child: stat(/..)", stat("/..", &st));
        R("nnp child: chdir(/)", chdir("/"));
        R("nnp child: chdir(/..)", chdir("/.."));
        R("nnp child: open(/, O_PATH)", open("/", O_PATH));
        R("nnp child: access(/, X_OK)", access("/", X_OK));
        R("nnp child: fchroot(failfs) again", syscall(472, -10004, 0));
        _exit(0);
    }
    reap("nnp child", p);
    close(tmp);
    close(file);
    close(path);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IOLBF, 0);
    if (argc > 1 && !strcmp(argv[1], "fchroot"))
        return fchroot_cases();
    if (argc > 1 && !strcmp(argv[1], "files"))
        return files();
    if (argc > 1 && !strcmp(argv[1], "trick")) {
        pid_t p = fork();
        if (p == 0)
            _exit(chromium());
        reap("chromium's trick", p);
        return 0;
    }
    R("chroot(/) unprivileged", chroot("/"));
    R("chroot(NULL)", syscall(SYS_chroot, 0));
    R("chroot(a kernel address)", syscall(SYS_chroot, KERNEL_ADDR));
    R("chroot(\"\")", chroot(""));
    R("chroot(/nonexistent)", chroot("/nonexistent"));
    R("chroot(/proc/self/status)", chroot("/proc/self/status"));
    R("chroot(/proc/self/status/x)", chroot("/proc/self/status/x"));
    pid_t p = fork();
    if (p == 0)
        _exit(plain());
    reap("plain chroot", p);
    return 0;
}
