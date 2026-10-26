/* fs-mountflags: the mount flags noexec and nodev as programs see them (FS-13), for the ABI diff (CachyOS is the
 * oracle; user/abi-tests/c/mountflags.c is the same file) and the ktest fs.mountflags.programs. Prints results and
 * errno names only (no paths, ids or numbers). Modes (argv[1]):
 *   proc      mmap, execve and access of a /proc file (/proc is nosuid,nodev,noexec on Linux and V2)
 *   tmpfs     Linux without root: a user and mount namespace, a tmpfs mounted nosuid,nodev,noexec at a fresh
 *             directory under /tmp, /dev/null bind-mounted onto <dir>/null and remounted nodev; then `at`'s probes
 *             (V2 has no unshare/mount before FS-11: the setup line differs there, a known case)
 *   at DIR    the probes on DIR, which must be a nosuid,nodev,noexec mount holding the character device `null`
 *             (1:3); the ktest makes that mount in the kernel. Control files go to /tmp.
 *   child     exits 42 (what the probes execute)
 * PT_INTERP on a noexec mount is not probed (it would need a dynamic ELF built for both systems); Linux opens
 * it with the same open_exec as the program and a script's interpreter, which the probes cover.
 * nosuid is not probed: a set-id program only changes ids for another owner, which needs root on CachyOS. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *en(int e) {
    static char b[16];
    switch (e) {
    case 0: return "0";
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case EACCES: return "EACCES";
    case ENODEV: return "ENODEV";
    case ENXIO: return "ENXIO";
    case EINVAL: return "EINVAL";
    case ENOSYS: return "ENOSYS";
    case ETXTBSY: return "ETXTBSY";
    case EOVERFLOW: return "EOVERFLOW";
    }
    snprintf(b, sizeof b, "E%d", e);
    return b;
}

static void line(const char *what, int err) {
    printf("%s: %s\n", what, en(err));
    fflush(stdout);
}

/* The per-mount options of the mount at `point` (field 6 of mountinfo; the last such mount counts). */
static void mount_opts(const char *label, const char *point) {
    FILE *f = fopen("/proc/self/mountinfo", "r");
    char l[1024], found[256] = "none";
    while (f && fgets(l, sizeof l, f)) {
        /* No sscanf: the image builds against glibc's headers and musl's libc.a (__isoc23_sscanf). */
        char *field[6], *save = NULL, *t = strtok_r(l, " ", &save);
        int n = 0;
        for (; t && n < 6; t = strtok_r(NULL, " ", &save)) field[n++] = t;
        if (n == 6 && strcmp(field[4], point) == 0) snprintf(found, sizeof found, "%s", field[5]);
    }
    if (f) fclose(f);
    printf("%s: %s\n", label, found);
    fflush(stdout);
}

/* execve `path` in a child: its errno, or how the child ended. */
static void exec_probe(const char *what, const char *path) {
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        char *argv[] = {"prog", "child", NULL}, *envp[] = {NULL};
        execve(path, argv, envp);
        line(what, errno);
        _exit(0);
    }
    int st = 0;
    waitpid(p, &st, 0);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 42) printf("%s: ran\n", what);
    fflush(stdout);
}

static int map_probe(int fd, int prot, int flags) {
    void *a = mmap(NULL, 4096, prot, flags, fd, 0);
    if (a == MAP_FAILED) return errno;
    munmap(a, 4096);
    return 0;
}

/* A copy of this program at `path` (0755), closed (an open writer would make execve ETXTBSY). */
static int copy_self(const char *path, int excl) {
    int in = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    int out = excl ? mkstemp((char *)path) : open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0755);
    if (in < 0 || out < 0) return errno;
    char b[65536];
    ssize_t n;
    while ((n = read(in, b, sizeof b)) > 0) {
        if (write(out, b, n) != n) return errno ? errno : EINVAL;
    }
    fchmod(out, 0755);
    close(in);
    close(out);
    return 0;
}

/* A script `#!<interp> child` at `path` (0755; mkstemp when tmpl), closed. */
static int write_script(char *path, int tmpl, const char *interp) {
    int fd = tmpl ? mkstemp(path) : open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0755);
    if (fd < 0) return errno;
    char b[600];
    int n = snprintf(b, sizeof b, "#!%s child\n", interp);
    int e = write(fd, b, n) == n ? 0 : (errno ? errno : EINVAL);
    fchmod(fd, 0755);
    close(fd);
    return e;
}

static int at(const char *dir) {
    char prog[512], ctrl[] = "/tmp/fs13-ctrl-XXXXXX", dev[512], fifo[512], script[512];
    char sh_interp[] = "/tmp/fs13-shi-XXXXXX", sh_plain[] = "/tmp/fs13-shp-XXXXXX";
    snprintf(prog, sizeof prog, "%s/prog", dir);
    snprintf(script, sizeof script, "%s/script", dir);
    snprintf(dev, sizeof dev, "%s/null", dir);
    snprintf(fifo, sizeof fifo, "%s/fifo", dir);
    mount_opts("mount", dir);
    int e = copy_self(prog, 0);
    if (e) { line("copy", e); return 1; }
    e = copy_self(ctrl, 1);
    if (e) { line("copy plain", e); return 1; }
    /* noexec: execve and access(X_OK) are EACCES; mmap PROT_EXEC is EPERM; without it VM_MAYEXEC goes. */
    exec_probe("exec noexec", prog);
    exec_probe("exec plain", ctrl);
    /* Scripts: the script itself on the noexec mount, and a script elsewhere whose interpreter lives there (both
     * open_exec in Linux: fs/exec.c:935 via binfmt_script.c). */
    if ((e = write_script(script, 0, ctrl)) || (e = write_script(sh_interp, 1, prog)) || (e = write_script(sh_plain, 1, ctrl))) {
        line("script", e);
        return 1;
    }
    exec_probe("exec script noexec", script);
    exec_probe("exec interp noexec", sh_interp);
    exec_probe("exec script plain", sh_plain);
    unlink(script);
    unlink(sh_interp);
    unlink(sh_plain);
    line("access X_OK noexec", access(prog, X_OK) ? errno : 0);
    line("access R_OK noexec", access(prog, R_OK) ? errno : 0);
    line("access X_OK plain", access(ctrl, X_OK) ? errno : 0);
    int fd = open(prog, O_RDONLY | O_CLOEXEC), cfd = open(ctrl, O_RDONLY | O_CLOEXEC);
    line("mmap R private", map_probe(fd, PROT_READ, MAP_PRIVATE));
    line("mmap RX private", map_probe(fd, PROT_READ | PROT_EXEC, MAP_PRIVATE));
    line("mmap RX shared", map_probe(fd, PROT_READ | PROT_EXEC, MAP_SHARED));
    line("mmap X private", map_probe(fd, PROT_EXEC, MAP_PRIVATE));
    void *a = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, fd, 0);
    line("mprotect RX", a == MAP_FAILED ? errno : (mprotect(a, 4096, PROT_READ | PROT_EXEC) ? errno : 0));
    if (a != MAP_FAILED) munmap(a, 4096);
    line("mmap RX private plain", map_probe(cfd, PROT_READ | PROT_EXEC, MAP_PRIVATE));
    close(fd);
    close(cfd);
    /* nodev: device nodes do not open (O_PATH does); FIFOs are no devices. */
    struct stat st;
    printf("stat dev: %s\n", stat(dev, &st) ? en(errno) : S_ISCHR(st.st_mode) ? "chr" : "other");
    fd = open(dev, O_RDONLY | O_CLOEXEC);
    line("open dev", fd < 0 ? errno : 0);
    if (fd >= 0) close(fd);
    fd = open(dev, O_PATH | O_CLOEXEC);
    line("open dev O_PATH", fd < 0 ? errno : 0);
    if (fd >= 0) close(fd);
    fd = open("/dev/null", O_RDONLY | O_CLOEXEC);
    line("open dev plain", fd < 0 ? errno : 0);
    if (fd >= 0) close(fd);
    line("mkfifo", mkfifo(fifo, 0600) ? errno : 0);
    fd = open(fifo, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    line("open fifo", fd < 0 ? errno : 0);
    if (fd >= 0) close(fd);
    unlink(fifo);
    unlink(prog);
    unlink(ctrl);
    printf("end\n");
    return 0;
}

static int write_file(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) return errno;
    int e = write(fd, text, strlen(text)) < 0 ? errno : 0;
    close(fd);
    return e;
}

static int tmpfs_mode(void) {
    char map[64], dir[] = "/tmp/fs13-mnt-XXXXXX", dev[64];
    uid_t uid = getuid();
    gid_t gid = getgid();
    if (!mkdtemp(dir)) { line("setup", errno); return 1; }
    int e = unshare(CLONE_NEWUSER | CLONE_NEWNS) ? errno : 0;
    if (e) { line("setup", e); rmdir(dir); return 1; }
    write_file("/proc/self/setgroups", "deny");
    snprintf(map, sizeof map, "0 %u 1", (unsigned)uid);
    write_file("/proc/self/uid_map", map);
    snprintf(map, sizeof map, "0 %u 1", (unsigned)gid);
    write_file("/proc/self/gid_map", map);
    snprintf(dev, sizeof dev, "%s/null", dir);
    if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) ||
        mount("none", dir, "tmpfs", MS_NOSUID | MS_NODEV | MS_NOEXEC, "mode=0777") ||
        close(open(dev, O_WRONLY | O_CREAT | O_CLOEXEC, 0666)) ||
        mount("/dev/null", dev, NULL, MS_BIND, NULL) ||
        mount(NULL, dev, NULL, MS_REMOUNT | MS_BIND | MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL)) {
        line("setup", errno);
        umount2(dir, MNT_DETACH);
        rmdir(dir);
        return 1;
    }
    line("setup", 0);
    int r = at(dir);
    umount2(dir, MNT_DETACH);
    rmdir(dir);
    return r;
}

static int proc_mode(void) {
    mount_opts("proc mount", "/proc");
    int fd = open("/proc/self/status", O_RDONLY | O_CLOEXEC);
    line("mmap R proc", map_probe(fd, PROT_READ, MAP_PRIVATE));
    line("mmap RX proc", map_probe(fd, PROT_READ | PROT_EXEC, MAP_PRIVATE));
    line("mmap RX shared proc", map_probe(fd, PROT_READ | PROT_EXEC, MAP_SHARED));
    close(fd);
    exec_probe("exec proc", "/proc/self/status");
    line("access X_OK proc", access("/proc/self/status", X_OK) ? errno : 0);
    printf("end\n");
    return 0;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "child") == 0) return 42;
    if (argc > 1 && strcmp(argv[1], "proc") == 0) return proc_mode();
    if (argc > 1 && strcmp(argv[1], "tmpfs") == 0) return tmpfs_mode();
    if (argc > 2 && strcmp(argv[1], "at") == 0) return at(argv[2]);
    fprintf(stderr, "usage: fs-mountflags proc|tmpfs|at DIR|child\n");
    return 2;
}
