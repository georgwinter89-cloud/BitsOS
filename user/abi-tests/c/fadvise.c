/* BL-0: posix_fadvise(POSIX_FADV_DONTNEED) and mincore (linux-7/mm/fadvise.c:113, mm/mincore.c), and
 * /proc/sys/vm/drop_caches' permissions (fs/drop_caches.c, mode 0200).
 *   abi-fadvise           a tmpfs file in /tmp: DONTNEED keeps its pages (shmem's no-op backing device); the
 *                         errors (EINVAL, EBADF, ESPIPE); every advice 0..5 accepted; drop_caches for a user.
 *   abi-fadvise dir DIR   a file on DIR's block file system (V2: ext4 /data, CachyOS: any disk file system):
 *                         the whole file goes, a mapped page stays, dirty data reads back intact. Not in
 *                         cases.txt: the diff's V2 has no block file system; host/tests/minipc/blk0.sh --proof
 *                         compares it (V2's /data in QEMU against CachyOS's /var/tmp).
 * Runs as a user (the diff's rule); prints only results and yes/no. */
#include "abi.h"
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>

#define PG 4096

static long fadv(int fd, long off, long len, long advice) {
    return syscall(SYS_fadvise64, fd, off, len, advice);
}

/* Residency of the file's first n pages as '0'/'1' through a fresh read-only mapping (no page touched). */
static void residency(int fd, size_t n, char *out) {
    void *m = mmap(0, n * PG, PROT_READ, MAP_SHARED, fd, 0);
    unsigned char v[64];
    if (m == MAP_FAILED || n > sizeof v || mincore(m, n * PG, v) != 0) {
        snprintf(out, n + 1, "%s", "error");
        if (m != MAP_FAILED)
            munmap(m, n * PG);
        return;
    }
    for (size_t i = 0; i < n; i++)
        out[i] = (v[i] & 1) ? '1' : '0';
    out[n] = 0;
    munmap(m, n * PG);
}

static int count1(const char *s) {
    int c = 0;
    for (; *s; s++)
        c += *s == '1';
    return c;
}

static void fill(char *b, size_t n, int seed) {
    for (size_t i = 0; i < n; i++)
        b[i] = (char)(i * 31 + seed);
}

static int tmpfs_case(void) {
    char path[64], r[16];
    snprintf(path, sizeof path, "/tmp/abi-fadvise-%d", getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    static char buf[8 * PG];
    fill(buf, sizeof buf, 1);
    yes("tmpfs file written", fd >= 0 && write(fd, buf, sizeof buf) == (ssize_t)sizeof buf && fsync(fd) == 0);
    show("DONTNEED whole tmpfs file", fadv(fd, 0, 0, 4));
    residency(fd, 8, r);
    printf("tmpfs residency after DONTNEED = %s\n", r);
    show("negative offset", fadv(fd, -1, 0, 4));
    show("negative length", fadv(fd, 0, -1, 4));
    show("advice 6", fadv(fd, 0, 0, 6));
    show("advice -1", fadv(fd, 0, 0, -1));
    show("bad fd", fadv(9999, 0, 0, 4));
    for (int a = 0; a <= 5; a++) {
        char l[32];
        snprintf(l, sizeof l, "advice %d", a);
        show(l, fadv(fd, 0, 0, a));
    }
    int p[2];
    if (pipe(p) == 0) {
        show("DONTNEED on a pipe", fadv(p[0], 0, 0, 4));
        close(p[0]);
        close(p[1]);
    }
    close(fd);
    unlink(path);
    struct stat st;
    yes("drop_caches is 0200 root", stat("/proc/sys/vm/drop_caches", &st) == 0 && (st.st_mode & 07777) == 0200 && st.st_uid == 0);
    SYS("open drop_caches for reading", SYS_openat, AT_FDCWD, "/proc/sys/vm/drop_caches", O_RDONLY);
    SYS("open drop_caches for writing", SYS_openat, AT_FDCWD, "/proc/sys/vm/drop_caches", O_WRONLY);
    return 0;
}

static int dir_case(const char *dir) {
    char path[512], r[64];
    snprintf(path, sizeof path, "%s/abi-fadvise-%d", dir, getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    const size_t n = 33, size = 32 * PG + 100;
    static char buf[33 * PG], back[33 * PG];
    fill(buf, size, 2);
    yes("file written and synced", fd >= 0 && write(fd, buf, size) == (ssize_t)size && fsync(fd) == 0);
    yes("file read", pread(fd, back, size, 0) == (ssize_t)size && memcmp(buf, back, size) == 0);
    residency(fd, n, r);
    yes("every page cached after the read", count1(r) == (int)n);
    /* Ranges inside the file are left out: Linux 7 keeps a large folio that the range covers only in part
     * (btrfs and ext4 use large folios), V2 has 4 KiB pages; bl0_tests.rs and fs.fadvise.dontneed check them. */
    show("DONTNEED whole file", fadv(fd, 0, 0, 4));
    residency(fd, n, r);
    printf("resident pages after it = %d\n", count1(r));
    volatile char *m = mmap(0, n * PG, PROT_READ, MAP_SHARED, fd, 0);
    if (m != MAP_FAILED) {
        yes("mapped page 5 reads its byte", m[5 * PG] == buf[5 * PG]);
        show("DONTNEED with page 5 mapped", fadv(fd, 0, 0, 4));
        residency(fd, n, r);
        printf("page 5 while mapped = %c\n", r[5]);
        munmap((void *)m, n * PG);
    }
    show("DONTNEED after munmap", fadv(fd, 0, 0, 4));
    residency(fd, n, r);
    printf("resident pages after it = %d\n", count1(r));
    fill(buf + 7 * PG, PG, 9);
    yes("page 7 rewritten (dirty)", pwrite(fd, buf + 7 * PG, PG, 7 * PG) == PG);
    show("DONTNEED with a dirty page", fadv(fd, 0, 0, 4));
    yes("the file reads back with the new page", pread(fd, back, size, 0) == (ssize_t)size && memcmp(buf, back, size) == 0);
    close(fd);
    unlink(path);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    if (argc == 3 && strcmp(argv[1], "dir") == 0)
        return dir_case(argv[2]);
    return tmpfs_case();
}
