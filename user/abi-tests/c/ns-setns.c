/* SX-2: /proc/<pid>/ns/ files and setns(2) (linux-6.6/fs/proc/namespaces.c, fs/nsfs.c, kernel/nsproxy.c setns,
 * kernel/user_namespace.c userns_install, kernel/utsname.c utsns_install, kernel/pid_namespace.c
 * pidns_install). Shaped like linux-6.6/tools/testing/selftests/nsfs. Inode numbers appear only as relations. */
#include "ns.h"
#include <sys/utsname.h>

static const char *names[] = { "net", "uts", "ipc", "pid", "pid_for_children", "user", "mnt" };

static unsigned long ino_of_link(const char *path, char *type, size_t n) {
    char b[128];
    long r = readlink(path, b, sizeof b - 1);
    if (r < 0)
        return 0;
    b[r] = 0;
    char *lb = strchr(b, '[');
    if (!lb || b[r - 1] != ']')
        return 0;
    snprintf(type, n, "%.*s", (int)(lb - b), b);
    return strtoul(lb + 1, 0, 10);
}

static unsigned long ino(const char *path) {
    struct stat st;
    return stat(path, &st) == 0 ? (unsigned long)st.st_ino : 0;
}

static int in_userns(void) {
    int up = open("/proc/self/ns/user", O_RDONLY);
    R("child: unshare(CLONE_NEWUSER|CLONE_NEWUTS)", unshare(CLONE_NEWUSER | CLONE_NEWUTS));
    R("child: sethostname(first)", sethostname("first", 5));
    int first = open("/proc/self/ns/uts", O_RDONLY);
    int own = open("/proc/self/ns/user", O_RDONLY);
    R("child: unshare(CLONE_NEWUTS)", unshare(CLONE_NEWUTS));
    R("child: sethostname(second)", sethostname("second", 6));
    R("child: setns(first uts, CLONE_NEWUTS)", setns(first, CLONE_NEWUTS));
    struct utsname u;
    uname(&u);
    printf("child: nodename %s\n", u.nodename);
    R("child: setns(own user namespace)", setns(own, CLONE_NEWUSER));
    R("child: setns(the parent's user namespace)", setns(up, CLONE_NEWUSER));
    R("child: setns(first uts, CLONE_NEWNET)", setns(first, CLONE_NEWNET));
    struct stat st;
    fstat(up, &st);
    printf("child: ns/user differs from the parent's = %s\n", yn((unsigned long)st.st_ino != ino("/proc/self/ns/user")));
    return 0;
}

/* Mode "fsshared": entering a user namespace needs an fs_struct nobody else uses (linux-6.6/kernel/user_namespace.c
 * userns_install: EINVAL while a CLONE_FS sibling shares it); after unshare(CLONE_FS) it works. */
static int fsshared(void) {
    int go[2], ready[2], hold[2];
    if (pipe(go) || pipe(ready))
        return 1;
    pid_t target = fork();
    if (target == 0) {
        close(go[1]);
        unshare(CLONE_NEWUSER);
        if (write(ready[1], "r", 1) != 1)
            _exit(1);
        char c;
        if (read(go[0], &c, 1) < 0)
            _exit(1);
        _exit(0);
    }
    close(go[0]);
    char c;
    if (read(ready[0], &c, 1) != 1)
        return 1;
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/ns/user", (int)target);
    int fd = open(path, O_RDONLY);
    printf("open the target's ns/user = %s\n", yn(fd >= 0));
    if (pipe(hold))
        return 1;
    R("chdir(/proc)", chdir("/proc"));
    long sib = rclone(CLONE_FS | SIGCHLD);
    if (sib == 0) {
        close(hold[1]);
        if (read(hold[0], &c, 1) < 0)
            _exit(1);
        char cwd[64];
        _exit(getcwd(cwd, sizeof cwd) && !strcmp(cwd, "/proc") ? 0 : 3);
    }
    R("setns(the target's user namespace) with a CLONE_FS sibling", setns(fd, CLONE_NEWUSER));
    R("unshare(CLONE_FS)", unshare(CLONE_FS));
    R("chdir(/)", chdir("/"));
    R("setns(the target's user namespace) alone", setns(fd, CLONE_NEWUSER));
    printf("in the target's namespace = %s\n", yn(ino("/proc/self/ns/user") == ino(path)));
    close(hold[1]);
    reap("sibling (exit 0: its cwd is still /proc)", (pid_t)sib);
    close(go[1]);
    reap("target", target);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IOLBF, 0);
    if (argc > 1 && !strcmp(argv[1], "fsshared"))
        return fsshared();
    for (unsigned i = 0; i < sizeof names / sizeof *names; i++) {
        char path[64], type[32] = "";
        snprintf(path, sizeof path, "/proc/self/ns/%s", names[i]);
        unsigned long n = ino_of_link(path, type, sizeof type);
        printf("%s: readlink type %s, stat inode is the number = %s\n", names[i], type, yn(n && n == ino(path)));
    }
    printf("pid and pid_for_children are one namespace = %s\n", yn(ino("/proc/self/ns/pid") == ino("/proc/self/ns/pid_for_children")));
    printf("user differs from net = %s\n", yn(ino("/proc/self/ns/user") != ino("/proc/self/ns/net")));
    int user = open("/proc/self/ns/user", O_RDONLY);
    int net = open("/proc/self/ns/net", O_RDONLY);
    int uts = open("/proc/self/ns/uts", O_RDONLY);
    int pidns = open("/proc/self/ns/pid", O_RDONLY);
    int file = open("/proc/self/status", O_RDONLY);
    printf("opened = %s\n", yn(user >= 0 && net >= 0 && uts >= 0 && pidns >= 0));
    struct stat st;
    fstat(user, &st);
    printf("an open ns file is the same inode = %s\n", yn((unsigned long)st.st_ino == ino("/proc/self/ns/user")));
    R("setns(-1, 0)", setns(-1, 0));
    R("setns(a regular file, 0)", setns(file, 0));
    R("setns(net, CLONE_NEWUTS)", setns(net, CLONE_NEWUTS));
    R("setns(own user, CLONE_NEWUSER)", setns(user, CLONE_NEWUSER));
    R("setns(own uts, 0) unprivileged", setns(uts, 0));
    R("setns(own net, CLONE_NEWNET) unprivileged", setns(net, CLONE_NEWNET));
    R("setns(own pid, CLONE_NEWPID) unprivileged", setns(pidns, CLONE_NEWPID));
    R("write to an ns file", write(user, "x", 1));

    pid_t p = fork();
    if (p == 0)
        _exit(in_userns());
    reap("child", p);

    /* The creator of a user namespace holds every capability in it: it may enter it. */
    int go[2], ready[2];
    if (pipe(go) || pipe(ready))
        return 1;
    pid_t target = fork();
    if (target == 0) {
        close(go[1]);
        unshare(CLONE_NEWUSER);
        if (write(ready[1], "r", 1) != 1)
            _exit(1);
        char c;
        if (read(go[0], &c, 1) < 0)
            _exit(1);
        _exit(0);
    }
    char c;
    if (read(ready[0], &c, 1) != 1)
        return 1;
    pid_t h = fork();
    if (h == 0) {
        char path[64];
        snprintf(path, sizeof path, "/proc/%d/ns/user", (int)target);
        int fd = open(path, O_RDONLY);
        printf("helper: open the target's ns/user = %s\n", yn(fd >= 0));
        R("helper: setns(the target's user namespace)", setns(fd, CLONE_NEWUSER));
        printf("helper: uid %d, in the target's namespace = %s\n", (int)getuid(), yn(ino("/proc/self/ns/user") == ino(path)));
        _exit(0);
    }
    reap("helper", h);
    close(go[1]);
    reap("target", target);
    return 0;
}
