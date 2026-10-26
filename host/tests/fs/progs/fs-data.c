/* fs-data: what Brave's profile needs from the data volume (FS-8c; plan/notes/br-BR-1.md: flock on Local State,
 * F_SETLK read/write/unlock, the SingletonLock symlink, rename over, fsync/fdatasync, pwrite/pread, ftruncate,
 * fallocate mode 0, utimensat, fstatfs; SQLite's and LevelDB's shared file mappings with msync, the F2FS feature
 * ioctl SQLite asks on every file, a directory of many names after renames and unlinks), run in a directory of the volume under test. CachyOS is the oracle
 * (host/tests/fs/data-linux.txt, made in a directory of the host's own file system); on V2 host/tests/fs/50-data.sh
 * runs it in /data over MCP. Prints results and errno names only (no paths, ids, inode numbers or times of day).
 *   fs-data DIR         the probes in DIR (made if missing, emptied and removed at the end)
 *   fs-data DIR magic   only the file system type of DIR (statfs f_type): 0xef53 on ext4 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *en(int e) {
    static char b[16];
    switch (e) {
    case 0: return "0";
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case EBADF: return "EBADF";
    case EAGAIN: return "EAGAIN";
    case EACCES: return "EACCES";
    case EEXIST: return "EEXIST";
    case EXDEV: return "EXDEV";
    case ENOTDIR: return "ENOTDIR";
    case EISDIR: return "EISDIR";
    case EINVAL: return "EINVAL";
    case ENOSPC: return "ENOSPC";
    case ELOOP: return "ELOOP";
    case ENOTEMPTY: return "ENOTEMPTY";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case EDEADLK: return "EDEADLK";
    case ENOLCK: return "ENOLCK";
    case ENOTTY: return "ENOTTY";
    }
    snprintf(b, sizeof b, "E%d", e);
    return b;
}

/* The raw call: the self-contained musl V2's image links against has no renameat2(). */
static int rename2(const char *a, const char *b, unsigned flags) {
    return (int)syscall(SYS_renameat2, AT_FDCWD, a, AT_FDCWD, b, flags);
}

static char dir[512];

static const char *at(const char *name) {
    static char p[2][640];
    static int k;
    k ^= 1;
    snprintf(p[k], sizeof p[k], "%s/%s", dir, name);
    return p[k];
}

/* "name: 0" or "name: ENAME" for a call that returns -1 on error. */
static void rc(const char *what, long r) {
    printf("%s: %s\n", what, r < 0 ? en(errno) : "0");
}

static void fill(char *b, size_t n, unsigned seed) {
    for (size_t i = 0; i < n; i++) b[i] = (char)((i * 2654435761u + seed) >> 13);
}

static void data_calls(void) {
    char w[10000], r[20000];
    fill(w, sizeof w, 7);
    int fd = open(at("data"), O_RDWR | O_CREAT | O_EXCL, 0600);
    printf("open excl: %s\n", fd < 0 ? en(errno) : "fd");
    printf("open excl again: %s\n", open(at("data"), O_RDWR | O_CREAT | O_EXCL, 0600) < 0 ? en(errno) : "fd");
    printf("pwrite: %zd\n", pwrite(fd, w, sizeof w, 0));
    ssize_t got = pread(fd, r, sizeof w, 0);
    printf("pread: %zd same=%d\n", got, memcmp(w, r, sizeof w) == 0);
    rc("fdatasync", fdatasync(fd));
    rc("ftruncate 5000", ftruncate(fd, 5000));
    struct stat st;
    fstat(fd, &st);
    printf("size after truncate: %lld\n", (long long)st.st_size);
    rc("fallocate 0..20000", fallocate(fd, 0, 0, 20000));
    fstat(fd, &st);
    memset(r, 1, sizeof r);
    ssize_t n = pread(fd, r, 15000, 5000);
    int zero = 1;
    for (ssize_t i = 0; i < n; i++) zero &= r[i] == 0;
    printf("size after fallocate: %lld tail_read=%zd zero=%d blocks_ge_40=%d\n", (long long)st.st_size, n, zero, st.st_blocks >= 40);
    rc("fallocate keep_size 0..65536", fallocate(fd, FALLOC_FL_KEEP_SIZE, 0, 65536));
    fstat(fd, &st);
    printf("size after keep_size: %lld\n", (long long)st.st_size);
    long long big = (4LL << 30) + 3;
    printf("pwrite past 4 GiB: %zd\n", pwrite(fd, "end", 3, big - 3));
    fstat(fd, &st);
    printf("size big: %lld\n", (long long)st.st_size);
    rc("ftruncate 0", ftruncate(fd, 0));
    rc("fsync", fsync(fd));
    struct timespec ts[2] = {{1000000000, 123456789}, {1100000000, 987654321}};
    rc("futimens", futimens(fd, ts));
    fstat(fd, &st);
    printf("times: atime=%lld.%09ld mtime=%lld.%09ld\n", (long long)st.st_atim.tv_sec, st.st_atim.tv_nsec, (long long)st.st_mtim.tv_sec, st.st_mtim.tv_nsec);
    struct timespec omit[2] = {{0, UTIME_OMIT}, {1200000000, 5}};
    rc("utimensat omit atime", utimensat(AT_FDCWD, at("data"), omit, 0));
    stat(at("data"), &st);
    printf("times: atime=%lld.%09ld mtime=%lld.%09ld\n", (long long)st.st_atim.tv_sec, st.st_atim.tv_nsec, (long long)st.st_mtim.tv_sec, st.st_mtim.tv_nsec);
    rc("link", link(at("data"), at("data-link")));
    fstat(fd, &st);
    printf("nlink: %lu\n", (unsigned long)st.st_nlink);
    rc("unlink both", unlink(at("data")) | unlink(at("data-link")));
    fstat(fd, &st);
    printf("open after unlink: nlink=%lu pwrite=%zd\n", (unsigned long)st.st_nlink, pwrite(fd, "x", 1, 0));
    close(fd);
}

static int put(const char *name, const char *text) {
    int fd = open(at(name), O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) return -1;
    ssize_t n = write(fd, text, strlen(text));
    close(fd);
    return n < 0 ? -1 : 0;
}

static void show(const char *name) {
    char b[64] = {0};
    int fd = open(at(name), O_RDONLY);
    if (fd < 0) {
        printf("read %s: %s\n", name, en(errno));
        return;
    }
    ssize_t n = read(fd, b, sizeof b - 1);
    close(fd);
    printf("read %s: \"%.*s\"\n", name, (int)(n < 0 ? 0 : n), b);
}

static void names(void) {
    put("Local State", "old");
    put("Local State.tmp", "new");
    rc("rename over", rename(at("Local State.tmp"), at("Local State")));
    show("Local State");
    show("Local State.tmp");
    put("other", "o");
    rc("renameat2 noreplace", rename2(at("other"), at("Local State"), RENAME_NOREPLACE));
    rc("renameat2 exchange", rename2(at("other"), at("Local State"), RENAME_EXCHANGE));
    show("Local State");
    show("other");
    rc("mkdir d1", mkdir(at("d1"), 0700));
    rc("mkdir d2", mkdir(at("d2"), 0755));
    rc("mkdir d1 again", mkdir(at("d1"), 0700));
    struct stat st;
    stat(at("d1"), &st);
    printf("d1 mode: %o\n", st.st_mode & 07777);
    put("d2/inside", "i");
    rc("rename dir over non-empty dir", rename(at("d1"), at("d2")));
    rc("rename file over dir", rename(at("other"), at("d1")));
    rc("rename dir over file", rename(at("d1"), at("other")));
    rc("rename dir into itself", rename(at("d2"), at("d2/sub")));
    rc("rmdir non-empty", rmdir(at("d2")));
    rc("unlink d2/inside", unlink(at("d2/inside")));
    rc("rename dir over empty dir", rename(at("d1"), at("d2")));
    rc("stat d1", stat(at("d1"), &st));
    stat(at("d2"), &st);
    printf("d2 mode now: %o\n", st.st_mode & 07777);
    /* The SingletonLock: a symlink whose target is "host-pid", read back with readlink. */
    rc("symlink", symlink("bitsos-1234", at("SingletonLock")));
    rc("symlink again", symlink("bitsos-5678", at("SingletonLock")));
    char t[64] = {0};
    ssize_t n = readlink(at("SingletonLock"), t, sizeof t);
    printf("readlink: %zd \"%.*s\"\n", n, (int)(n < 0 ? 0 : n), t);
    lstat(at("SingletonLock"), &st);
    printf("lstat: link=%d size=%lld\n", S_ISLNK(st.st_mode), (long long)st.st_size);
    printf("stat through dangling: %s\n", stat(at("SingletonLock"), &st) < 0 ? en(errno) : "0");
    printf("open nofollow: %s\n", open(at("SingletonLock"), O_RDONLY | O_NOFOLLOW) < 0 ? en(errno) : "fd");
    char longt[300];
    memset(longt, 'y', sizeof longt - 1);
    longt[sizeof longt - 1] = 0;
    rc("symlink long", symlink(longt, at("long")));
    n = readlink(at("long"), t, sizeof t);
    printf("readlink long into 64: %zd\n", n);
    rc("unlink SingletonLock", unlink(at("SingletonLock")));
    DIR *d = opendir(dir);
    int count = 0;
    struct dirent *e;
    while ((e = readdir(d)) != NULL) count++;
    closedir(d);
    printf("entries: %d\n", count);
}

static void flocks(void) {
    put("lockfile", "l");
    int a = open(at("lockfile"), O_RDWR), b = open(at("lockfile"), O_RDWR);
    rc("flock a ex", flock(a, LOCK_EX));
    rc("flock b ex nb", flock(b, LOCK_EX | LOCK_NB));
    rc("flock b sh nb", flock(b, LOCK_SH | LOCK_NB));
    int c = dup(a);
    rc("flock dup ex nb (same lock)", flock(c, LOCK_EX | LOCK_NB));
    rc("flock a un", flock(a, LOCK_UN));
    rc("flock b sh nb", flock(b, LOCK_SH | LOCK_NB));
    rc("flock a sh nb", flock(a, LOCK_SH | LOCK_NB));
    rc("flock a ex nb (b shares)", flock(a, LOCK_EX | LOCK_NB));
    close(b);
    rc("flock a ex nb (b closed)", flock(a, LOCK_EX | LOCK_NB));
    rc("flock bad op", flock(a, 0));
    close(a);
    close(c);
}

static void getlk(int fd, short type, off_t start, off_t len, pid_t owner, const char *what) {
    struct flock l = {.l_type = type, .l_whence = SEEK_SET, .l_start = start, .l_len = len};
    int r = fcntl(fd, F_GETLK, &l);
    const char *ty = l.l_type == F_UNLCK ? "UNLCK" : l.l_type == F_RDLCK ? "RDLCK" : "WRLCK";
    printf("%s: %s %s start=%lld len=%lld owner=%d\n", what, r < 0 ? en(errno) : "0", ty, (long long)l.l_start, (long long)l.l_len, l.l_type != F_UNLCK && l.l_pid == owner);
}

static int setlk(int fd, short type, off_t start, off_t len) {
    struct flock l = {.l_type = type, .l_whence = SEEK_SET, .l_start = start, .l_len = len};
    return fcntl(fd, F_SETLK, &l);
}

static void posix_locks(void) {
    int fd = open(at("lockfile"), O_RDWR);
    rc("setlk wr 0+100", setlk(fd, F_WRLCK, 0, 100));
    rc("setlk rd 200+10", setlk(fd, F_RDLCK, 200, 10));
    pid_t me = getpid();
    int ready[2], done[2];
    if (pipe(ready) || pipe(done)) return;
    pid_t ch = fork();
    if (ch == 0) {
        int f = open(at("lockfile"), O_RDWR);
        getlk(f, F_RDLCK, 50, 10, me, "child getlk rd 50+10");
        getlk(f, F_RDLCK, 200, 10, me, "child getlk rd 200+10");
        getlk(f, F_WRLCK, 205, 1, me, "child getlk wr 205+1");
        getlk(f, F_WRLCK, 300, 0, me, "child getlk wr 300+eof");
        rc("child setlk rd 0+10", setlk(f, F_RDLCK, 0, 10));
        rc("child setlk rd 200+10", setlk(f, F_RDLCK, 200, 10));
        rc("child setlk wr 500+10", setlk(f, F_WRLCK, 500, 10));
        fflush(stdout);
        if (write(ready[1], "r", 1) != 1) _exit(3);
        char x;
        if (read(done[0], &x, 1) != 1) _exit(3);
        _exit(0);
    }
    char x;
    if (read(ready[0], &x, 1) != 1) return;
    getlk(fd, F_WRLCK, 500, 10, ch, "parent getlk wr 500+10");
    rc("parent setlk wr 505+1", setlk(fd, F_WRLCK, 505, 1));
    rc("parent setlk un 0+0", setlk(fd, F_UNLCK, 0, 0));
    if (write(done[1], "d", 1) != 1) return;
    int st;
    waitpid(ch, &st, 0);
    getlk(fd, F_WRLCK, 0, 0, ch, "parent getlk after child exit");
    rc("setlk bad type", setlk(fd, 7, 0, 1));
    rc("setlk negative start", setlk(fd, F_WRLCK, -5, 1));
    int ro = open(at("lockfile"), O_RDONLY);
    rc("setlk wr on read-only fd", setlk(ro, F_WRLCK, 0, 1));
    close(ro);
    close(fd);
}

static void fs_info(void) {
    struct statfs s;
    int r = statfs(dir, &s);
    printf("statfs: %s bsize_pos=%d blocks_ge_bfree=%d bfree_ge_bavail=%d files_ge_ffree=%d namelen=%ld\n", r < 0 ? en(errno) : "0",
           s.f_bsize > 0, s.f_blocks >= s.f_bfree, s.f_bfree >= s.f_bavail, s.f_files >= s.f_ffree, (long)s.f_namelen);
    int fd = open(dir, O_RDONLY | O_DIRECTORY);
    struct statfs t;
    r = fstatfs(fd, &t);
    printf("fstatfs: %s same_type=%d\n", r < 0 ? en(errno) : "0", t.f_type == s.f_type);
    rc("fsync dir", fsync(fd));
    close(fd);
}

/* SQLite (mmap_size) and LevelDB map their files shared: stores through the mapping reach the file (msync,
 * pread), pwrite reaches the mapping, a grown file maps its new pages as zeros, a private mapping stays private. */
static void maps(void) {
    const long pg = 4096;
    int fd = open(at("map"), O_RDWR | O_CREAT | O_TRUNC, 0600);
    rc("map ftruncate 3 pages", ftruncate(fd, 3 * pg));
    unsigned char *m = mmap(NULL, 3 * pg, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    printf("mmap shared: %s\n", m == MAP_FAILED ? en(errno) : "0");
    if (m == MAP_FAILED) { close(fd); return; }
    char w[4096], r[4096];
    fill(w, sizeof w, 11);
    memcpy(m, w, sizeof w);
    memcpy(m + 2 * pg + 100, "tail of page 2", 14);
    rc("msync", msync(m, 3 * pg, MS_SYNC));
    printf("pread after store: same=%d\n", pread(fd, r, sizeof r, 0) == (ssize_t)sizeof r && memcmp(r, w, sizeof w) == 0);
    printf("pread page 2: %d\n", pread(fd, r, 14, 2 * pg + 100) == 14 && memcmp(r, "tail of page 2", 14) == 0);
    printf("pwrite page 1: %zd\n", pwrite(fd, "seen in the map", 15, pg + 7));
    printf("map sees pwrite: %d\n", memcmp(m + pg + 7, "seen in the map", 15) == 0);
    rc("map ftruncate 5 pages", ftruncate(fd, 5 * pg));
    unsigned char *g = mmap(NULL, 5 * pg, PROT_READ, MAP_SHARED, fd, 0);
    int zeros = 1;
    if (g != MAP_FAILED) for (long i = 3 * pg; i < 5 * pg; i++) zeros &= g[i] == 0;
    printf("mmap grown: %s zeros=%d old=%d\n", g == MAP_FAILED ? en(errno) : "0", zeros, g != MAP_FAILED && memcmp(g, w, sizeof w) == 0);
    if (g != MAP_FAILED) munmap(g, 5 * pg);
    unsigned char *pv = mmap(NULL, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    if (pv != MAP_FAILED) { pv[0] ^= 0xff; munmap(pv, pg); }
    printf("private store stays private: %d\n", pread(fd, r, 1, 0) == 1 && r[0] == w[0]);
    rc("munmap", munmap(m, 3 * pg));
    rc("fsync map", fsync(fd));
    close(fd);
    fd = open(at("map"), O_RDONLY);
    struct stat st;
    fstat(fd, &st);
    printf("reopened: size=%ld same=%d\n", (long)st.st_size, pread(fd, r, sizeof r, 0) == (ssize_t)sizeof r && memcmp(r, w, sizeof w) == 0);
    /* F2FS_IOC_GET_FEATURES, _IOR(0xf5, 12, __u32): SQLite asks on every file; not f2fs, so ENOTTY. */
    unsigned feat = 0;
    printf("ioctl f2fs features: %s\n", ioctl(fd, 0x8004f50cUL, &feat) < 0 ? en(errno) : "0");
    close(fd);
    rc("unlink map", unlink(at("map")));
}

/* A cache directory: many names, a third renamed, a fifth unlinked, read back with getdents. */
static void many(void) {
    const int n = 1500;
    char a[300], b[64];
    rc("mkdir many", mkdir(at("many"), 0700));
    int bad = 0;
    for (int i = 0; i < n; i++) {
        snprintf(a, sizeof a, "many/n%04d", i);
        int fd = open(at(a), O_WRONLY | O_CREAT | O_EXCL, 0600);
        if (fd < 0 || write(fd, a, 10) != 10) bad++;
        if (fd >= 0) close(fd);
    }
    for (int i = 0; i < n; i += 3) {
        snprintf(a, sizeof a, "many/n%04d", i);
        snprintf(b, sizeof b, "many/r%04d", i);
        char pa[640];
        snprintf(pa, sizeof pa, "%s", at(a));
        if (rename(pa, at(b)) < 0) bad++;
    }
    for (int i = 1; i < n; i += 5) {
        snprintf(a, sizeof a, "many/%c%04d", i % 3 == 0 ? 'r' : 'n', i);
        if (unlink(at(a)) < 0) bad++;
    }
    printf("many: made renamed unlinked errors=%d\n", bad);
    DIR *d = opendir(at("many"));
    int names = 0, renamed = 0, dots = 0;
    struct dirent *e;
    while (d && (e = readdir(d))) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) { dots++; continue; }
        names++;
        renamed += e->d_name[0] == 'r';
    }
    if (d) closedir(d);
    printf("many: names=%d renamed=%d dots=%d\n", names, renamed, dots);
    printf("many: rmdir non-empty: %s\n", rmdir(at("many")) < 0 ? en(errno) : "0");
    d = opendir(at("many"));
    while (d && (e = readdir(d))) {
        if (e->d_name[0] == '.') continue;
        snprintf(a, sizeof a, "many/%s", e->d_name);
        unlink(at(a));
    }
    if (d) closedir(d);
    rc("rmdir many", rmdir(at("many")));
}

static void cleanup(void) {
    const char *gone[] = {"Local State", "other", "lockfile", "long", "d2", NULL};
    for (int i = 0; gone[i]; i++) {
        if (unlink(at(gone[i])) < 0) rmdir(at(gone[i]));
    }
    rc("rmdir dir", rmdir(dir));
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: fs-data DIR [magic]\n");
        return 2;
    }
    snprintf(dir, sizeof dir, "%s", argv[1]);
    if (argc > 2 && strcmp(argv[2], "magic") == 0) {
        struct statfs s;
        if (statfs(dir, &s) < 0) {
            printf("statfs: %s\n", en(errno));
            return 1;
        }
        printf("type: 0x%lx\n", (unsigned long)s.f_type);
        return 0;
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (mkdir(dir, 0755) < 0 && errno != EEXIST) {
        printf("mkdir: %s\n", en(errno));
        return 1;
    }
    fs_info();
    data_calls();
    names();
    flocks();
    posix_locks();
    maps();
    many();
    cleanup();
    return 0;
}
