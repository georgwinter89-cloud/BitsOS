/* L7-VFS: linkat(2) with AT_EMPTY_PATH as an ordinary user (linux-7.3.1/fs/namei.c:5948 filename_linkat, :2750
 * path_init's LOOKUP_LINKAT_EMPTY): a start fd the caller opened itself links without CAP_DAC_READ_SEARCH (the
 * O_TMPFILE pattern of glibc and systemd); one opened before a fork (the child has new credentials) is ENOENT,
 * also with a name, after EBADF and EFAULT, before ENOTDIR; an absolute name or AT_FDCWD is never checked. An
 * execve gives new credentials too (fs/exec.c prepare_exec_creds), so an inherited fd fails after it. A pipe,
 * eventfd or memfd sits on a pseudo file system: EXDEV, after the new name's EEXIST/ENOENT (filename_create). */
#include "ns.h"
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/syscall.h>

static char dir[64];

static const char *at(const char *name) {
    static char b[4][96];
    static int i;
    i = (i + 1) % 4;
    snprintf(b[i], sizeof b[i], "%s/%s", dir, name);
    return b[i];
}

static long la(int fd, const char *name, const char *to, int flags) {
    return syscall(SYS_linkat, fd, name, AT_FDCWD, at(to), flags);
}

static void nlink(const char *label, const char *name) {
    struct stat st;
    if (stat(at(name), &st))
        printf("%s = -%s\n", label, en(errno));
    else
        printf("%s = %lu\n", label, (unsigned long)st.st_nlink);
}

/* After the execve: fd 3 is the parent's O_TMPFILE fd (no close-on-exec), argv[2] the directory. */
static int exec_child(const char *d) {
    snprintf(dir, sizeof dir, "%s", d);
    R("exec: inherited fd", la(3, "", "x1", AT_EMPTY_PATH));
    R("exec: inherited fd without the flag", syscall(SYS_linkat, 3, "", AT_FDCWD, at("x1"), 0));
    int own = open(at("e1"), O_RDONLY);
    R("exec: own fd", la(own, "", "x2", AT_EMPTY_PATH));
    return 0;
}

/* Fds without a name in a file system the caller can see. */
static void pseudo(void) {
    int p[2];
    if (pipe(p))
        return (void)R("pipe", -1);
    int e = eventfd(0, 0);
    int m = syscall(SYS_memfd_create, "abi", 0);
    R("pipe end", la(p[0], "", "p1", AT_EMPTY_PATH));
    R("pipe end onto an existing name", la(p[0], "", "e1", AT_EMPTY_PATH));
    R("pipe end into a missing directory", la(p[0], "", "none/p1", AT_EMPTY_PATH));
    R("eventfd", la(e, "", "p2", AT_EMPTY_PATH));
    R("memfd", la(m, "", "p3", AT_EMPTY_PATH));
    close(p[0]);
    close(p[1]);
    close(e);
    close(m);
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    if (argc > 2 && !strcmp(argv[1], "exec"))
        return exec_child(argv[2]);
    snprintf(dir, sizeof dir, "/tmp/abi-linkat-%d", getpid());
    if (mkdir(dir, 0755)) {
        printf("mkdir = -%s\n", en(errno));
        return 0;
    }
    int t = open(dir, O_TMPFILE | O_RDWR, 0600);
    int d = open(dir, O_RDONLY | O_DIRECTORY);
    R("O_TMPFILE", t < 0 ? -1 : 0);
    R("write it", write(t, "temp", 4));
    R("own O_TMPFILE fd", la(t, "", "e1", AT_EMPTY_PATH));
    R("own directory fd with a name", la(d, "e1", "e2", AT_EMPTY_PATH));
    R("absolute name", syscall(SYS_linkat, AT_FDCWD, at("e1"), AT_FDCWD, at("e3"), AT_EMPTY_PATH));
    R("bad fd", la(999, "", "x", AT_EMPTY_PATH));
    R("bad name pointer", la(t, (const char *)1, "x", AT_EMPTY_PATH));
    R("file fd with a name", la(t, "x", "x", AT_EMPTY_PATH));
    pid_t pid = fork();
    if (pid == 0) {
        R("child: parent's fd", la(t, "", "c1", AT_EMPTY_PATH));
        R("child: parent's directory fd with a name", la(d, "e1", "c1", AT_EMPTY_PATH));
        R("child: parent's file fd with a name", la(t, "x", "c1", AT_EMPTY_PATH));
        R("child: bad fd", la(999, "", "c1", AT_EMPTY_PATH));
        R("child: absolute name", syscall(SYS_linkat, AT_FDCWD, at("e1"), AT_FDCWD, at("c2"), AT_EMPTY_PATH));
        int own = open(at("e1"), O_RDONLY);
        R("child: own fd", la(own, "", "c3", AT_EMPTY_PATH));
        R("child: parent's directory fd without the flag", la(d, "e1", "c4", 0));
        _exit(0);
    }
    reap("child", pid);
    pseudo();
    char self[256] = {0};
    if (readlink("/proc/self/exe", self, sizeof self - 1) < 0)
        R("readlink", -1);
    else if (dup2(t, 3) != 3)
        R("dup2", -1);
    else if ((pid = fork()) == 0) {
        char *av[] = {"abi-linkat-empty", "exec", dir, NULL}, *ev[] = {NULL};
        execve(self, av, ev);
        _exit(127);
    } else
        reap("exec child", pid);
    nlink("nlink", "e1");
    R("parent's fd still links", la(t, "", "e4", AT_EMPTY_PATH));
    const char *names[] = {"e1", "e2", "e3", "e4", "c1", "c2", "c3", "c4", "x1", "x2", "p1", "p2", "p3"};
    for (unsigned i = 0; i < sizeof names / sizeof *names; i++)
        unlink(at(names[i]));
    close(t);
    close(d);
    R("rmdir", rmdir(dir));
    return 0;
}
