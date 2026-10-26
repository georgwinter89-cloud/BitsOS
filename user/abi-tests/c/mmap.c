/* fs-mmap: file mappings (FS-5) as a program sees them, for the ABI diff (CachyOS is the oracle) and the
 * ktest fs.map.programs. Prints results, errno names and yes/no relations only (no addresses, pids, devices
 * or inode numbers). Modes (argv[1]):
 *   shm      the Chromium pattern: create in /dev/shm, unlink, ftruncate, mmap twice MAP_SHARED, write
 *            through one and read through the other and through read(2), munmap/close in one order
 *   order2   the same with the other orders of munmap, munmap and close
 *   private  MAP_PRIVATE copies on write, sees the file until then; read(2)/write(2) into and out of mappings
 *   errors   mmap/mprotect/msync errors in Linux's order
 *   maps     how /proc/self/maps shows file mappings (perms, offset, name with "(deleted)")
 *   bus      a page past the end of the file: SIGBUS ends the program
 *   grow     ftruncate after mmap makes the pages reachable (shrinking a mapped file and touching the cut
 *            pages differs from Linux: V2 does not unmap them yet, FS-5 open point)
 * Raw syscalls are not needed: musl's wrappers pass the arguments through. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#define PG 4096

static const char *ename(int e) {
    static char other[24];
    switch (e) {
    case 0: return "0";
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case EBADF: return "EBADF";
    case ENOMEM: return "ENOMEM";
    case EACCES: return "EACCES";
    case EFAULT: return "EFAULT";
    case EEXIST: return "EEXIST";
    case EINVAL: return "EINVAL";
    case ENODEV: return "ENODEV";
    case ENOSPC: return "ENOSPC";
    case EOVERFLOW: return "EOVERFLOW";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case ETXTBSY: return "ETXTBSY";
    }
    snprintf(other, sizeof other, "errno%d", e);
    return other;
}

static void yes(const char *label, int cond) { printf("%s: %s\n", label, cond ? "yes" : "no"); }

/* mmap through the raw syscall, so musl's own checks (it refuses some lengths/offsets itself) stay out. */
static long rmmap(void *addr, unsigned long len, unsigned long prot, unsigned long flags, long fd, unsigned long off) {
    return syscall(SYS_mmap, addr, len, prot, flags, fd, off);
}

static void merr(const char *label, void *addr, unsigned long len, unsigned long prot, unsigned long flags, long fd, unsigned long off) {
    errno = 0;
    long r = rmmap(addr, len, prot, flags, fd, off);
    /* musl's syscall() answers -1 and sets errno. */
    printf("%s: %s\n", label, r == -1 ? ename(errno) : "mapped");
    if (r != -1) munmap((void *)r, len);
}

static void sys(const char *label, long r) {
    printf("%s: %s\n", label, r < 0 ? ename(errno) : "0");
}

static char path_buf[128];
static const char *tmpname(const char *dir, const char *stem) {
    snprintf(path_buf, sizeof path_buf, "%s/fs-mmap-%s-%d", dir, stem, (int)getpid());
    return path_buf;
}

static int make_file(const char *dir, const char *stem, int pages, int unlink_it) {
    const char *p = tmpname(dir, stem);
    int fd = open(p, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (fd < 0) { printf("open %s: %s\n", stem, ename(errno)); exit(2); }
    if (unlink_it) unlink(p);
    if (pages && ftruncate(fd, (off_t)pages * PG) != 0) { printf("ftruncate: %s\n", ename(errno)); exit(2); }
    return fd;
}

static void shm(int order) {
    int fd = make_file("/dev/shm", "shm", 0, 1);
    struct stat st;
    fstat(fd, &st);
    printf("unlinked nlink: %d\n", (int)st.st_nlink);
    sys("ftruncate 3 pages", ftruncate(fd, 3 * PG));
    char *a = mmap(NULL, 3 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    char *b = mmap(NULL, 3 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    yes("both mapped", a != MAP_FAILED && b != MAP_FAILED);
    if (a == MAP_FAILED || b == MAP_FAILED) exit(1);
    yes("two places", a != b);
    yes("fresh pages read zero", a[0] == 0 && b[2 * PG + 17] == 0);
    strcpy(a + PG + 100, "written through a");
    printf("read through b: %s\n", b + PG + 100);
    char buf[32] = {0};
    pread(fd, buf, 17, PG + 100);
    printf("pread sees: %s\n", buf);
    pwrite(fd, "pwrite", 6, 2 * PG);
    printf("a sees pwrite: %.6s\n", a + 2 * PG);
    fstat(fd, &st);
    printf("size: %ld blocks>0: %s\n", (long)st.st_size, st.st_blocks > 0 ? "yes" : "no");
    b[5] = 'X';
    yes("a sees b's store", a[5] == 'X');
    if (order == 0) {
        sys("munmap a", munmap(a, 3 * PG));
        printf("b after munmap a: %s\n", b + PG + 100);
        sys("close", close(fd));
        printf("b after close: %s\n", b + PG + 100);
        b[PG + 100] = 'W';
        printf("b still writable: %c\n", b[PG + 100]);
        sys("munmap b", munmap(b, 3 * PG));
    } else if (order == 1) {
        sys("close", close(fd));
        printf("a after close: %s\n", a + PG + 100);
        sys("munmap b", munmap(b, 3 * PG));
        printf("a after munmap b: %s %c\n", a + PG + 100, a[5]);
        sys("munmap a", munmap(a, 3 * PG));
    } else {
        sys("munmap b", munmap(b, 3 * PG));
        sys("munmap a", munmap(a, 3 * PG));
        memset(buf, 0, sizeof buf);
        pread(fd, buf, 17, PG + 100);
        printf("file after both munmaps: %s\n", buf);
        sys("close", close(fd));
    }
    /* Partial munmap of the middle page leaves two working pieces. */
    fd = make_file("/dev/shm", "shm2", 3, 1);
    char *c = mmap(NULL, 3 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    c[0] = 'L';
    c[2 * PG] = 'R';
    sys("munmap middle", munmap(c + PG, PG));
    memset(buf, 0, sizeof buf);
    pread(fd, buf, 1, 2 * PG);
    printf("pieces: %c %c file: %c\n", c[0], c[2 * PG], buf[0]);
    munmap(c, PG);
    munmap(c + 2 * PG, PG);
    close(fd);
}

static void private_maps(void) {
    int fd = make_file("/tmp", "priv", 0, 1);
    char page[PG];
    memset(page, 'f', PG);
    for (int i = 0; i < 3; i++) write(fd, page, PG);
    char *p = mmap(NULL, 3 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    char *s = mmap(NULL, 3 * PG, PROT_READ, MAP_SHARED, fd, 0);
    yes("mapped", p != MAP_FAILED && s != MAP_FAILED);
    p[10] = 'p';
    char c = 0;
    pread(fd, &c, 1, 10);
    printf("file after private store: %c, shared map: %c, private map: %c\n", c, s[10], p[10]);
    pwrite(fd, "w", 1, PG + 1);
    printf("untouched private page sees pwrite: %c\n", p[PG + 1]);
    pwrite(fd, "v", 1, 11);
    printf("copied private page keeps its copy: %c\n", p[11]);
    /* read(2) into a private mapping copies the page; write(2) from a shared mapping reads the cache. */
    int fd2 = make_file("/tmp", "priv2", 0, 1);
    write(fd2, "into the map", 12);
    lseek(fd2, 0, SEEK_SET);
    long n = read(fd2, p + 2 * PG, 12);
    printf("read into private map: %ld %.12s, file still: %c\n", n, p + 2 * PG, (pread(fd, &c, 1, 2 * PG), c));
    lseek(fd2, 0, SEEK_SET);
    n = write(fd2, s + PG, 2);
    memset(page, 0, 8);
    pread(fd2, page, 2, 0);
    printf("write from shared map: %ld %.2s\n", n, page);
    sys("mprotect private RO", mprotect(p, 3 * PG, PROT_READ));
    printf("content kept: %c %c\n", p[10], p[PG + 1]);
    sys("mprotect private RW", mprotect(p, 3 * PG, PROT_READ | PROT_WRITE));
    p[PG + 2] = 'q';
    pread(fd, &c, 1, PG + 2);
    printf("after RW again: map %c file %c\n", p[PG + 2], c);
    munmap(p, 3 * PG);
    munmap(s, 3 * PG);
    close(fd);
    close(fd2);
}

static void errors(void) {
    const char *f = tmpname("/tmp", "err");
    int rw = open(f, O_CREAT | O_EXCL | O_RDWR, 0600);
    ftruncate(rw, 2 * PG);
    int ro = open(f, O_RDONLY);
    int wo = open(f, O_WRONLY);
    int op = open(f, O_PATH);
    unlink(f);
    int dir = open("/tmp", O_RDONLY | O_DIRECTORY);
    int null = open("/dev/null", O_RDWR);
    merr("shared write on O_RDONLY", NULL, PG, PROT_READ | PROT_WRITE, MAP_SHARED, ro, 0);
    merr("shared read on O_RDONLY", NULL, PG, PROT_READ, MAP_SHARED, ro, 0);
    merr("private write on O_RDONLY", NULL, PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, ro, 0);
    merr("private on O_WRONLY", NULL, PG, PROT_READ, MAP_PRIVATE, wo, 0);
    merr("shared on O_WRONLY", NULL, PG, PROT_WRITE, MAP_SHARED, wo, 0);
    merr("O_PATH fd", NULL, PG, PROT_READ, MAP_SHARED, op, 0);
    merr("closed fd", NULL, PG, PROT_READ, MAP_SHARED, 999, 0);
    merr("fd -1 without MAP_ANONYMOUS", NULL, PG, PROT_READ, MAP_PRIVATE, -1, 0);
    merr("fd 2^32+rw (int truncation)", NULL, PG, PROT_READ, MAP_SHARED, (long)((1ul << 32) | (unsigned)rw), 0);
    merr("directory", NULL, PG, PROT_READ, MAP_SHARED, dir, 0);
    merr("/dev/null", NULL, PG, PROT_READ, MAP_SHARED, null, 0);
    merr("unaligned offset", NULL, PG, PROT_READ, MAP_SHARED, rw, 100);
    merr("unaligned offset, bad fd", NULL, PG, PROT_READ, MAP_SHARED, 999, 100);
    merr("length 0", NULL, 0, PROT_READ, MAP_SHARED, rw, 0);
    merr("length 0, bad fd", NULL, 0, PROT_READ, MAP_SHARED, 999, 0);
    merr("map type 0", NULL, PG, PROT_READ, 0, rw, 0);
    merr("map type 0xf", NULL, PG, PROT_READ, 0xf, rw, 0);
    merr("SHARED_VALIDATE", NULL, PG, PROT_READ, MAP_SHARED_VALIDATE, rw, 0);
    merr("SHARED_VALIDATE|MAP_SYNC", NULL, PG, PROT_READ, MAP_SHARED_VALIDATE | MAP_SYNC, rw, 0);
    merr("SHARED|MAP_SYNC", NULL, PG, PROT_READ, MAP_SHARED | MAP_SYNC, rw, 0);
    merr("MAP_HUGETLB on a file", NULL, PG, PROT_READ, MAP_SHARED | MAP_HUGETLB, rw, 0);
    merr("MAP_GROWSDOWN on a file", NULL, PG, PROT_READ, MAP_PRIVATE | MAP_GROWSDOWN, rw, 0);
    merr("offset past MAX_LFS_FILESIZE", NULL, 2 * PG, PROT_READ, MAP_SHARED, rw, 0x7ffffffffffff000ul);
    merr("offset near the top, overflow", NULL, 2 * PG, PROT_READ, MAP_SHARED, rw, 0xfffffffffffff000ul);
    merr("offset beyond the file (allowed)", NULL, PG, PROT_READ, MAP_SHARED, rw, 100 * PG);
    merr("below mmap_min_addr, MAP_FIXED", (void *)0x1000, PG, PROT_READ, MAP_SHARED | MAP_FIXED, rw, 0);
    merr("type 0 with offset overflow", NULL, 2 * PG, PROT_READ, 0, rw, 0x7ffffffffffff000ul);
    /* mprotect: a read-only fd's shared mapping can never become writable; a private one can. */
    char *m = mmap(NULL, 2 * PG, PROT_READ, MAP_SHARED, ro, 0);
    sys("mprotect RW shared on O_RDONLY", mprotect(m, 2 * PG, PROT_READ | PROT_WRITE));
    sys("mprotect RX shared on O_RDONLY", mprotect(m, 2 * PG, PROT_READ | PROT_EXEC));
    munmap(m, 2 * PG);
    m = mmap(NULL, 2 * PG, PROT_READ, MAP_PRIVATE, ro, 0);
    sys("mprotect RW private on O_RDONLY", mprotect(m, 2 * PG, PROT_READ | PROT_WRITE));
    m[0] = 1;
    printf("private copy written: %d\n", m[0]);
    munmap(m, 2 * PG);
    /* Two neighbours: an anonymous RW one, then the read-only shared one: Linux changes the first, then EACCES. */
    char *two = mmap(NULL, 2 * PG, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    mmap(two + PG, PG, PROT_READ, MAP_SHARED | MAP_FIXED, ro, 0);
    sys("mprotect over anon + RO shared", mprotect(two, 2 * PG, PROT_READ | PROT_WRITE));
    two[0] = 7;
    printf("anonymous part changed: %d\n", two[0]);
    munmap(two, 2 * PG);
    /* msync */
    m = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, rw, 0);
    sys("msync MS_SYNC", msync(m, 2 * PG, MS_SYNC));
    sys("msync MS_ASYNC", msync(m, 2 * PG, MS_ASYNC));
    sys("msync MS_INVALIDATE", msync(m, PG, MS_INVALIDATE));
    sys("msync 0", msync(m, PG, 0));
    sys("msync ASYNC|SYNC", msync(m, PG, MS_ASYNC | MS_SYNC));
    sys("msync bad flag", msync(m, PG, 8));
    sys("msync unaligned", msync(m + 1, PG, MS_SYNC));
    sys("msync len 0", msync(m, 0, MS_SYNC));
    sys("msync unaligned len", msync(m, 10, MS_SYNC));
    munmap(m + PG, PG);
    sys("msync over a hole MS_SYNC", msync(m, 2 * PG, MS_SYNC));
    sys("msync over a hole MS_ASYNC", msync(m, 2 * PG, MS_ASYNC));
    sys("msync hole first MS_SYNC", msync(m + PG, 2 * PG, MS_SYNC));
    sys("msync unmapped only", msync(m + PG, PG, MS_SYNC));
    sys("msync overflow", syscall(SYS_msync, m, (size_t)-4096, MS_SYNC));
    munmap(m, PG);
    close(rw); close(ro); close(wo); close(op); close(dir); close(null);
}

static void maps(void) {
    int fd = make_file("/dev/shm", "maps", 3, 0);
    char *a = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, PG);
    char *b = mmap(NULL, PG, PROT_READ, MAP_PRIVATE, fd, 2 * PG);
    unlink(tmpname("/dev/shm", "maps"));
    char *c = mmap(NULL, PG, PROT_READ, MAP_SHARED, fd, 0);
    (void)a; (void)b; (void)c;
    FILE *m = fopen("/proc/self/maps", "r");
    char line[512], want[64];
    snprintf(want, sizeof want, "fs-mmap-maps-%d", (int)getpid());
    while (m && fgets(line, sizeof line, m)) {
        char *name = strstr(line, want);
        if (!name) continue;
        /* perms and offset (fields 2 and 3), then the name after the pid. */
        char *perms = strchr(line, ' ');
        if (!perms) continue;
        perms++;
        char *off = strchr(perms, ' ');
        if (!off) continue;
        *off++ = 0;
        char *end = strchr(off, ' ');
        if (!end) continue;
        *end = 0;
        printf("map %s %s ...%s", perms, off, name + strlen(want));
    }
    if (m) fclose(m);
    close(fd);
}

static void bus(void) {
    int fd = make_file("/tmp", "bus", 1, 1);
    char *a = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    a[0] = 'x';
    printf("page 0 ok: %c\n", a[0]);
    fflush(stdout);
    a[PG] = 'y'; /* past the end of the file: SIGBUS */
    printf("not reached\n");
}

static void grow(void) {
    int fd = make_file("/tmp", "grow", 1, 1);
    char *a = mmap(NULL, 3 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    sys("ftruncate 3 pages", ftruncate(fd, 3 * PG));
    a[2 * PG] = 'g';
    char c = 0;
    pread(fd, &c, 1, 2 * PG);
    printf("grown page through the map: %c\n", c);
    sys("ftruncate back to 1 page", ftruncate(fd, PG));
    printf("page 0 still: %d, pread past the end: %ld\n", a[0], (long)pread(fd, &c, 1, 2 * PG));
    munmap(a, 3 * PG);
    close(fd);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    const char *mode = argc > 1 ? argv[1] : "shm";
    if (!strcmp(mode, "shm")) shm(0);
    else if (!strcmp(mode, "order2")) { shm(1); shm(2); }
    else if (!strcmp(mode, "private")) private_maps();
    else if (!strcmp(mode, "errors")) errors();
    else if (!strcmp(mode, "maps")) maps();
    else if (!strcmp(mode, "bus")) bus();
    else if (!strcmp(mode, "grow")) grow();
    else { printf("unknown mode %s\n", mode); return 2; }
    printf("done\n");
    return 0;
}
