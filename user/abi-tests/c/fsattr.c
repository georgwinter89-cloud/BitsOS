/* FS-1b: statfs per file system, attributes of objects without a name, sendfile/copy_file_range corners and
 * the ctime rules of chmod/chown/utimensat. Runs as a normal user on CachyOS and on V2 (ABI diff).
 * Usage: abi-fsattr statfs | objects | copy | ctime */
#include "abi.h"
#include <fcntl.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/uio.h>

static void fs(const char *path) {
    struct statfs s;
    if (statfs(path, &s) < 0) { printf("statfs %s = -%s\n", path, ename(errno)); return; }
    printf("statfs %s: type 0x%lx namelen %ld valid %d rdonly %d frsize=bsize %d blocks>=free %d\n", path,
           (long)s.f_type, (long)s.f_namelen, !!(s.f_flags & 0x20), (int)(s.f_flags & 1), s.f_frsize == s.f_bsize,
           s.f_blocks >= s.f_bfree && s.f_bfree >= s.f_bavail);
}

static void show_fstatfs(const char *label, int fd) {
    struct statfs s;
    if (fstatfs(fd, &s) < 0) { printf("fstatfs %s = -%s\n", label, ename(errno)); return; }
    printf("fstatfs %s: type 0x%lx namelen %ld bsize %ld\n", label, (long)s.f_type, (long)s.f_namelen, (long)s.f_bsize);
}

static void statfs_case(void) {
    fs("/tmp");
    fs("/proc");
    fs("/sys");
    fs("/dev/pts");
    fs("/dev/shm");
    fs("/nonexistent");
    SYS("statfs efault", SYS_statfs, "/tmp", KERNEL_ADDR);
    SYS("fstatfs ebadf", SYS_fstatfs, 999, KERNEL_ADDR);
    struct statfs t;
    yes("/tmp has a size", statfs("/tmp", &t) == 0 && t.f_blocks > 0);
    /* FS-15 (INT-W7): tmpfs counts inodes and its free blocks follow what a file holds. */
    yes("/tmp has inodes", t.f_files > 0 && t.f_ffree < t.f_files);
    static char mib[1 << 20];
    memset(mib, 'u', sizeof mib);
    int fd = open("/tmp/abi-fsattr-usage", O_CREAT | O_TRUNC | O_WRONLY, 0600);
    struct statfs t2;
    int ok = fd >= 0 && write(fd, mib, sizeof mib) == (ssize_t)sizeof mib && fsync(fd) == 0 && statfs("/tmp", &t2) == 0;
    yes("/tmp usage moves", ok && t2.f_bavail < t.f_bavail && t2.f_ffree < t.f_ffree);
    if (fd >= 0) close(fd);
    unlink("/tmp/abi-fsattr-usage");
}

static void objects_case(void) {
    int p[2], sv[2];
    pipe(p);
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    int e = eventfd(0, 0);
    show_fstatfs("pipe", p[0]);
    show_fstatfs("socket", sv[0]);
    show_fstatfs("eventfd", e);
    /* A pipe keeps its own mode and owner; the anonymous inode is root's (EPERM/EACCES for a user). */
    struct stat st;
    SYS("fchmod pipe", SYS_fchmod, p[0], 0640);
    fstat(p[1], &st);
    printf("pipe mode %o (both ends share the inode)\n", st.st_mode & 07777);
    SYS("fchown pipe -1 -1", SYS_fchown, p[0], -1, -1);
    SYS("fchmod eventfd", SYS_fchmod, e, 0644);
    SYS("fchown eventfd -1 -1", SYS_fchown, e, -1, -1);
    SYS("futimens eventfd now", SYS_utimensat, e, 0, 0, 0);
    SYS("fadvise pipe", SYS_fadvise64, p[0], 0, 0, 0);
    SYS("syncfs pipe", SYS_syncfs, p[0]);
    SYS("sync", SYS_sync);
}

static int tmpfile_named(char *buf, size_t n, const char *tag, int flags) {
    snprintf(buf, n, "/tmp/abi-fsattr-%s-%d", tag, getpid());
    return open(buf, O_CREAT | O_RDWR | flags, 0644);
}

static void copy_case(void) {
    char a[64], b[64], c[64];
    int in = tmpfile_named(a, sizeof a, "in", O_TRUNC);
    int app = tmpfile_named(b, sizeof b, "app", O_APPEND | O_TRUNC);
    write(in, "0123456789", 10);
    int p[2];
    pipe(p);
    off_t off = 0;
    SYS("sendfile to O_APPEND", SYS_sendfile, app, in, &off, 4);
    SYS("sendfile to pipe", SYS_sendfile, p[1], in, &off, 4);
    printf("offset now %ld\n", (long)off);
    SYS("sendfile from pipe", SYS_sendfile, in, p[0], 0, 2);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    off = 6;
    SYS("sendfile to socket", SYS_sendfile, sv[0], in, &off, 100);
    char got[16] = {0};
    printf("socket got %ld \"%s\"\n", (long)read(sv[1], got, sizeof got - 1), got);
    SYS("sendfile efault offset", SYS_sendfile, p[1], in, KERNEL_ADDR, 1);
    /* copy_file_range across two tmpfs instances (/tmp and /dev/shm). */
    snprintf(c, sizeof c, "/dev/shm/abi-fsattr-%d", getpid());
    int shm = open(c, O_CREAT | O_RDWR | O_TRUNC, 0644);
    loff_t oi = 0, oo = 0;
    SYS("copy_file_range across file systems", SYS_copy_file_range, in, &oi, shm, &oo, 4, 0);
    SYS("copy_file_range to O_APPEND", SYS_copy_file_range, in, &oi, app, 0, 4, 0);
    SYS("copy_file_range flags", SYS_copy_file_range, in, &oi, in, &oo, 1, 1);
    SYS("copy_file_range efault", SYS_copy_file_range, in, KERNEL_ADDR, in, &oo, 1, 0);
    /* RWF flags of preadv2: unknown bits, and NOWAIT on an empty pipe. */
    char x[4];
    struct iovec iv = {x, sizeof x};
    printf("pipe drained %ld\n", (long)read(p[0], x, sizeof x));
    SYS("preadv2 nowait empty pipe", SYS_preadv2, p[0], &iv, 1, -1L, 0L, 8L);
    SYS("preadv2 flag 0x80000000", SYS_preadv2, in, &iv, 1, 0, 0, 0x80000000);
    SYS("pwritev2 append", SYS_pwritev2, in, &iv, 1, 0, 0, 0x10);
    struct stat st;
    fstat(in, &st);
    printf("size after append %ld\n", (long)st.st_size);
    unlink(a);
    unlink(b);
    unlink(c);
}

static void ctime_case(void) {
    char a[64];
    int fd = tmpfile_named(a, sizeof a, "ct", O_TRUNC);
    struct timespec ts[2] = {{100, 0}, {200, 0}};
    struct stat s1, s2;
    utimensat(AT_FDCWD, a, ts, 0);
    fstat(fd, &s1);
    printf("times %ld %ld\n", (long)s1.st_atime, (long)s1.st_mtime);
    yes("utimensat moved ctime", s1.st_ctime > 200);
    chmod(a, 0600);
    fstat(fd, &s2);
    yes("chmod kept mtime", s2.st_mtime == 200 && s2.st_atime == 100);
    printf("mode %o\n", s2.st_mode & 07777);
    SYS("chown to root", SYS_chown, a, 0, -1);
    SYS("chown to self", SYS_chown, a, getuid(), getgid());
    SYS("fchmodat2 bad flag", SYS_fchmodat2, AT_FDCWD, a, 0644, 0x8000);
    SYS("utimensat bad nsec", SYS_utimensat, AT_FDCWD, a, (struct timespec[2]){{0, -5}, {0, 0}}, 0);
    SYS("utimensat efault", SYS_utimensat, AT_FDCWD, a, KERNEL_ADDR, 0);
    SYS("utime efault", SYS_utime, a, KERNEL_ADDR);
    unlink(a);
    /* Not the owner (/ belongs to root; this runs as a normal user): explicit times EPERM, "now" EACCES. */
    SYS("utimensat / times", SYS_utimensat, AT_FDCWD, "/", (struct timespec[2]){{1, 0}, {1, 0}}, 0);
    SYS("utimensat / now", SYS_utimensat, AT_FDCWD, "/", 0, 0);
    SYS("utimensat / now+omit", SYS_utimensat, AT_FDCWD, "/", (struct timespec[2]){{0, UTIME_NOW}, {0, UTIME_OMIT}}, 0);
    SYS("chmod /", SYS_chmod, "/", 0755);
    SYS("chown / to self", SYS_chown, "/", getuid(), -1);
    SYS("lchown / group", SYS_lchown, "/", -1, getgid());
}

int main(int argc, char **argv) {
    const char *c = argc > 1 ? argv[1] : "statfs";
    if (!strcmp(c, "statfs")) statfs_case();
    else if (!strcmp(c, "objects")) objects_case();
    else if (!strcmp(c, "copy")) copy_case();
    else if (!strcmp(c, "ctime")) ctime_case();
    return 0;
}
