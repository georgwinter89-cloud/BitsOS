/* SX-2: helpers for the namespace ABI programs (abi-ns-*). Every line must be the same on CachyOS and V2 when
 * the kernels behave the same: results, errno names and yes/no relations, never pids or inode numbers. Both
 * sides run the programs as uid 1000 without capabilities (CachyOS allows unprivileged user namespaces). */
#ifndef NS_H
#define NS_H
#include "abi.h"
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <sys/stat.h>
#include <sys/wait.h>

static inline const char *en(int e) {
    switch (e) {
    case ENETUNREACH: return "ENETUNREACH";
    case EADDRNOTAVAIL: return "EADDRNOTAVAIL";
    case ECONNREFUSED: return "ECONNREFUSED";
    case ENOSPC: return "ENOSPC";
    case EUSERS: return "EUSERS";
    case EAFNOSUPPORT: return "EAFNOSUPPORT";
    case EBUSY: return "EBUSY";
    }
    return ename(e);
}

/* "label = <result>" or "label = -<ERRNO>". */
static inline long rep(const char *label, long r) {
    if (r == -1)
        printf("%s = -%s\n", label, en(errno));
    else
        printf("%s = %ld\n", label, r);
    return r;
}
#define R(label, expr) rep(label, (long)(expr))

/* Writes `text` to `path` in one write: the write's result (or the open's error). */
static inline long wr(const char *label, const char *path, const char *text) {
    int fd = open(path, O_WRONLY);
    if (fd < 0)
        return rep(label, -1);
    long r = write(fd, text, strlen(text));
    int e = errno;
    close(fd);
    errno = e;
    return rep(label, r);
}

/* Reads up to n-1 bytes of `path` into out: the length, or -1. */
static inline long rd(const char *path, char *out, size_t n) {
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        return -1;
    long r = read(fd, out, n - 1);
    close(fd);
    out[r < 0 ? 0 : r] = 0;
    return r;
}

/* The fields of a /proc/self/status line ("NSpid:" etc.) into v; their count. */
static inline int status_fields(const char *key, long *v, int max) {
    char buf[4096];
    if (rd("/proc/self/status", buf, sizeof buf) <= 0)
        return -1;
    char *p = strstr(buf, key);
    if (!p)
        return -1;
    p += strlen(key);
    int n = 0;
    while (*p && *p != '\n' && n < max) {
        char *end;
        long x = strtol(p, &end, 10);
        if (end == p)
            break;
        v[n++] = x;
        p = end;
    }
    return n;
}

/* fork-like raw clone (no new stack): the child returns 0. */
static inline long rclone(unsigned long flags) {
    return syscall(SYS_clone, flags, 0, 0, 0, 0);
}

/* Waits for `pid` and prints how it ended. */
static inline void reap(const char *label, pid_t pid) {
    int st = 0;
    if (waitpid(pid, &st, 0) != pid)
        printf("%s: waitpid = -%s\n", label, en(errno));
    else if (WIFEXITED(st))
        printf("%s: exit %d\n", label, WEXITSTATUS(st));
    else if (WIFSIGNALED(st))
        printf("%s: signal %d\n", label, WTERMSIG(st));
}

static inline const char *yn(int b) {
    return b ? "yes" : "no";
}
#endif
