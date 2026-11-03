/* MM-13: transparent huge pages (CachyOS: enabled = always). Private anonymous memory takes 2 MiB pages where a
 * whole aligned 2 MiB lies inside the mapping; smaps AnonHugePages and Rss show them, a cut (munmap, mprotect,
 * madvise DONTNEED, mremap to another offset) splits one, fork shares them and the first write splits the
 * writer's entry, MADV_NOHUGEPAGE and PR_SET_THP_DISABLE keep memory page by page; mmap aligns lengths that are
 * 2 MiB multiples; /sys/kernel/mm/transparent_hugepage and /proc/meminfo AnonHugePages; FD-1's frame pattern
 * (a fresh 13.8 MB buffer per frame). Regions sit between PROT_NONE gaps of one reserve, so no VMA merges with a
 * neighbour; no addresses are printed. */
#include "abi.h"
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/wait.h>

#define P 4096ul
#define M2 (2ul << 20)
#define REGION (4 * M2)

static char buf[1 << 20];
static char *base;

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

/* The sum of smaps field `key` (kB) over the VMAs that start in [lo, hi). */
static long smaps_kb(char *lo, char *hi, const char *key) {
    slurp("/proc/self/smaps");
    long sum = 0;
    char *l = buf;
    unsigned long klen = strlen(key);
    unsigned long s = 0, e = 0;
    int in = 0;
    while (*l) {
        char *nl = strchr(l, '\n');
        if (sscanf(l, "%lx-%lx ", &s, &e) == 2 && strchr(l, '-') < strchr(l, ' '))
            in = s >= (unsigned long)lo && s < (unsigned long)hi;
        else if (in && !strncmp(l, key, klen) && l[klen] == ':')
            sum += atol(l + klen + 1);
        if (!nl)
            break;
        l = nl + 1;
    }
    return sum;
}

static void show_kb(const char *what, char *m, unsigned long len) {
    printf("%s: Rss %ld kB, AnonHugePages %ld kB\n", what, smaps_kb(m, m + len, "Rss"), smaps_kb(m, m + len, "AnonHugePages"));
}

static long meminfo_kb(const char *key) {
    slurp("/proc/meminfo");
    char *l = strstr(buf, key);
    return l ? atol(l + strlen(key) + 1) : -1;
}

static long status_kb(const char *k) {
    slurp("/proc/self/status");
    char *l = strstr(buf, k);
    return l ? atol(l + strlen(k) + 1) : -1;
}

/* Region k of the reserve: REGION bytes, 2 MiB aligned, PROT_NONE around it. */
static char *region(int k) {
    char *m = base + (unsigned long)k * 2 * REGION;
    if (mmap(m, REGION, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) != m) {
        printf("region %d = -%s\n", k, ename(errno));
        exit(1);
    }
    return m;
}

static void release(char *m) {
    mmap(m, REGION, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
}

static void touch(char *m, unsigned long len, char v) {
    for (unsigned long i = 0; i < len; i += P)
        m[i] = v;
}

static void file(const char *path) {
    long n = slurp(path);
    if (n < 0)
        printf("%s = -%s\n", path, ename((int)-n));
    else
        printf("%s = %s", path, buf);
}

static void aligned(void) {
    unsigned long lens[] = {M2, 2 * M2, 16 * M2};
    for (int i = 0; i < 3; i++) {
        char *p = mmap(NULL, lens[i], PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        printf("mmap of %lu MiB without an address: 2 MiB aligned = %d\n", lens[i] >> 20, p != MAP_FAILED && (unsigned long)p % M2 == 0);
        munmap(p, lens[i]);
    }
}

static void splits(void) {
    char *m = region(0);
    m[5 * P] = 1;
    show_kb("one write", m, REGION);
    touch(m, REGION, 2);
    show_kb("all written", m, REGION);
    munmap(m + M2 / 2, P);
    show_kb("munmap of a page in the first 2 MiB", m, REGION);
    yes("contents kept", m[0] == 2 && m[M2 / 2 + P] == 2 && m[REGION - 1] == 0);
    release(m);

    m = region(1);
    touch(m, REGION, 3);
    mprotect(m + M2, 16 * P, PROT_READ);
    show_kb("mprotect of 64 KiB in the second 2 MiB", m, REGION);
    madvise(m + 3 * M2, 16 * P, MADV_DONTNEED);
    show_kb("DONTNEED of 64 KiB in the fourth 2 MiB", m, REGION);
    yes("DONTNEED zeroed only its part", m[3 * M2] == 0 && m[3 * M2 + 16 * P] == 3 && m[M2] == 3);
    release(m);

    m = region(2);
    touch(m, REGION, 4);
    char *to = base + 7 * 2 * REGION;
    char *r = mremap(m, REGION, REGION, MREMAP_MAYMOVE | MREMAP_FIXED, to);
    show_kb("mremap to the same offset in 2 MiB", to, REGION);
    char *odd = base + 9 * 2 * REGION + 4 * P;
    r = mremap(to, REGION, REGION, MREMAP_MAYMOVE | MREMAP_FIXED, odd);
    show_kb("mremap to another offset", odd, REGION);
    yes("mremap kept the contents", r == odd && odd[0] == 4 && odd[REGION - P] == 4);
    munmap(odd, REGION);
    release(to);

    m = region(3);
    madvise(m, REGION, MADV_NOHUGEPAGE);
    m[5 * P] = 1;
    show_kb("MADV_NOHUGEPAGE, one write", m, REGION);
    release(m);
    m = region(3);
    madvise(m, REGION, MADV_HUGEPAGE);
    m[5 * P] = 1;
    show_kb("MADV_HUGEPAGE, one write", m, REGION);
    release(m);
}

static void forked(void) {
    char *m = region(4);
    touch(m, REGION, 5);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        show_kb("child after fork", m, REGION);
        m[3 * P] = 6;
        show_kb("child after its write", m, REGION);
        yes("child sees the parent's data", m[M2] == 5 && m[3 * P] == 6);
        fflush(stdout);
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    show_kb("parent after the child", m, REGION);
    m[3 * P] = 7;
    show_kb("parent after its write", m, REGION);
    yes("parent's data kept", m[3 * P] == 7 && m[4 * P] == 5);
    release(m);
}

static void system_wide(void) {
    file("/sys/kernel/mm/transparent_hugepage/enabled");
    file("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size");
    long before = meminfo_kb("AnonHugePages");
    char *big = mmap(NULL, 32 * M2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    touch(big, 32 * M2, 1);
    long after = meminfo_kb("AnonHugePages");
    yes("/proc/meminfo AnonHugePages grows by the 64 MiB written (half as slack for other programs)", after - before >= 32 * 1024);
    munmap(big, 32 * M2);
}

static void disabled(void) {
    printf("THP_enabled before = %ld\n", status_kb("THP_enabled"));
    SYS("PR_SET_THP_DISABLE", SYS_prctl, PR_SET_THP_DISABLE, 1, 0, 0, 0);
    SYS("PR_GET_THP_DISABLE", SYS_prctl, PR_GET_THP_DISABLE, 0, 0, 0, 0);
    printf("THP_enabled after = %ld\n", status_kb("THP_enabled"));
    char *m = region(5);
    m[5 * P] = 1;
    show_kb("PR_SET_THP_DISABLE, one write", m, REGION);
    release(m);
}

/* FD-1: Chromium's software compositor writes a fresh 13.8 MB buffer per frame. */
static void frames(void) {
    const unsigned long len = 13800000;
    unsigned long sum = 0;
    for (int f = 0; f < 30; f++) {
        unsigned char *b = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        for (unsigned long i = 0; i < len; i += 64)
            b[i] = (unsigned char)(f + i / 64);
        for (unsigned long i = 0; i < len; i += 4096 + 64)
            sum += b[i] + b[i + 1];
        munmap(b, len);
    }
    printf("30 fresh 13.8 MB frames: checksum %lu\n", sum);
}

int main(void) {
    char *raw = mmap(NULL, 22 * REGION + M2, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    if (raw == MAP_FAILED) {
        printf("reserve = -%s\n", ename(errno));
        return 1;
    }
    base = (char *)(((unsigned long)raw + M2 - 1) & ~(M2 - 1));
    aligned();
    splits();
    forked();
    system_wide();
    frames();
    disabled();
    return 0;
}
