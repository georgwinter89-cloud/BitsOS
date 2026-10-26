/* abi-rmap (MM-rmap): a hole punch or a truncate of a tmpfs/memfd file takes its pages out of every mapping and
 * frees them (Linux unmap_mapping_range from shmem_fallocate, linux-6.6/mm/shmem.c:3061, even_cows = 0, and from
 * shmem_setattr :1194/:1201, even_cows = 1). Prints results, errno names, st_blocks (512-byte units: the pages the
 * file holds) and the values the mappings read. Modes (argv[1]):
 *   punch     a shared and a private mapping, whole and partial punches, a second process mapping the file
 *   truncate  a truncate below mapped pages (SIGBUS in a child past the end, also for a private copy), growing
 *             again, and the calls seals refuse (F_SEAL_SHRINK, F_SEAL_FUTURE_WRITE) keeping every mapping */
#include "abi.h"
#include <fcntl.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/wait.h>

#define PG 4096L
#define PUNCH 3 /* FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE */

static long blocks(int fd) {
    struct stat st;
    return fstat(fd, &st) ? -1 : (long)st.st_blocks;
}

static const char *r(int x) {
    return x ? ename(errno) : "0";
}

static long val(char *p) {
    return *(volatile long *)p;
}

static void put(char *p, long v) {
    *(volatile long *)p = v;
}

/* A child reads (or writes) *p; prints how it ended. No core dump: the signal is the answer. */
static void touch(const char *label, char *p, int write) {
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        struct rlimit no_core = {0, 0};
        setrlimit(RLIMIT_CORE, &no_core);
        prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
        if (write)
            put(p, 1);
        else
            (void)val(p);
        _exit(0);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    if (WIFSIGNALED(st))
        printf("%s: signal %s\n", label, WTERMSIG(st) == SIGBUS ? "SIGBUS" : "other");
    else
        printf("%s: exit %d\n", label, WEXITSTATUS(st));
}

static char *map(int fd, long pages, int flags) {
    char *p = mmap(NULL, pages * PG, PROT_READ | PROT_WRITE, flags, fd, 0);
    if (p == MAP_FAILED) {
        printf("mmap: %s\n", ename(errno));
        exit(1);
    }
    return p;
}

static int punch(void) {
    int fd = memfd_create("rmap", 0);
    if (fd < 0) {
        printf("memfd_create: %s\n", ename(errno));
        return 1;
    }
    printf("ftruncate 8 pages: %s\n", r(ftruncate(fd, 8 * PG)));
    char *s = map(fd, 8, MAP_SHARED);
    for (long pg = 0; pg < 8; pg++)
        put(s + pg * PG, 0x100 + pg);
    printf("8 pages written: st_blocks %ld\n", blocks(fd));
    char *p = map(fd, 2, MAP_PRIVATE);
    put(p, 0xAA);
    printf("private view of page 1: 0x%lx\n", val(p + PG));

    printf("punch pages 2-4: %s\n", r(fallocate(fd, PUNCH, 2 * PG, 3 * PG)));
    printf("  st_blocks %ld\n", blocks(fd));
    printf("  page 5 0x%lx, page 2 0x%lx, page 4 0x%lx\n", val(s + 5 * PG), val(s + 2 * PG), val(s + 4 * PG));
    printf("  after reading the hole: st_blocks %ld\n", blocks(fd));

    printf("punch the second half of page 6: %s\n", r(fallocate(fd, PUNCH, 6 * PG + PG / 2, PG / 2)));
    printf("  st_blocks %ld, first half 0x%lx, second half 0x%lx\n", blocks(fd), val(s + 6 * PG), val(s + 6 * PG + PG / 2));

    printf("punch pages 0-1 (also privately mapped): %s\n", r(fallocate(fd, PUNCH, 0, 2 * PG)));
    printf("  st_blocks %ld\n", blocks(fd));
    printf("  private copy of page 0 0x%lx, private view of page 1 0x%lx, shared page 0 0x%lx\n", val(p), val(p + PG), val(s));
    put(s + 3 * PG, 0x55);
    char b[8];
    long got = pread(fd, b, sizeof b, 3 * PG) == sizeof b ? *(long *)b : -1;
    printf("  a store into the hole reaches the file: 0x%lx\n", got);

    /* A second process maps the file: the punch frees the page although it maps it too. */
    int up[2], down[2];
    if (pipe(up) || pipe(down)) {
        printf("pipe: %s\n", ename(errno));
        return 1;
    }
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        char *c = map(fd, 8, MAP_SHARED);
        char x = val(c + 7 * PG) == 0x107 ? 'y' : 'n';
        if (write(up[1], &x, 1) != 1 || read(down[0], &x, 1) != 1)
            _exit(4);
        _exit(val(c + 7 * PG) == 0 ? 0 : 3);
    }
    char x = 0;
    if (read(up[0], &x, 1) != 1)
        x = '?';
    printf("the child sees page 7: %c\n", x);
    long before = blocks(fd);
    printf("punch page 7 under two processes: %s\n", r(fallocate(fd, PUNCH, 7 * PG, PG)));
    printf("  st_blocks went down by %ld\n", before - blocks(fd));
    if (write(down[1], &x, 1) != 1)
        printf("pipe write: %s\n", ename(errno));
    int st = 0;
    waitpid(pid, &st, 0);
    printf("  the child read the hole: exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    printf("  parent page 7 0x%lx\n", val(s + 7 * PG));
    close(fd);
    return 0;
}

static int truncate_mode(void) {
    int fd = memfd_create("rmap", 0);
    if (fd < 0) {
        printf("memfd_create: %s\n", ename(errno));
        return 1;
    }
    printf("ftruncate 4 pages: %s\n", r(ftruncate(fd, 4 * PG)));
    char *s = map(fd, 4, MAP_SHARED);
    for (long pg = 0; pg < 4; pg++)
        memset(s + pg * PG, 0xff, PG);
    char *p = map(fd, 4, MAP_PRIVATE);
    put(p + 3 * PG, 0x99);
    printf("4 pages written: st_blocks %ld\n", blocks(fd));
    printf("ftruncate to 1 page + 100 bytes: %s\n", r(ftruncate(fd, PG + 100)));
    printf("  st_blocks %ld\n", blocks(fd));
    touch("  shared page 2", s + 2 * PG, 0);
    touch("  shared page 3 (store)", s + 3 * PG, 1);
    touch("  private copy of page 3", p + 3 * PG, 0);
    printf("  before the end: %s, the new last page's tail: 0x%lx\n", val(s + PG + 88) == -1 ? "kept" : "changed", val(s + PG + 104));
    printf("ftruncate 4 pages again: %s\n", r(ftruncate(fd, 4 * PG)));
    printf("  shared page 3 0x%lx, private page 3 0x%lx, st_blocks %ld\n", val(s + 3 * PG), val(p + 3 * PG), blocks(fd));
    close(fd);

    fd = memfd_create("rmap-sealed", MFD_ALLOW_SEALING);
    if (fd < 0) {
        printf("memfd_create sealing: %s\n", ename(errno));
        return 1;
    }
    printf("sealed: ftruncate 4 pages: %s\n", r(ftruncate(fd, 4 * PG)));
    s = map(fd, 4, MAP_SHARED);
    p = map(fd, 4, MAP_PRIVATE);
    for (long pg = 0; pg < 4; pg++)
        put(s + pg * PG, 0x400 + pg);
    put(p + 3 * PG, 0x99);
    printf("  F_SEAL_SHRINK: %s\n", r(fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK) < 0));
    printf("  ftruncate 1 page: %s\n", r(ftruncate(fd, PG)));
    printf("  st_blocks %ld, page 3 0x%lx, private page 3 0x%lx\n", blocks(fd), val(s + 3 * PG), val(p + 3 * PG));
    printf("  F_SEAL_FUTURE_WRITE: %s\n", r(fcntl(fd, F_ADD_SEALS, F_SEAL_FUTURE_WRITE) < 0));
    printf("  punch pages 0-3: %s\n", r(fallocate(fd, PUNCH, 0, 4 * PG)));
    printf("  st_blocks %ld, page 0 0x%lx, page 3 0x%lx, private page 3 0x%lx\n", blocks(fd), val(s), val(s + 3 * PG), val(p + 3 * PG));
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "punch";
    if (!strcmp(m, "punch"))
        return punch();
    if (!strcmp(m, "truncate"))
        return truncate_mode();
    printf("unknown mode\n");
    return 2;
}
