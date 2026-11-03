/* MM-12: resident memory as a program writes fresh anonymous memory. V2 fills the 64 KiB block of a write fault
 * (fault-around), CachyOS a 4 KiB page (or 2 MiB with transparent huge pages, impossible in the 64-page regions
 * here): a first write is printed as a range, page-exact memory (MADV_RANDOM, MADV_NOHUGEPAGE, MAP_STACK) and
 * fully written memory exactly. smaps Rss is a page walk on both; statm and status count in per-CPU batches on
 * Linux, so only relations with that slack are printed for them. No addresses are printed. */
#include "abi.h"
#include <fcntl.h>
#include <sys/mman.h>

#define P 4096ul
#define PAGES 64

static char buf[1 << 17];

static long slurp(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        return -errno;
    long n = 0, r;
    while (n < (long)sizeof buf - 1 && (r = read(fd, buf + n, sizeof buf - 1 - n)) > 0)
        n += r;
    close(fd);
    buf[n] = 0;
    return n;
}

/* Rss in kB of the smaps entry starting at `at`, or -1. */
static long rss_kb(char *at) {
    slurp("/proc/self/smaps");
    char head[32];
    snprintf(head, sizeof head, "%lx-", (unsigned long)at);
    char *e = strstr(buf, head);
    if (!e || (e != buf && e[-1] != '\n'))
        return -1;
    char *r = strstr(e, "\nRss:");
    return r ? atol(r + 5) : -1;
}

static long statm_resident(void) {
    long sz = 0, res = -1;
    slurp("/proc/self/statm");
    sscanf(buf, "%ld %ld", &sz, &res);
    return res;
}

static long status_kb(const char *k) {
    slurp("/proc/self/status");
    char *l = strstr(buf, k);
    return l ? atol(l + strlen(k) + 1) : -1;
}

/* PAGES writable pages between two PROT_NONE guard pages (no neighbour merges, nothing lands next to them). */
static char *region(int flags, int advice) {
    char *g = mmap(NULL, (PAGES + 2) * P, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | flags, -1, 0);
    if (g == MAP_FAILED) {
        printf("mmap = -%s\n", ename(errno));
        exit(1);
    }
    char *m = g + P;
    if (mprotect(m, PAGES * P, PROT_READ | PROT_WRITE))
        printf("mprotect = -%s\n", ename(errno));
    if (advice >= 0 && madvise(m, PAGES * P, advice))
        printf("madvise = -%s\n", ename(errno));
    return m;
}

static void exact(const char *what, int flags, int advice) {
    char *m = region(flags, advice);
    m[5 * P] = 1;
    printf("%s: Rss after one write = %ld kB\n", what, rss_kb(m));
    m[40 * P] = 1;
    printf("%s: Rss after two writes = %ld kB\n", what, rss_kb(m));
    munmap(m - P, (PAGES + 2) * P);
}

/* mincore after one store: a 4-page VMA (cut blocks stay page by page) is exact, the aligned 64 KiB block
 * inside a 64-page VMA a range (Linux: the page, V2: the block). The FD-1 clamping broke the exact case. */
static void cores(void) {
    unsigned char v[16];
    char *g = mmap(NULL, 6 * P, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (g == MAP_FAILED || mprotect(g + P, 4 * P, PROT_READ | PROT_WRITE))
        printf("small mapping = -%s\n", ename(errno));
    g[P + 100] = 1;
    if (mincore(g + P, 4 * P, v))
        printf("mincore small = -%s\n", ename(errno));
    printf("mincore 4 pages after one store = [%d,%d,%d,%d]\n", v[0] & 1, v[1] & 1, v[2] & 1, v[3] & 1);
    munmap(g, 6 * P);
    char *m = region(0, -1);
    char *a = (char *)(((unsigned long)m + 16 * P - 1) & ~(16 * P - 1));
    a[3 * P] = 1;
    if (mincore(a, 16 * P, v))
        printf("mincore block = -%s\n", ename(errno));
    int n = 0;
    for (int i = 0; i < 16; i++)
        n += v[i] & 1;
    yes("mincore of an aligned block after one store: its page, at most the block", (v[3] & 1) && n >= 1 && n <= 16);
    yes("mincore: no page before the block", mincore(a - P, P, v) == 0 && (v[0] & 1) == 0);
    munmap(m - P, (PAGES + 2) * P);
}

int main(void) {
    cores();
    char *m = region(0, -1);
    printf("untouched Rss = %ld kB\n", rss_kb(m));
    /* INT-W19: both writes in one aligned 64 KiB block inside the region, so the answer does not depend on where
     * mmap placed the region (two pages of an unaligned region may straddle two fault-around blocks). */
    char *b = (char *)(((unsigned long)m + 16 * P - 1) & ~(16 * P - 1));
    b[5 * P] = 1;
    long one = rss_kb(m);
    yes("one write: 4 to 64 kB resident", one >= 4 && one <= 64);
    b[6 * P] = 1;
    yes("a write next to it adds at most a page", rss_kb(m) - one <= 4);
    for (unsigned long i = 0; i < PAGES; i++)
        m[i * P] = 2;
    printf("all written Rss = %ld kB\n", rss_kb(m));
    yes("contents kept", m[5 * P] == 2 && m[63 * P + 100] == 0);
    munmap(m - P, (PAGES + 2) * P);
    printf("unmapped Rss = %ld\n", rss_kb(m));

    exact("MADV_RANDOM", 0, MADV_RANDOM);
    exact("MADV_NOHUGEPAGE", 0, MADV_NOHUGEPAGE);
    exact("MAP_STACK", MAP_STACK, -1);

    /* statm: 32 MiB written page by page grows resident by 8192 pages; Linux's per-CPU batches (up to 64 pages
     * per CPU on a big machine) are the slack. */
    const unsigned long n = 8192, slack = 2048;
    char *big = mmap(NULL, n * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    long before = statm_resident();
    for (unsigned long i = 0; i < n; i++)
        big[i * P] = 3;
    long after = statm_resident();
    yes("statm resident grows by the 32 MiB written", after - before + (long)slack >= (long)n && after - before <= (long)(n + slack));
    yes("statm resident = VmRSS", after * 4 == status_kb("VmRSS"));
    yes("VmHWM >= VmRSS", status_kb("VmHWM") >= status_kb("VmRSS"));
    munmap(big, n * P);
    long gone = statm_resident();
    yes("munmap gives the 32 MiB back", after - gone + (long)slack >= (long)n);
    return 0;
}
