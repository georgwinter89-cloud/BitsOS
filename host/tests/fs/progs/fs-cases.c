/* FS-1/FS-2 case runner: executes host/tests/fs/cases.txt (and dev-cases.txt) with raw syscalls and compares every result with the
 * expectation written there. The same binary runs on CachyOS (the truth: the list is checked against Linux)
 * and on BitsOS V2 (ktest fs.calls.linux_cases); the host test `sys_cases` runs the same list against the VFS.
 * Usage: fs-cases <cases-file> [base-dir]   (base default /tmp/fs-cases-<pid>; it must not exist).
 * Output: "FAIL <line>: <case> => got <x>" per mismatch, then "fs-cases: <p> passed, <f> failed". */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <sys/resource.h>
#include <signal.h>
#include <poll.h>
#include <sys/stat.h>

#define NR_read 0
#define NR_write 1
#define NR_open 2
#define NR_close 3
#define NR_stat 4
#define NR_fstat 5
#define NR_lstat 6
#define NR_lseek 8
#define NR_pread64 17
#define NR_pwrite64 18
#define NR_readv 19
#define NR_writev 20
#define NR_access 21
#define NR_dup 32
#define NR_dup2 33
#define NR_fcntl 72
#define NR_fsync 74
#define NR_truncate 76
#define NR_ftruncate 77
#define NR_getcwd 79
#define NR_chdir 80
#define NR_fchdir 81
#define NR_rename 82
#define NR_mkdir 83
#define NR_rmdir 84
#define NR_creat 85
#define NR_link 86
#define NR_unlink 87
#define NR_symlink 88
#define NR_readlink 89
#define NR_umask 95
#define NR_getdents64 217
#define NR_openat 257
#define NR_mkdirat 258
#define NR_newfstatat 262
#define NR_unlinkat 263
#define NR_linkat 265
#define NR_readlinkat 267
#define NR_dup3 292
#define NR_renameat2 316
#define NR_statx 332
#define NR_faccessat2 439
#define NR_pipe 22
#define NR_ioctl 16
#define NR_poll 7
#define NR_mknod 133
#define NR_mknodat 259
#define NR_eventfd 284
#define NR_fallocate 285
#define NR_eventfd2 290
#define NR_pipe2 293
#define NR_memfd_create 319
/* FS-1b */
#define NR_sendfile 40
#define NR_flock 73
#define NR_chmod 90
#define NR_fchmod 91
#define NR_chown 92
#define NR_fchown 93
#define NR_lchown 94
#define NR_utime 132
#define NR_statfs 137
#define NR_fstatfs 138
#define NR_sync 162
#define NR_fadvise64 221
#define NR_utimes 235
#define NR_fchownat 260
#define NR_futimesat 261
#define NR_fchmodat 268
#define NR_utimensat 280
#define NR_preadv 295
#define NR_pwritev 296
#define NR_syncfs 306
#define NR_copy_file_range 326
#define NR_preadv2 327
#define NR_pwritev2 328
#define NR_fchmodat2 452

static long sc(long n, long a, long b, long c, long d, long e) {
    long r = syscall(n, a, b, c, d, e, 0);
    return r == -1 ? -errno : r;
}
static long sc6(long n, long a, long b, long c, long d, long e, long f) {
    long r = syscall(n, a, b, c, d, e, f);
    return r == -1 ? -errno : r;
}

static const struct { const char *n; int v; } errs[] = {
    {"EPERM", 1}, {"ENOENT", 2}, {"EIO", 5}, {"ENXIO", 6}, {"EBADF", 9}, {"EAGAIN", 11}, {"ENOMEM", 12},
    {"EACCES", 13}, {"EFAULT", 14}, {"EBUSY", 16}, {"EEXIST", 17}, {"EXDEV", 18}, {"ENOTDIR", 20},
    {"EISDIR", 21}, {"EINVAL", 22}, {"EMFILE", 24}, {"ENOTTY", 25}, {"EFBIG", 27}, {"ENOSPC", 28},
    {"ESPIPE", 29}, {"EROFS", 30}, {"EMLINK", 31}, {"ERANGE", 34}, {"ENAMETOOLONG", 36}, {"ENOSYS", 38},
    {"ENOTEMPTY", 39}, {"ELOOP", 40}, {"EOPNOTSUPP", 95}, {"EINTR", 4}, {"EOVERFLOW", 75}, {"EPIPE", 32}, {"ENODEV", 19},
    {0, 0}};

static const struct { const char *n; long v; } names[] = {
    {"O_RDONLY", 0}, {"O_WRONLY", 1}, {"O_RDWR", 2}, {"O_CREAT", 0100}, {"O_EXCL", 0200}, {"O_TRUNC", 01000},
    {"O_APPEND", 02000}, {"O_NONBLOCK", 04000}, {"O_DIRECTORY", 0200000}, {"O_NOFOLLOW", 0400000},
    {"O_CLOEXEC", 02000000}, {"O_PATH", 010000000}, {"SEEK_SET", 0}, {"SEEK_CUR", 1}, {"SEEK_END", 2},
    {"SEEK_DATA", 3}, {"SEEK_HOLE", 4}, {"F_DUPFD", 0}, {"F_GETFD", 1}, {"F_SETFD", 2}, {"F_GETFL", 3},
    {"F_SETFL", 4}, {"F_DUPFD_CLOEXEC", 1030}, {"F_OK", 0}, {"R_OK", 4}, {"W_OK", 2}, {"X_OK", 1},
    {"AT_FDCWD", -100}, {"AT_EMPTY_PATH", 0x1000}, {"AT_SYMLINK_NOFOLLOW", 0x100}, {"AT_REMOVEDIR", 0x200},
    {"AT_SYMLINK_FOLLOW", 0x400}, {"O_TMPFILE", 020200000}, {"F_GETLK", 5}, {"F_SETLK", 6}, {"F_SETLKW", 7},
    {"F_OFD_GETLK", 36}, {"F_OFD_SETLK", 37}, {"F_OFD_SETLKW", 38}, {"F_RDLCK", 0}, {"F_WRLCK", 1}, {"F_UNLCK", 2},
    {"O_DIRECT", 040000}, {"F_SETPIPE_SZ", 1031}, {"F_GETPIPE_SZ", 1032}, {"EFD_SEMAPHORE", 1},
    {"S_IFIFO", 010000}, {"S_IFCHR", 020000}, {"S_IFDIR", 040000}, {"S_IFBLK", 060000}, {"S_IFREG", 0100000},
    {"S_IFSOCK", 0140000}, {"POLLIN", 1}, {"POLLPRI", 2}, {"POLLOUT", 4}, {"POLLRDHUP", 0x2000},
    {"F_ADD_SEALS", 1033}, {"F_GET_SEALS", 1034}, {"F_SEAL_SEAL", 1}, {"F_SEAL_SHRINK", 2}, {"F_SEAL_GROW", 4},
    {"F_SEAL_WRITE", 8}, {"F_SEAL_FUTURE_WRITE", 16}, {"F_SEAL_EXEC", 32}, {"MFD_CLOEXEC", 1},
    {"MFD_ALLOW_SEALING", 2}, {"MFD_NOEXEC_SEAL", 8}, {"MFD_EXEC", 16},
    {"FALLOC_FL_KEEP_SIZE", 1}, {"FALLOC_FL_PUNCH_HOLE", 2}, {"FALLOC_FL_COLLAPSE_RANGE", 8},
    {"FALLOC_FL_ZERO_RANGE", 16}, {"FALLOC_FL_INSERT_RANGE", 32}, {"FALLOC_FL_UNSHARE_RANGE", 64},
    {"UTIME_NOW", 0x3fffffff}, {"UTIME_OMIT", 0x3ffffffe}, {"LOCK_SH", 1}, {"LOCK_EX", 2}, {"LOCK_NB", 4},
    {"LOCK_UN", 8}, {"RWF_HIPRI", 1}, {"RWF_DSYNC", 2}, {"RWF_SYNC", 4}, {"RWF_NOWAIT", 8}, {"RWF_APPEND", 16},
    {0, 0}};

static char base[512];
static long vars[26];

/* Tokens: words, or "quoted" with \0 \n \\ \" escapes (length kept for embedded NULs). */
struct tok { char *s; int len; };
static struct tok toks[16];
static int ntok;

static int tokenize(char *line) {
    ntok = 0;
    char *p = line;
    while (*p && ntok < 16) {
        while (*p == ' ' || *p == '\t') p++;
        if (!*p || *p == '\n' || *p == '#') break;
        if (*p == '"') {
            char *out = ++p, *start = out;
            while (*p && *p != '"') {
                if (*p == '\\' && p[1]) {
                    p++;
                    *out++ = *p == '0' ? 0 : *p == 'n' ? '\n' : *p;
                    p++;
                } else *out++ = *p++;
            }
            if (*p == '"') p++;
            toks[ntok].s = start;
            toks[ntok].len = (int)(out - start);
            *out = 0;
            /* a quoted token is marked by a negative length offset trick: keep a flag */
            toks[ntok].len |= 1 << 30;
            ntok++;
        } else {
            char *start = p;
            while (*p && *p != ' ' && *p != '\t' && *p != '\n') p++;
            if (*p) *p++ = 0;
            toks[ntok].s = start;
            toks[ntok].len = (int)strlen(start);
            ntok++;
        }
    }
    return ntok;
}
static int quoted(int i) { return toks[i].len >> 30; }
static int tlen(int i) { return toks[i].len & ~(1 << 30); }

static long num(const char *s) {
    if (s[0] >= 'A' && s[0] <= 'Z' && s[1] == 0) return vars[s[0] - 'A'];
    long v = 0;
    char buf[256];
    strncpy(buf, s, 255);
    buf[255] = 0;
    for (char *part = strtok(buf, "|"); part; part = strtok(0, "|")) {
        int found = 0;
        for (int i = 0; names[i].n; i++)
            if (!strcmp(names[i].n, part)) { v |= names[i].v; found = 1; }
        if (!found) v |= strtol(part, 0, 0);
    }
    return v;
}

static char pathbuf[3][8192];
/* $ = base, $/x = base/x, %N = N times 'x', ^N = "./" repeated + 'q' to N bytes, "" = empty. */
static const char *path(int i, int slot) {
    char *o = pathbuf[slot];
    const char *s = toks[i].s;
    if (quoted(i)) return s;
    if (s[0] == '$') { snprintf(o, 8192, "%s%s", base, s + 1); return o; }
    if (s[0] == '%') { int n = atoi(s + 1); memset(o, 'x', n); o[n] = 0; return o; }
    if (s[0] == '^') {
        int n = atoi(s + 1), k = (n - 1) / 2, j = 0;
        for (int t = 0; t < k; t++) { o[j++] = '.'; o[j++] = '/'; }
        while (j < n) o[j++] = 'q';
        o[j] = 0;
        return o;
    }
    return s;
}

static const char *errname(long e) {
    for (int i = 0; errs[i].n; i++)
        if (errs[i].v == e) return errs[i].n;
    return 0;
}

static char got[8192];

static void put_long(long r) {
    const char *n = r < 0 ? errname(-r) : 0;
    if (n) snprintf(got, sizeof got, "%s", n);
    else snprintf(got, sizeof got, "%ld", r);
}

static void put_stat_fields(unsigned mode, unsigned long nlink, long size) {
    const char *t = "?";
    switch (mode & 0170000) {
    case 0100000: t = "reg"; break;
    case 0040000: t = "dir"; break;
    case 0120000: t = "lnk"; break;
    case 0020000: t = "chr"; break;
    case 0010000: t = "fifo"; break;
    case 0060000: t = "blk"; break;
    case 0140000: t = "sock"; break;
    case 0: t = "none"; break;
    }
    snprintf(got, sizeof got, "%s:%o:%lu:%ld", t, mode & 07777, nlink, size);
}

static void put_stat(long r, unsigned char *st) {
    if (r < 0) { put_long(r); return; }
    unsigned mode;
    unsigned long nlink;
    long size;
    memcpy(&nlink, st + 16, 8);
    memcpy(&mode, st + 24, 4);
    memcpy(&size, st + 48, 8);
    put_stat_fields(mode, nlink, size);
}

static void put_data(long r, const char *buf) {
    if (r < 0) { put_long(r); return; }
    char *o = got;
    *o++ = '"';
    for (long i = 0; i < r && o < got + sizeof got - 4; i++) {
        if (buf[i] == 0) { *o++ = '\\'; *o++ = '0'; }
        else if (buf[i] == '\n') { *o++ = '\\'; *o++ = 'n'; }
        else *o++ = buf[i];
    }
    *o++ = '"';
    *o = 0;
}

static int cmp_names(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

/* ls PATH: names of a directory without . and .., sorted, joined by ','; "-" when empty. */
static void do_ls(const char *p) {
    long fd = sc(NR_open, (long)p, 0200000, 0, 0, 0);
    if (fd < 0) { put_long(fd); return; }
    static char buf[65536];
    static char *list[4096];
    int n = 0, dots = 0;
    long r;
    while ((r = sc(NR_getdents64, fd, (long)buf, sizeof buf, 0, 0)) > 0) {
        for (long off = 0; off < r;) {
            unsigned short reclen;
            memcpy(&reclen, buf + off + 16, 2);
            char *name = buf + off + 19;
            if (!strcmp(name, ".") || !strcmp(name, "..")) dots++;
            else if (n < 4096) list[n++] = strdup(name);
            off += reclen;
        }
    }
    sc(NR_close, fd, 0, 0, 0, 0);
    if (r < 0) { put_long(r); return; }
    if (dots != 2) { snprintf(got, sizeof got, "dots=%d", dots); return; }
    qsort(list, n, sizeof list[0], cmp_names);
    got[0] = 0;
    if (!n) strcpy(got, "-");
    for (int i = 0; i < n; i++) {
        if (i) strcat(got, ",");
        if (strlen(got) + strlen(list[i]) < sizeof got - 2) strcat(got, list[i]);
        free(list[i]);
    }
}

/* getdents FD SIZE: number of records one call returns. */
static void do_getdents(long fd, long size) {
    static char buf[65536];
    long r = sc(NR_getdents64, fd, (long)buf, size, 0, 0);
    if (r < 0) { put_long(r); return; }
    int n = 0;
    for (long off = 0; off < r;) {
        unsigned short reclen;
        memcpy(&reclen, buf + off + 16, 2);
        off += reclen;
        n++;
    }
    put_long(n);
}

/* fill PATH N: one write of N pattern bytes, one read back, compare: "ok" or what differed. */
static void do_fill(const char *p, long n) {
    char *b = malloc(n), *c = malloc(n);
    for (long i = 0; i < n; i++) b[i] = (char)(i * 7 % 251);
    long fd = sc(NR_open, (long)p, 0102, 0644, 0, 0);
    if (fd < 0) { put_long(fd); return; }
    long w = sc(NR_write, fd, (long)b, n, 0, 0);
    long s = sc(NR_lseek, fd, 0, 0, 0, 0);
    long r = sc(NR_read, fd, (long)c, n, 0, 0);
    sc(NR_close, fd, 0, 0, 0, 0);
    if (w != n || s != 0 || r != n || memcmp(b, c, n)) snprintf(got, sizeof got, "w=%ld s=%ld r=%ld", w, s, r);
    else strcpy(got, "ok");
    free(b);
    free(c);
}

/* ---- FS-2 commands on /dev, /proc and /sys ---- */
static int g_argc;
static char **g_argv;

/* The whole file (proc files have no size: read until end of file). Returns length or -errno. */
static long slurp(const char *p, char *out, long max) {
    long fd = sc(NR_open, (long)p, 0, 0, 0, 0);
    if (fd < 0) return fd;
    long n = 0, r;
    while (n < max - 1 && (r = sc(NR_read, fd, (long)(out + n), max - 1 - n, 0, 0)) > 0) n += r;
    sc(NR_close, fd, 0, 0, 0, 0);
    if (r < 0 && n == 0) return r;
    out[n] = 0;
    return n;
}

/* A word equal to the own pid becomes "pid" (the pid differs between runs and systems). */
static void pid_words(char *dst, const char *src, long size) {
    char me[32];
    snprintf(me, sizeof me, "%ld", (long)getpid());
    dst[0] = 0;
    char tmp[4096];
    snprintf(tmp, sizeof tmp, "%s", src);
    int first = 1;
    for (char *w = strtok(tmp, " \t\n"); w; w = strtok(0, " \t\n")) {
        if (!first) strncat(dst, " ", size - strlen(dst) - 1);
        strncat(dst, strcmp(w, me) ? w : "pid", size - strlen(dst) - 1);
        first = 0;
    }
}

static void do_has(const char *dir, const char *list) {
    long fd = sc(NR_open, (long)dir, 0200000, 0, 0, 0);
    if (fd < 0) { put_long(fd); return; }
    static char buf[65536], names[262144];
    long r, used = 0;
    names[0] = 0;
    while ((r = sc(NR_getdents64, fd, (long)buf, sizeof buf, 0, 0)) > 0) {
        for (long off = 0; off < r;) {
            unsigned short reclen;
            memcpy(&reclen, buf + off + 16, 2);
            long l = (long)strlen(buf + off + 19);
            if (used + l + 2 < (long)sizeof names) { names[used++] = '/'; memcpy(names + used, buf + off + 19, l); used += l; names[used] = 0; }
            off += reclen;
        }
    }
    sc(NR_close, fd, 0, 0, 0, 0);
    if (r < 0) { put_long(r); return; }
    strcat(names, "/");
    char want[1024];
    snprintf(want, sizeof want, "%s", list);
    for (char *n = strtok(want, ","); n; n = strtok(0, ",")) {
        char key[300];
        if (!strcmp(n, "pid")) snprintf(key, sizeof key, "/%ld/", (long)getpid());
        else snprintf(key, sizeof key, "/%s/", n);
        if (!strstr(names, key)) { snprintf(got, sizeof got, "missing:%s", n); return; }
    }
    strcpy(got, "yes");
}

static const char *ftype(unsigned mode) {
    switch (mode & 0170000) {
    case 0100000: return "reg";
    case 0040000: return "dir";
    case 0120000: return "lnk";
    case 0020000: return "chr";
    case 0060000: return "blk";
    case 0010000: return "fifo";
    }
    return "?";
}

/* FS-3 commands: pipes, FIFOs, mknod, eventfd (see cases.txt "--- pipes"). */
static int fs3(const char *c) {
    static char big[1 << 20];
    char *T1 = ntok > 1 ? toks[1].s : "", *T2 = ntok > 2 ? toks[2].s : "", *T3 = ntok > 3 ? toks[3].s : "";
    char *T4 = ntok > 4 ? toks[4].s : "";
    long r;
    int fds[2];
    if (!strcmp(c, "pipe2") || !strcmp(c, "pipe1")) { /* pipe2 X Y FLAGS: stores the read end in X, the write end in Y */
        r = !strcmp(c, "pipe1") ? sc(NR_pipe, (long)fds, 0, 0, 0, 0) : sc(NR_pipe2, (long)fds, num(T3), 0, 0, 0);
        if (r == 0) { vars[T1[0] - 'A'] = fds[0]; vars[T2[0] - 'A'] = fds[1]; }
        put_long(r);
    } else if (!strcmp(c, "pipe2null")) put_long(sc(NR_pipe2, 0, num(T1), 0, 0, 0));
    else if (!strcmp(c, "mknod")) put_long(sc(NR_mknod, (long)path(1, 0), num(T2), num(T3), 0, 0));
    else if (!strcmp(c, "mknodat")) put_long(sc(NR_mknodat, num(T1), (long)path(2, 0), num(T3), num(T4), 0));
    else if (!strcmp(c, "eventfd")) put_long(sc(NR_eventfd2, num(T1), num(T2), 0, 0, 0));
    /* FS-10: memfd NAME FLAGS (the name is a path token: "quoted", %N); memfdnull FLAGS passes a NULL name. */
    else if (!strcmp(c, "memfd")) put_long(sc(NR_memfd_create, (long)path(1, 0), num(T2), 0, 0, 0));
    else if (!strcmp(c, "memfdnull")) put_long(sc(NR_memfd_create, 0, num(T1), 0, 0, 0));
    else if (!strcmp(c, "eventfd1")) put_long(sc(NR_eventfd, num(T1), 0, 0, 0, 0));
    /* FS-10 fixer: fallocate FD MODE OFFSET LEN. */
    else if (!strcmp(c, "fallocate")) put_long(sc(NR_fallocate, num(T1), num(T2), num(T3), num(T4), 0));
    else if (!strcmp(c, "fionread")) { int n = -1; r = sc(NR_ioctl, num(T1), 0x541B, (long)&n, 0, 0); put_long(r < 0 ? r : n); }
    else if (!strcmp(c, "poll")) { /* poll FD EVENTS: revents after a poll without waiting */
        struct pollfd pf = {(int)num(T1), (short)num(T2), 0};
        r = sc(NR_poll, (long)&pf, 1, 0, 0, 0);
        put_long(r < 0 ? r : pf.revents);
    } else if (!strcmp(c, "writen")) { long n = num(T2); memset(big, 'x', n); put_long(sc(NR_write, num(T1), (long)big, n, 0, 0)); }
    else if (!strcmp(c, "readn")) put_long(sc(NR_read, num(T1), (long)big, num(T2), 0, 0));
    else if (!strcmp(c, "fillnb")) { /* fillnb FD CHUNK: CHUNK-byte writes until one is short; "total,last" */
        long n = num(T2), total = 0;
        memset(big, 'y', n);
        for (;;) { r = sc(NR_write, num(T1), (long)big, n, 0, 0); if (r > 0) total += r; if (r != n) break; }
        char last[32];
        snprintf(last, sizeof last, "%s", r < 0 && errname(-r) ? errname(-r) : "");
        if (!last[0]) snprintf(last, sizeof last, "%ld", r);
        snprintf(got, sizeof got, "%ld,%s", total, last);
    } else if (!strcmp(c, "efdw")) { /* efdw FD VALUE LEN: write the 8-byte value with count LEN */
        unsigned long v = (unsigned long)num(T2);
        memcpy(big, &v, 8);
        put_long(sc(NR_write, num(T1), (long)big, num(T3), 0, 0));
    } else if (!strcmp(c, "efdr")) { /* efdr FD LEN: the value read, or the error */
        unsigned long v = 0;
        r = sc(NR_read, num(T1), (long)big, num(T2), 0, 0);
        if (r == 8) { memcpy(&v, big, 8); snprintf(got, sizeof got, "%lu", v); } else put_long(r);
    } else if (!strcmp(c, "efdrv")) { /* efdrv FD L1 L2: readv into two pieces, the value or the error */
        long iov[4] = {(long)big, num(T2), (long)(big + 4096), num(T3)};
        r = sc(NR_readv, num(T1), (long)iov, 2, 0, 0);
        if (r == 8) { unsigned char b8[8]; long a = iov[1] < 8 ? iov[1] : 8; memcpy(b8, big, a); memcpy(b8 + a, big + 4096, 8 - a);
            unsigned long v; memcpy(&v, b8, 8); snprintf(got, sizeof got, "%lu", v); } else put_long(r);
    } else if (!strcmp(c, "efdwv")) { /* efdwv FD L1 L2: writev of the value 1 in two pieces */
        unsigned long one = 1;
        memcpy(big, &one, 8);
        memcpy(big + 4096, &one, 8);
        long iov[4] = {(long)big, num(T2), (long)(big + 4096), num(T3)};
        put_long(sc(NR_writev, num(T1), (long)iov, 2, 0, 0));
    } else if (!strcmp(c, "readfault")) put_long(sc(NR_read, num(T1), 0x10, num(T2), 0, 0));
    else if (!strcmp(c, "writefault")) put_long(sc(NR_write, num(T1), 0x10, num(T2), 0, 0));
    else return 0;
    return 1;
}

static int fs2(const char *c) {
    static char file[262144];
    static unsigned char st[256];
    char *T1 = ntok > 1 ? toks[1].s : "", *T2 = ntok > 2 ? toks[2].s : "";
    long r;
    if (!strcmp(c, "has")) do_has(path(1, 0), T2);
    else if (!strcmp(c, "rdev") || !strcmp(c, "mode")) {
        r = sc(c[0] == 'm' ? NR_lstat : NR_stat, (long)path(1, 0), (long)st, 0, 0, 0);
        if (r < 0) put_long(r);
        else if (c[0] == 'r') {
            unsigned long d;
            memcpy(&d, st + 40, 8);
            snprintf(got, sizeof got, "%lu:%lu", (d >> 8) & 0xfff, (d & 0xff) | ((d >> 12) & ~0xffUL));
        } else {
            unsigned mode;
            memcpy(&mode, st + 24, 4);
            snprintf(got, sizeof got, "%s:%o", ftype(mode), mode & 07777);
        }
    } else if (!strcmp(c, "nonzero")) {
        static char b[65536];
        long n = num(T2);
        memset(b, 0, sizeof b);
        r = sc(NR_read, num(T1), (long)b, n, 0, 0);
        int any = 0;
        for (long i = 0; i < r; i++) any |= b[i];
        if (r >= 0 && !any && r > 8) strcpy(got, "zeros"); else put_long(r);
    } else if (!strcmp(c, "self")) {
        r = sc(NR_readlink, (long)path(1, 0), (long)file, 4096, 0, 0);
        if (r < 0) { put_long(r); return 1; }
        file[r] = 0;
        char me[32];
        snprintf(me, sizeof me, "%ld", (long)getpid());
        if (!strcmp(file, me)) strcpy(got, "pid"); else put_data(r, file);
    } else if (!strcmp(c, "key") || !strcmp(c, "text") || !strcmp(c, "grep") || !strcmp(c, "lines") || !strcmp(c, "fields") || !strcmp(c, "field")) {
        r = slurp(path(1, 0), file, sizeof file);
        if (r < 0) { put_long(r); return 1; }
        if (!strcmp(c, "text")) { strcpy(got, strstr(file, toks[2].s) ? "yes" : "no"); return 1; }
        if (!strcmp(c, "lines")) { long n = 0; for (long i = 0; i < r; i++) n += file[i] == '\n'; put_long(n); return 1; }
        char *nl = strchr(file, '\n');
        if (!strcmp(c, "fields") || !strcmp(c, "field")) {
            if (nl) *nl = 0;
            int want = !strcmp(c, "field") ? (int)num(T2) : 0, k = 0;
            for (char *w = strtok(file, " \t"); w; w = strtok(0, " \t")) {
                k++;
                if (k == want) {
                    char me[32];
                    snprintf(me, sizeof me, "%ld", (long)getpid());
                    snprintf(got, sizeof got, "%s", strcmp(w, me) ? w : "pid");
                    return 1;
                }
            }
            if (want) strcpy(got, "none"); else put_long(k);
            return 1;
        }
        const char *pre = toks[2].s;
        size_t pl = strlen(pre);
        for (char *l = file; l && *l; l = strchr(l, '\n') ? strchr(l, '\n') + 1 : 0) {
            if (!strncmp(l, pre, pl)) {
                if (!strcmp(c, "key")) { strcpy(got, "yes"); return 1; }
                char line[4096];
                char *e = strchr(l, '\n');
                long len = e ? e - l : (long)strlen(l);
                if (len > 4000) len = 4000;
                memcpy(line, l, len);
                line[len] = 0;
                char w[4096];
                pid_words(w, line, sizeof w);
                snprintf(got, sizeof got, "\"%s\"", w);
                return 1;
            }
        }
        strcpy(got, !strcmp(c, "key") ? "no" : "none");
    } else if (!strcmp(c, "cmdline")) {
        r = slurp("/proc/self/cmdline", file, sizeof file);
        if (r < 0) { put_long(r); return 1; }
        long at = 0;
        for (int i = 0; i < g_argc; i++) {
            long l = (long)strlen(g_argv[i]) + 1;
            if (at + l > r || memcmp(file + at, g_argv[i], l)) { put_data(r, file); return 1; }
            at += l;
        }
        if (at == r) strcpy(got, "argv"); else put_data(r, file);
    } else return 0;
    return 1;
}

static int run(char *line) {
    char *arrow = strstr(line, "=>");
    if (!arrow) return -1;
    *arrow = 0;
    char expect[256];
    if (sscanf(arrow + 2, " %255s", expect) != 1) return -1;
    if (expect[0] == '"') { /* quoted expectation may hold spaces: take up to the closing quote */
        char *q = strchr(arrow + 2, '"');
        char *e = q ? strrchr(q + 1, '"') : 0;
        if (q && e) { int l = (int)(e - q + 1); if (l > 255) l = 255; memcpy(expect, q, l); expect[l] = 0; }
    }
    if (tokenize(line) < 1) return -1;
    const char *c = toks[0].s;
    char *T1 = ntok > 1 ? toks[1].s : "", *T2 = ntok > 2 ? toks[2].s : "", *T3 = ntok > 3 ? toks[3].s : "";
    char *T4 = ntok > 4 ? toks[4].s : "", *T5 = ntok > 5 ? toks[5].s : "";
    static char buf[65536];
    static unsigned char st[256];
    long r;
    got[0] = 0;
#define P(i, s) path(i, s)
    if (!strcmp(c, "open")) put_long(sc(NR_open, (long)P(1, 0), num(T2), ntok > 3 ? num(T3) : 0, 0, 0));
    else if (!strcmp(c, "openat")) put_long(sc(NR_openat, num(T1), (long)P(2, 0), num(T3), ntok > 4 ? num(T4) : 0, 0));
    else if (!strcmp(c, "creat")) put_long(sc(NR_creat, (long)P(1, 0), num(T2), 0, 0, 0));
    else if (!strcmp(c, "close")) put_long(sc(NR_close, num(T1), 0, 0, 0, 0));
    else if (!strcmp(c, "read")) { r = sc(NR_read, num(T1), (long)buf, num(T2), 0, 0); put_data(r, buf); }
    else if (!strcmp(c, "pread")) { r = sc(NR_pread64, num(T1), (long)buf, num(T2), num(T3), 0); put_data(r, buf); }
    else if (!strcmp(c, "write")) put_long(sc(NR_write, num(T1), (long)toks[2].s, tlen(2), 0, 0));
    else if (!strcmp(c, "pwrite")) put_long(sc(NR_pwrite64, num(T1), (long)toks[2].s, tlen(2), num(T3), 0));
    else if (!strcmp(c, "readv")) {
        long iov[4] = {(long)buf, num(T2), (long)(buf + 32768), num(T3)};
        r = sc(NR_readv, num(T1), (long)iov, 2, 0, 0);
        if (r >= 0) { long a = r < iov[1] ? r : iov[1]; memmove(buf + a, buf + 32768, r - a); }
        put_data(r, buf);
    } else if (!strcmp(c, "writev")) {
        long iov[4] = {(long)toks[2].s, tlen(2), (long)toks[3].s, tlen(3)};
        put_long(sc(NR_writev, num(T1), (long)iov, 2, 0, 0));
    }
    /* FS-1b: attributes (NULL as a path token passes a NULL pointer), statfs, vectored and copying I/O. */
    else if (!strcmp(c, "chmod")) put_long(sc(NR_chmod, (long)path(1, 0), num(T2), 0, 0, 0));
    else if (!strcmp(c, "fchmod")) put_long(sc(NR_fchmod, num(T1), num(T2), 0, 0, 0));
    else if (!strcmp(c, "fchmodat")) put_long(sc(NR_fchmodat, num(T1), (long)path(2, 0), num(T3), 0, 0));
    else if (!strcmp(c, "fchmodat2")) put_long(sc(NR_fchmodat2, num(T1), (long)path(2, 0), num(T3), num(T4), 0));
    else if (!strcmp(c, "chown")) put_long(sc(NR_chown, (long)path(1, 0), num(T2), num(T3), 0, 0));
    else if (!strcmp(c, "lchown")) put_long(sc(NR_lchown, (long)path(1, 0), num(T2), num(T3), 0, 0));
    else if (!strcmp(c, "fchown")) put_long(sc(NR_fchown, num(T1), num(T2), num(T3), 0, 0));
    else if (!strcmp(c, "fchownat")) put_long(sc(NR_fchownat, num(T1), (long)path(2, 0), num(T3), num(T4), num(toks[5].s)));
    else if (!strcmp(c, "utimens")) { /* utimens DIRFD PATH ASEC ANSEC MSEC MNSEC FLAGS; ASEC "-" = NULL times */
        long ts[4] = {num(T3), num(T4), num(toks[5].s), num(toks[6].s)};
        const char *p = strcmp(T2, "NULL") ? path(2, 0) : 0;
        put_long(sc(NR_utimensat, num(T1), (long)p, strcmp(T3, "-") ? (long)ts : 0, num(toks[7].s), 0));
    } else if (!strcmp(c, "utimes")) { long tv[4] = {num(T2), num(T3), num(T4), ntok > 5 ? num(toks[5].s) : 0};
        put_long(sc(NR_utimes, (long)path(1, 0), (long)tv, 0, 0, 0)); }
    else if (!strcmp(c, "futimesat")) { /* futimesat DIRFD PATH ASEC AUSEC MSEC MUSEC; PATH "NULL", ASEC "-" = NULL */
        long tv[4] = {num(T3), ntok > 4 ? num(T4) : 0, ntok > 5 ? num(toks[5].s) : 0, ntok > 6 ? num(toks[6].s) : 0};
        const char *p = strcmp(T2, "NULL") ? path(2, 0) : 0;
        put_long(sc(NR_futimesat, num(T1), (long)p, strcmp(T3, "-") ? (long)tv : 0, 0, 0)); }
    else if (!strcmp(c, "utime")) { long ub[2] = {num(T2), num(T3)}; put_long(sc(NR_utime, (long)path(1, 0), (long)ub, 0, 0, 0)); }
    else if (!strcmp(c, "times")) { /* times PATH: "atime_sec:mtime_sec" */
        struct stat sb;
        r = sc(NR_lstat, (long)path(1, 0), (long)&sb, 0, 0, 0);
        if (r < 0) put_long(r); else snprintf(got, sizeof got, "%ld:%ld", (long)sb.st_atime, (long)sb.st_mtime);
    } else if (!strcmp(c, "ctimenew")) { /* ctimenew PATH: 1 when ctime >= mtime and >= atime (it moved last) */
        struct stat sb;
        r = sc(NR_lstat, (long)path(1, 0), (long)&sb, 0, 0, 0);
        if (r < 0) put_long(r); else put_long(sb.st_ctime >= sb.st_mtime && sb.st_ctime >= sb.st_atime);
    } else if (!strcmp(c, "statfs") || !strcmp(c, "fstatfs")) { /* "0xTYPE:namelen:rdonly:frsize=bsize" */
        long sf[15] = {0};
        r = !strcmp(c, "statfs") ? sc(NR_statfs, (long)path(1, 0), (long)sf, 0, 0, 0) : sc(NR_fstatfs, num(T1), (long)sf, 0, 0, 0);
        if (r < 0) put_long(r); else snprintf(got, sizeof got, "0x%lx:%ld:%ld:%d", sf[0], sf[8], sf[10] & 1, sf[9] == sf[1]);
    } else if (!strcmp(c, "sync")) put_long(sc(NR_sync, 0, 0, 0, 0, 0));
    else if (!strcmp(c, "syncfs")) put_long(sc(NR_syncfs, num(T1), 0, 0, 0, 0));
    else if (!strcmp(c, "flock")) put_long(sc(NR_flock, num(T1), num(T2), 0, 0, 0));
    else if (!strcmp(c, "fadvise")) put_long(sc(NR_fadvise64, num(T1), num(T2), num(T3), num(T4), 0));
    else if (!strcmp(c, "preadv") || !strcmp(c, "preadv2")) { /* preadv FD L1 L2 POS [FLAGS] */
        long iov[4] = {(long)buf, num(T2), (long)(buf + 32768), num(T3)};
        r = !strcmp(c, "preadv") ? sc(NR_preadv, num(T1), (long)iov, 2, num(T4), 0)
                                 : sc6(NR_preadv2, num(T1), (long)iov, 2, num(T4), 0, num(toks[5].s));
        if (r >= 0) { long a = r < iov[1] ? r : iov[1]; memmove(buf + a, buf + 32768, r - a); }
        put_data(r, buf);
    } else if (!strcmp(c, "pwritev") || !strcmp(c, "pwritev2")) { /* pwritev FD "a" "b" POS [FLAGS] */
        long iov[4] = {(long)toks[2].s, tlen(2), (long)toks[3].s, tlen(3)};
        put_long(!strcmp(c, "pwritev") ? sc(NR_pwritev, num(T1), (long)iov, 2, num(T4), 0)
                                       : sc6(NR_pwritev2, num(T1), (long)iov, 2, num(T4), 0, num(toks[5].s)));
    } else if (!strcmp(c, "sendfile")) { /* sendfile OUT IN OFF COUNT: OFF "-" = NULL; "ret" or "ret,off" */
        long off = strcmp(T3, "-") ? num(T3) : 0;
        r = sc(NR_sendfile, num(T1), num(T2), strcmp(T3, "-") ? (long)&off : 0, num(T4), 0);
        if (r >= 0 && strcmp(T3, "-")) snprintf(got, sizeof got, "%ld,%ld", r, off); else put_long(r);
    } else if (!strcmp(c, "copyrange")) { /* copyrange IN OFFIN OUT OFFOUT LEN FLAGS: "-" = NULL offset */
        long oi = strcmp(T2, "-") ? num(T2) : 0, oo = strcmp(T4, "-") ? num(T4) : 0;
        r = sc6(NR_copy_file_range, num(T1), strcmp(T2, "-") ? (long)&oi : 0, num(T3), strcmp(T4, "-") ? (long)&oo : 0,
                num(toks[5].s), num(toks[6].s));
        put_long(r);
    }
    else if (!strcmp(c, "lseek")) put_long(sc(NR_lseek, num(T1), num(T2), num(T3), 0, 0));
    else if (!strcmp(c, "dup")) put_long(sc(NR_dup, num(T1), 0, 0, 0, 0));
    else if (!strcmp(c, "dup2")) put_long(sc(NR_dup2, num(T1), num(T2), 0, 0, 0));
    else if (!strcmp(c, "dup3")) put_long(sc(NR_dup3, num(T1), num(T2), num(T3), 0, 0));
    else if (!strcmp(c, "fcntl")) {
        r = sc(NR_fcntl, num(T1), num(T2), num(T3), 0, 0);
        if (r >= 0 && num(T2) == 3) snprintf(got, sizeof got, "0%lo", r); else put_long(r);
    } else if (!strcmp(c, "lock")) {
        /* lock FD CMD TYPE WHENCE START LEN [PID]: struct flock; a GETLK answers the l_type it wrote back. */
        struct { short t, w; int pad; long s, l; int pid, pad2; } fl = {
            (short)num(T3), (short)num(T4), 0, num(T5), ntok > 6 ? num(toks[6].s) : 0, ntok > 7 ? (int)num(toks[7].s) : 0, 0};
        r = sc(NR_fcntl, num(T1), num(T2), (long)&fl, 0, 0);
        put_long(r >= 0 && (num(T2) == 5 || num(T2) == 36) ? fl.t : r);
    } else if (!strcmp(c, "fsync")) put_long(sc(NR_fsync, num(T1), 0, 0, 0, 0));
    else if (!strcmp(c, "stat")) { r = sc(NR_stat, (long)P(1, 0), (long)st, 0, 0, 0); put_stat(r, st); }
    else if (!strcmp(c, "lstat")) { r = sc(NR_lstat, (long)P(1, 0), (long)st, 0, 0, 0); put_stat(r, st); }
    else if (!strcmp(c, "fstat")) { r = sc(NR_fstat, num(T1), (long)st, 0, 0, 0); put_stat(r, st); }
    else if (!strcmp(c, "fstatat")) { r = sc(NR_newfstatat, num(T1), (long)P(2, 0), (long)st, num(T3), 0); put_stat(r, st); }
    else if (!strcmp(c, "blocks")) {
        r = sc(NR_stat, (long)P(1, 0), (long)st, 0, 0, 0);
        if (r < 0) put_long(r); else { long b; memcpy(&b, st + 64, 8); put_long(b); }
    } else if (!strcmp(c, "statx") || !strcmp(c, "statxmask")) {
        memset(st, 0, sizeof st);
        r = sc(NR_statx, num(T1), (long)P(2, 0), num(T3), num(T4), (long)st);
        if (r < 0) put_long(r);
        else if (c[5] == 'm') { unsigned m; memcpy(&m, st, 4); snprintf(got, sizeof got, "0x%x", m & 0x1fff); }
        else {
            unsigned nlink; unsigned short mode; unsigned long size;
            memcpy(&nlink, st + 16, 4); memcpy(&mode, st + 28, 2); memcpy(&size, st + 40, 8);
            put_stat_fields(mode, nlink, (long)size);
        }
    } else if (!strcmp(c, "access")) put_long(sc(NR_access, (long)P(1, 0), num(T2), 0, 0, 0));
    else if (!strcmp(c, "faccessat2")) put_long(sc(NR_faccessat2, num(T1), (long)P(2, 0), num(T3), num(T4), 0));
    else if (!strcmp(c, "ls")) do_ls(P(1, 0));
    else if (!strcmp(c, "getdents")) do_getdents(num(T1), num(T2));
    else if (!strcmp(c, "mkdir")) put_long(sc(NR_mkdir, (long)P(1, 0), num(T2), 0, 0, 0));
    else if (!strcmp(c, "mkdirat")) put_long(sc(NR_mkdirat, num(T1), (long)P(2, 0), num(T3), 0, 0));
    else if (!strcmp(c, "unlink")) put_long(sc(NR_unlink, (long)P(1, 0), 0, 0, 0, 0));
    else if (!strcmp(c, "unlinkat")) put_long(sc(NR_unlinkat, num(T1), (long)P(2, 0), num(T3), 0, 0));
    else if (!strcmp(c, "rmdir")) put_long(sc(NR_rmdir, (long)P(1, 0), 0, 0, 0, 0));
    else if (!strcmp(c, "rename")) put_long(sc(NR_rename, (long)P(1, 0), (long)P(2, 1), 0, 0, 0));
    else if (!strcmp(c, "renameat2")) put_long(sc(NR_renameat2, num(T1), (long)P(2, 0), num(T3), (long)P(4, 1), num(T5)));
    else if (!strcmp(c, "link")) put_long(sc(NR_link, (long)P(1, 0), (long)P(2, 1), 0, 0, 0));
    else if (!strcmp(c, "linkat")) put_long(sc(NR_linkat, num(T1), (long)P(2, 0), num(T3), (long)P(4, 1), num(T5)));
    else if (!strcmp(c, "symlink")) put_long(sc(NR_symlink, (long)P(1, 0), (long)P(2, 1), 0, 0, 0));
    else if (!strcmp(c, "symchain")) { /* symchain N: c0 -> f, ci -> c(i-1) */
        long n = num(T1); r = 0;
        for (long i = 0; i < n && r >= 0; i++) {
            char a[32], b[32];
            snprintf(a, 32, "c%ld", i);
            if (i) snprintf(b, 32, "c%ld", i - 1); else strcpy(b, "f");
            r = sc(NR_symlink, (long)b, (long)a, 0, 0, 0);
        }
        put_long(r);
    } else if (!strcmp(c, "readlink")) { r = sc(NR_readlink, (long)P(1, 0), (long)buf, num(T2), 0, 0); put_data(r, buf); }
    else if (!strcmp(c, "readlinkat")) { r = sc(NR_readlinkat, num(T1), (long)P(2, 0), (long)buf, num(T3), 0); put_data(r, buf); }
    else if (!strcmp(c, "getcwd")) {
        r = sc(NR_getcwd, (long)buf, num(T1), 0, 0, 0);
        if (r < 0) put_long(r);
        else if (!strncmp(buf, base, strlen(base))) snprintf(got, sizeof got, "\"$%s\"", buf + strlen(base));
        else snprintf(got, sizeof got, "\"%.4000s\"", buf);
    } else if (!strcmp(c, "chdir")) put_long(sc(NR_chdir, (long)P(1, 0), 0, 0, 0, 0));
    else if (!strcmp(c, "fchdir")) put_long(sc(NR_fchdir, num(T1), 0, 0, 0, 0));
    else if (!strcmp(c, "truncate")) put_long(sc(NR_truncate, (long)P(1, 0), num(T2), 0, 0, 0));
    else if (!strcmp(c, "ftruncate")) put_long(sc(NR_ftruncate, num(T1), num(T2), 0, 0, 0));
    else if (!strcmp(c, "umask")) { r = sc(NR_umask, num(T1), 0, 0, 0, 0); snprintf(got, sizeof got, "0%lo", r); }
    else if (!strcmp(c, "fill")) do_fill(P(1, 0), num(T2));
    else if (!fs3(c) && !fs2(c)) return -1;
    /* fd:X stores; var:X compares with a variable; ok = any non-negative number */
    if (!strncmp(expect, "fd:", 3)) {
        long v = strtol(got, 0, 10);
        if (got[0] < '0' || got[0] > '9') return 0;
        vars[expect[3] - 'A'] = v;
        return 1;
    }
    if (!strncmp(expect, "var:", 4)) { char want[32]; snprintf(want, 32, "%ld", vars[expect[4] - 'A']); return !strcmp(got, want); }
    if (!strcmp(expect, "ok")) return !strcmp(got, "ok") || (got[0] >= '0' && got[0] <= '9');
    return !strcmp(got, expect);
}

int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: fs-cases <cases> [base]\n"); return 2; }
    g_argc = argc;
    g_argv = argv;
    if (argc > 2) snprintf(base, sizeof base, "%s", argv[2]);
    else snprintf(base, sizeof base, "/tmp/fs-cases-%d", (int)getpid());
    FILE *f = fopen(argv[1], "r");
    if (!f) { fprintf(stderr, "fs-cases: cannot open %s\n", argv[1]); return 2; }
    /* The cases assume Linux's default RLIMIT_NOFILE (1024), which BitsOS starts with too. */
    struct rlimit rl = {1024, 1024};
    setrlimit(RLIMIT_NOFILE, &rl);
    /* FS-3: a write to a pipe without readers answers EPIPE here instead of ending the runner. */
    signal(SIGPIPE, SIG_IGN);
    sc(NR_umask, 022, 0, 0, 0, 0);
    long r = sc(NR_mkdir, (long)base, 0755, 0, 0, 0);
    if (r < 0 || sc(NR_chdir, (long)base, 0, 0, 0, 0) < 0) { fprintf(stderr, "fs-cases: base %s: %ld\n", base, r); return 2; }
    char line[1024], copy[1024];
    int lineno = 0, pass = 0, fail = 0;
    while (fgets(line, sizeof line, f)) {
        lineno++;
        char *p = line;
        while (*p == ' ') p++;
        if (!*p || *p == '\n' || *p == '#') continue;
        strcpy(copy, line);
        copy[strcspn(copy, "\n")] = 0;
        int ok = run(line);
        if (ok == 1) pass++;
        else {
            fail++;
            printf("FAIL %d: %s => got %s%s\n", lineno, copy, got, ok < 0 ? " (bad case line)" : "");
        }
    }
    printf("fs-cases: %d passed, %d failed\n", pass, fail);
    return fail ? 1 : 0;
}
