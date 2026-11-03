/* PF-4: read and pread of a cached file into a buffer whose later part faults (a PROT_NONE page, a read-only
 * page, an unmapped page). Linux copies page by page and stops at the first user page that cannot be written:
 * the call returns the bytes before it (EFAULT when there are none) and read(2) moves the position by exactly
 * those (linux-6.6/mm/shmem.c shmem_file_read_iter, mm/filemap.c:2703-2709 filemap_read). Every byte that
 * reached the buffer is compared with the file. readv/preadv the same over several segments: the copy goes
 * segment by segment and stops at the first faulting byte, a later writable segment stays untouched.
 * Argument: the directory for the file (default /tmp). */
#include "abi.h"
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/uio.h>

#define PG 4096L
#define FILE_LEN (4 * PG + 100)
#define BUF_PAGES 5

static unsigned char pat(long o) {
    return (unsigned char)(o % 251 + 1);
}

static char *buf;
static const char *const kinds[] = {"none", "readonly", "unmapped"};

/* Buffer pages [0, bad) read-write, page `bad` made `kind` (0: PROT_NONE, 1: read-only, 2: unmapped), the
 * pages after it read-write again. */
static void arrange(int bad, int kind) {
    munmap(buf, BUF_PAGES * PG);
    char *b = mmap(buf, BUF_PAGES * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (b != buf) {
        printf("mmap: %s\n", ename(errno));
        exit(1);
    }
    memset(buf, 0xEE, BUF_PAGES * PG);
    if (bad >= BUF_PAGES)
        return;
    char *p = buf + bad * PG;
    int r = kind == 0 ? mprotect(p, PG, PROT_NONE) : kind == 1 ? mprotect(p, PG, PROT_READ) : munmap(p, PG);
    if (r) {
        printf("arrange: %s\n", ename(errno));
        exit(1);
    }
}

/* Bytes [0, n) at buf + mis equal the file from `off`; the writable bytes after them are untouched. */
static int bytes_ok(long mis, long n, long off, int bad) {
    for (long i = 0; i < n; i++)
        if ((unsigned char)buf[mis + i] != pat(off + i))
            return 0;
    for (long i = mis + n; i < bad * PG; i++)
        if ((unsigned char)buf[i] != 0xEE)
            return 0;
    return 1;
}


/* readv/preadv layouts: segment starts and lengths (buffer offsets; `mis` is added to the first start). */
struct layout {
    long st[3], len[3];
    int n;
};
static const struct layout layouts[] = {
    {{0, PG + 3, 0}, {PG + 3, 3 * PG}, 2},          /* one span split in two */
    {{0, 2 * PG + 7, 0}, {100, 2 * PG}, 2},         /* a gap between them */
    {{3 * PG, 0, 0}, {PG + 50, 2 * PG}, 2},         /* the later pages first, then page 0 */
    {{0, 4 * PG + 1, PG}, {10, 1000, 2 * PG}, 3},   /* three, the last in front of the second */
};

/* What Linux copies (filemap_read via copy_folio_to_iter): segment by segment from file offset `off` until
 * the first byte in the bad page or the file's end; `want` gets that count, `shadow` the buffer it leaves. */
static long vec_expect(const struct layout *l, long mis, long off, int bad, unsigned char *shadow) {
    memset(shadow, 0xEE, BUF_PAGES * PG);
    long got = 0, left = FILE_LEN - off;
    for (int i = 0; i < l->n && left > 0; i++) {
        long st = l->st[i] + (i == 0 ? mis : 0), n = l->len[i] < left ? l->len[i] : left, k = 0;
        for (; k < n && (st + k) / PG != bad; k++)
            shadow[st + k] = pat(off + got + k);
        got += k;
        left -= k;
        if (k < n)
            break;
    }
    return got;
}

static int vec_cases(int fd, int pos_call) {
    static unsigned char shadow[BUF_PAGES * PG];
    static const long voffs[] = {0, 1, 5000};
    static const long vmiss[] = {0, 1, 2048};
    int all = 1, cases = 0;
    for (int kind = 0; kind < 3; kind++)
        for (int bad = 0; bad <= BUF_PAGES; bad++)
            for (unsigned li = 0; li < sizeof layouts / sizeof *layouts; li++)
                for (unsigned o = 0; o < sizeof voffs / sizeof *voffs; o++)
                    for (unsigned m = 0; m < sizeof vmiss / sizeof *vmiss; m++) {
                        const struct layout *l = &layouts[li];
                        long off = voffs[o], mis = vmiss[m];
                        struct iovec iov[3];
                        for (int i = 0; i < l->n; i++)
                            iov[i] = (struct iovec){buf + l->st[i] + (i == 0 ? mis : 0), (size_t)l->len[i]};
                        arrange(bad, kind);
                        long r;
                        if (pos_call)
                            r = preadv(fd, iov, l->n, off);
                        else {
                            lseek(fd, off, SEEK_SET);
                            r = readv(fd, iov, l->n);
                        }
                        int e = r < 0 ? errno : 0;
                        long pos = pos_call ? 0 : (long)lseek(fd, 0, SEEK_CUR);
                        long want = vec_expect(l, mis, off, bad, shadow);
                        int good = want == 0 ? (r == -1 && e == EFAULT) : r == want;
                        if (!pos_call && good)
                            good = pos == off + (r > 0 ? r : 0);
                        for (long i = 0; good && i < BUF_PAGES * PG; i++)
                            if (i / PG != bad && (unsigned char)buf[i] != shadow[i])
                                good = 0;
                        if (!good) {
                            printf("%s %s bad=%d layout=%u off=%ld mis=%ld: %ld (%s), want %ld\n",
                                   pos_call ? "preadv" : "readv", kinds[kind], bad, li, off, mis, r,
                                   e ? ename(e) : "-", want);
                            all = 0;
                        }
                        cases++;
                    }
    printf("%s: %d cases, every byte as the file: %s\n", pos_call ? "preadv" : "readv", cases, all ? "yes" : "no");
    return all;
}

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "/tmp";
    char path[256];
    snprintf(path, sizeof path, "%s/pffault-%d", dir, getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) {
        printf("open %s: %s\n", path, ename(errno));
        return 1;
    }
    unlink(path);
    static unsigned char data[FILE_LEN];
    for (long i = 0; i < FILE_LEN; i++)
        data[i] = pat(i);
    if (write(fd, data, FILE_LEN) != FILE_LEN) {
        printf("write: %s\n", ename(errno));
        return 1;
    }
    buf = mmap(NULL, BUF_PAGES * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (buf == MAP_FAILED) {
        printf("mmap: %s\n", ename(errno));
        return 1;
    }
    static const long offs[] = {0, 1, 100, 4095, 4096, 5000, 4 * PG};
    static const long miss[] = {0, 1, 2048, 4095};
    int all = 1, cases = 0;
    for (int kind = 0; kind < 3; kind++)
        for (int bad = 0; bad <= 4; bad++)
            for (unsigned o = 0; o < sizeof offs / sizeof *offs; o++)
                for (unsigned m = 0; m < sizeof miss / sizeof *miss; m++) {
                    long off = offs[o], mis = miss[m];
                    arrange(bad, kind);
                    long r = pread(fd, buf + mis, 4 * PG, off);
                    int e = r < 0 ? errno : 0;
                    long want = FILE_LEN - off < 4 * PG ? FILE_LEN - off : 4 * PG;
                    if (bad * PG - mis < want)
                        want = bad * PG - mis;
                    int good = want <= 0 ? (r == -1 && e == EFAULT) : (r == want && bytes_ok(mis, r, off, bad));
                    /* One line per buffer layout, the offsets and misalignments summed up in it. */
                    if (!good) {
                        printf("pread %s bad=%d off=%ld mis=%ld: %ld (%s), want %ld\n", kinds[kind], bad, off, mis, r,
                               e ? ename(e) : "-", want);
                        all = 0;
                    }
                    cases++;
                }
    printf("pread: %d cases, every byte as the file: %s\n", cases, all ? "yes" : "no");

    /* read(2): the position moves by what reached the buffer; the next read goes on there. */
    arrange(2, 0);
    lseek(fd, 0, SEEK_SET);
    long r = read(fd, buf + 10, 4 * PG);
    printf("read into 2 good pages from +10: %ld, position %ld, bytes %s\n", r, (long)lseek(fd, 0, SEEK_CUR),
           r > 0 && bytes_ok(10, r, 0, 2) ? "ok" : "differ");
    arrange(BUF_PAGES, 0);
    long r2 = read(fd, buf, 5 * PG);
    printf("next read: %ld, bytes %s\n", r2, r2 > 0 && bytes_ok(0, r2, r, BUF_PAGES) ? "ok" : "differ");
    arrange(0, 0);
    lseek(fd, 0, SEEK_SET);
    r = read(fd, buf, 16);
    printf("read into a PROT_NONE page: %ld %s, position %ld\n", r, r < 0 ? ename(errno) : "", (long)lseek(fd, 0, SEEK_CUR));

    vec_cases(fd, 0);
    vec_cases(fd, 1);

    /* A hole (sparse part of the file) faults the same way. */
    if (ftruncate(fd, 8 * PG) == 0) {
        arrange(1, 0);
        r = pread(fd, buf + 1, 3 * PG, 5 * PG);
        int zero = 1;
        for (long i = 0; i < r; i++)
            zero &= buf[1 + i] == 0;
        printf("pread of a hole into 1 good page from +1: %ld, zeros %s\n", r, zero ? "yes" : "no");
    }
    close(fd);
    return 0;
}
