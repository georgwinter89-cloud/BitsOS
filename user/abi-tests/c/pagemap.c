/* L7-RESTE: /proc/<pid>/pagemap (linux-7.3.1/fs/proc/task_mmu.c pagemap_read :2304) and the "gu" VmFlags of smaps
 * (:1194) for guard regions (MADV_GUARD_INSTALL 102 / REMOVE 103). An entry is printed as its flags: present (63),
 * swap (62), file (61), guard (58), exclusive (56); soft-dirty (55) is left out (CachyOS tracks it, V2 has no
 * soft-dirty, a kernel without CONFIG_MEM_SOFT_DIRTY) and the frame is only said to be zero (no CAP_SYS_ADMIN).
 * Pages are touched by writing: a read touch of anonymous memory is Linux's shared zero page, V2 has none.
 * Modes: (none) everything that is the same; sticky: "gu" after the last guard went (V2 drops it); exec: a
 * descriptor opened on a child's pagemap reads 0 bytes once the child ran execve (the open binds the mm,
 * proc_mem_open), a new open reads the new image. */
#include "abi.h"
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096L
#define GUARD_INSTALL 102
#define GUARD_REMOVE 103

static int pm;

static void entry(const char *label, unsigned long long e) {
    printf("%s =%s%s%s%s%s%s frame0 %s\n", label, e >> 63 & 1 ? " present" : "", e >> 62 & 1 ? " swap" : "",
           e >> 61 & 1 ? " file" : "", e >> 58 & 1 ? " guard" : "", e >> 56 & 1 ? " exclusive" : "",
           (e & ~(0x1ffull << 55)) ? " other" : "", (e & ((1ull << 55) - 1)) == 0 ? "yes" : "no");
}

/* The entries of n pages from p. */
static void pages(const char *label, char *p, int n) {
    unsigned long long e[16];
    long r = pread(pm, e, n * 8, (unsigned long)p / PG * 8);
    if (r != n * 8) {
        printf("%s: read %ld %s\n", label, r, r < 0 ? ename(errno) : "");
        return;
    }
    char l[64];
    for (int i = 0; i < n; i++) {
        snprintf(l, sizeof l, "%s %d", label, i);
        entry(l, e[i]);
    }
}

/* Whether smaps shows "gu" for the VMA holding p (it may have merged with a neighbour). */
static void gu(const char *label, char *p) {
    FILE *f = fopen("/proc/self/smaps", "r");
    char line[512];
    unsigned long lo, hi, a = (unsigned long)p;
    int in = 0, n;
    while (fgets(line, sizeof line, f)) {
        if (sscanf(line, "%lx-%lx %n", &lo, &hi, &n) == 2 && n > 0)
            in = lo <= a && a < hi;
        else if (in && !strncmp(line, "VmFlags:", 8)) {
            printf("%s gu = %s\n", label, strstr(line, " gu") ? "yes" : "no");
            break;
        }
    }
    fclose(f);
}

static void sticky(char *m) {
    show("install 1", madvise(m + PG, PG, GUARD_INSTALL) ? -1 : 0);
    gu("with a guard", m);
    show("remove 1", madvise(m + PG, PG, GUARD_REMOVE) ? -1 : 0);
    pages("removed", m + PG, 1);
    gu("after the remove", m);
}

/* The child: says it is there, waits for the go, then runs this file again as "execd". */
static void exec_child(int up, int down) {
    char c, a[16], b[16];
    write(up, "a", 1);
    read(down, &c, 1);
    snprintf(a, sizeof a, "%d", up);
    snprintf(b, sizeof b, "%d", down);
    char *av[] = {"abi-pagemap", "execd", a, b, 0};
    execv("/proc/self/exe", av);
    _exit(1);
}

static void exec_case(void) {
    int up[2], down[2];
    char c;
    pipe(up);
    pipe(down);
    pid_t k = fork();
    if (k == 0) {
        close(up[0]);
        close(down[1]);
        exec_child(up[1], down[0]);
    }
    close(up[1]);
    close(down[0]);
    read(up[0], &c, 1);
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/pagemap", k);
    int fd = open(path, O_RDONLY);
    show("open the child's", fd < 0 ? -1 : 0);
    unsigned long long e;
    off_t at = 0x10000 / PG * 8;
    show("read before the exec", pread(fd, &e, 8, at));
    write(down[1], "g", 1);
    read(up[0], &c, 1);
    show("read after the exec", pread(fd, &e, 8, at));
    show("unaligned after the exec", pread(fd, &e, 8, at + 1));
    int fd2 = open(path, O_RDONLY);
    show("open again", fd2 < 0 ? -1 : 0);
    show("read the new image", pread(fd2, &e, 8, at));
    close(down[1]);
    int ws;
    waitpid(k, &ws, 0);
    printf("child exit %d\n", WIFEXITED(ws) ? WEXITSTATUS(ws) : -1);
    show("read after the end", pread(fd2, &e, 8, at));
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    if (argc > 3 && !strcmp(argv[1], "execd")) {
        char c;
        write(atoi(argv[2]), "b", 1);
        while (read(atoi(argv[3]), &c, 1) > 0)
            ;
        return 0;
    }
    if (argc > 1 && !strcmp(argv[1], "exec")) {
        exec_case();
        return 0;
    }
    pm = open("/proc/self/pagemap", O_RDONLY);
    show("open", pm < 0 ? -1 : 0);
    char *m = mmap(0, 8 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (argc > 1 && !strcmp(argv[1], "sticky")) {
        sticky(m);
        return 0;
    }
    struct stat st;
    fstat(pm, &st);
    printf("mode %o size %ld\n", st.st_mode & 0777, (long)st.st_size);
    for (int i = 0; i < 8; i++)
        if (i != 3)
            m[i * PG] = 'a' + i;
    pages("fresh", m, 8);
    gu("no guard yet", m);
    show("install 2..4", madvise(m + 2 * PG, 2 * PG, GUARD_INSTALL) ? -1 : 0);
    pages("guarded", m, 8);
    gu("with guards", m);
    show("DONTNEED all", madvise(m, 8 * PG, MADV_DONTNEED) ? -1 : 0);
    pages("after DONTNEED", m, 5);
    show("remove 2", madvise(m + 2 * PG, PG, GUARD_REMOVE) ? -1 : 0);
    pages("after remove", m + 2 * PG, 2);
    m[0] = 'x';
    pid_t c = fork();
    if (c == 0) {
        pages("child", m, 4);
        _exit(0);
    }
    int ws;
    waitpid(c, &ws, 0);
    pages("parent after the fork", m, 1);
    m[0] = 'y';
    pages("parent after its write", m, 1);

    /* A file: its page cache page, then a private copy. */
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-pagemap-%d", getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    char b[PG];
    memset(b, 'f', sizeof b);
    write(fd, b, sizeof b);
    char *f = mmap(0, PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    volatile char r = f[0];
    (void)r;
    pages("file read", f, 1);
    f[0] = 'g';
    pages("file written", f, 1);
    char *s = mmap(0, PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    s[0] = 'h';
    pages("shared written", s, 1);
    char *s2 = mmap(0, PG, PROT_READ, MAP_SHARED, fd, 0);
    r = s2[0];
    pages("shared twice", s, 1);
    unlink(path);

    /* The read rules. */
    unsigned long long e[2];
    off_t at = (unsigned long)m / PG * 8;
    show("pos not aligned", pread(pm, e, 8, at + 1));
    show("size not aligned", pread(pm, e, 7, at));
    show("size 0", pread(pm, e, 0, at));
    show("size 0 pos not aligned", pread(pm, e, 0, at + 4));
    unsigned long top = 0x7ffffffff000ul;
    show("last page and past it", pread(pm, e, 16, (top - PG) / PG * 8));
    show("at the top", pread(pm, e, 8, top / PG * 8));
    show("far past the top", pread(pm, e, 8, 1ul << 60));
    show("unmapped", pread(pm, e, 8, 4096 / PG * 8));
    printf("unmapped entry = %llu\n", e[0]);
    show("lseek far", lseek(pm, 1ul << 60, SEEK_SET) == (off_t)(1ul << 60) ? 0 : -1);
    show("read there", read(pm, e, 8));
    show("write", write(pm, e, 8));
    return 0;
}
