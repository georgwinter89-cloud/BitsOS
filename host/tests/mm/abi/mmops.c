// MM-5a ABI diff, Linux side: runs crates/mm/abi/mm-ops.txt (and MM-5b's mm-ops2.txt: madvise, mremap,
// mincore) with real syscalls on CachyOS and prints the same text the kernel's interpreter (crates/mm/src/abi.rs)
// prints. Nothing here may call malloc or stdio
// while the script runs: the program break and the address space must stay as the script leaves them.
// Build: cc -O1 -o mmops mmops.c (dynamic: a static glibc moves the break for TLS before main).
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define WINDOW 0x200000000000ULL
#define WINDOW_LEN 0x40000000ULL
#define HEAP_LEN 0x1000000ULL

static char script[1 << 16];
static char out[1 << 18];
static size_t outn;
static uint64_t heap, last;
static sigjmp_buf jb;

static void put(const char *s) {
    size_t n = strlen(s);
    if (outn + n < sizeof out) {
        memcpy(out + outn, s, n);
        outn += n;
    }
}

static int num(const char *s, uint64_t *v) {
    char *end;
    errno = 0;
    *v = strtoull(s, &end, 0);
    return *s && !*end && !errno;
}

static int addr(const char *s, uint64_t *v) {
    uint64_t base, n;
    switch (s[0]) {
    case 'B': base = WINDOW; break;
    case 'H': base = heap; break;
    case 'L': base = last; break;
    default: return num(s, v);
    }
    if (!s[1]) { *v = base; return 1; }
    if ((s[1] != '+' && s[1] != '-') || !num(s + 2, &n)) return 0;
    *v = s[1] == '+' ? base + n : base - n;
    return 1;
}

static void show(uint64_t a, char *buf, size_t len) {
    uint64_t d = a > heap ? a - heap : heap - a;
    if (a >= WINDOW && a <= WINDOW + WINDOW_LEN)
        snprintf(buf, len, "B+0x%llx", (unsigned long long)(a - WINDOW));
    else if (d <= HEAP_LEN)
        snprintf(buf, len, a >= heap ? "H+0x%llx" : "H-0x%llx", (unsigned long long)d);
    else if (a >= 0x40000000 && a < 0x80000000)
        snprintf(buf, len, "in-32bit-window");
    else if (a < 0x100000000ULL)
        snprintf(buf, len, "abs 0x%llx", (unsigned long long)a);
    else
        snprintf(buf, len, "elsewhere");
}

static int prot(const char *s, uint64_t *v) {
    if (!strncmp(s, "0x", 2)) return num(s, v);
    if (!strcmp(s, "none")) { *v = 0; return 1; }
    *v = 0;
    for (; *s; s++) {
        if (*s == 'r') *v |= 1;
        else if (*s == 'w') *v |= 2;
        else if (*s == 'x') *v |= 4;
        else return 0;
    }
    return 1;
}

static int flags(char *s, uint64_t *v) {
    static const struct { const char *n; uint64_t b; } t[] = {
        {"none", 0}, {"private", 0x02}, {"shared", 0x01}, {"validate", 0x03}, {"anon", 0x20}, {"fixed", 0x10},
        {"noreplace", 0x100000}, {"noreserve", 0x4000}, {"populate", 0x8000}, {"nonblock", 0x10000},
        {"stack", 0x20000}, {"hugetlb", 0x40000}, {"32bit", 0x40},
    };
    *v = 0;
    char *save;
    for (char *p = strtok_r(s, "|", &save); p; p = strtok_r(NULL, "|", &save)) {
        size_t i;
        for (i = 0; i < sizeof t / sizeof t[0] && strcmp(t[i].n, p); i++) {}
        uint64_t b;
        if (i < sizeof t / sizeof t[0]) b = t[i].b;
        else if (!num(p, &b)) return 0;
        *v |= b;
    }
    return 1;
}

static const char *ename(long e) {
    switch (e) {
    case EPERM: return "EPERM";
    case ENOMEM: return "ENOMEM";
    case EINVAL: return "EINVAL";
    case EEXIST: return "EEXIST";
    case ENODEV: return "ENODEV";
    case EOVERFLOW: return "EOVERFLOW";
    case EBADF: return "EBADF";
    case EACCES: return "EACCES";
    case EAGAIN: return "EAGAIN";
    case EFAULT: return "EFAULT";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    default: return "E?";
    }
}

static int advice(const char *s, uint64_t *v) {
    static const struct { const char *n; uint64_t b; } t[] = {
        {"normal", 0}, {"random", 1}, {"sequential", 2}, {"willneed", 3}, {"dontneed", 4}, {"free", 8},
        {"remove", 9}, {"dontfork", 10}, {"dofork", 11}, {"mergeable", 12}, {"unmergeable", 13},
        {"hugepage", 14}, {"nohugepage", 15}, {"dontdump", 16}, {"dodump", 17}, {"wipeonfork", 18},
        {"keeponfork", 19}, {"cold", 20}, {"pageout", 21}, {"populate_read", 22}, {"populate_write", 23},
        {"dontneed_locked", 24}, {"collapse", 25}, {"hwpoison", 100}, {"soft_offline", 101},
    };
    for (size_t i = 0; i < sizeof t / sizeof t[0]; i++)
        if (!strcmp(t[i].n, s)) { *v = t[i].b; return 1; }
    return num(s, v);
}

static int rflags(char *s, uint64_t *v) {
    *v = 0;
    char *save;
    for (char *p = strtok_r(s, "|", &save); p; p = strtok_r(NULL, "|", &save)) {
        uint64_t b;
        if (!strcmp(p, "none")) b = 0;
        else if (!strcmp(p, "maymove")) b = 1;
        else if (!strcmp(p, "fixed")) b = 2;
        else if (!strcmp(p, "dontunmap")) b = 4;
        else if (!num(p, &b)) return 0;
        *v |= b;
    }
    return 1;
}

// Raw syscalls: glibc's wrappers would hide the kernel's answer (brk) or set errno only.
static long sc(long n, long a, long b, long c, long d, long e, long f) {
    long r = syscall(n, a, b, c, d, e, f);
    return r == -1 ? -errno : r;
}

static void err(long r, char *buf, size_t len) { snprintf(buf, len, "-%s", ename(-r)); }

static void segv(int sig) { siglongjmp(jb, sig); }

static int store(uint64_t a, uint8_t b) {
    if (sigsetjmp(jb, 1)) return 0;
    *(volatile uint8_t *)a = b;
    return 1;
}

static int load(uint64_t a, uint8_t *b) {
    if (sigsetjmp(jb, 1)) return 0;
    *b = *(volatile uint8_t *)a;
    return 1;
}

// Mapped ranges in [lo, hi) from /proc/self/maps, adjacent equal rights merged (as abi.rs prints them).
static void ranges(uint64_t lo, uint64_t hi) {
    static char maps[1 << 20];
    int fd = open("/proc/self/maps", O_RDONLY);
    ssize_t n = 0, r;
    while (fd >= 0 && (r = read(fd, maps + n, sizeof maps - 1 - n)) > 0) n += r;
    if (fd >= 0) close(fd);
    maps[n < 0 ? 0 : n] = 0;
    uint64_t ms = 0, me = 0;
    char mp[4] = "", buf[160], b1[40], b2[40];
    int any = 0;
    for (char *l = maps; *l;) {
        char *nl = strchr(l, '\n');
        if (nl) *nl = 0;
        unsigned long long s, e;
        char perms[5];
        if (sscanf(l, "%llx-%llx %4s", &s, &e, perms) == 3 && s < hi && e > lo) {
            if (s < lo) s = lo;
            if (e > hi) e = hi;
            perms[3] = 0;
            if (any && me == s && !strcmp(mp, perms)) {
                me = e;
            } else {
                if (any) {
                    show(ms, b1, sizeof b1), show(me, b2, sizeof b2);
                    snprintf(buf, sizeof buf, "  %s..%s %s\n", b1, b2, mp), put(buf);
                }
                ms = s, me = e, strcpy(mp, perms), any = 1;
            }
        }
        if (!nl) break;
        l = nl + 1;
    }
    if (any) {
        show(ms, b1, sizeof b1), show(me, b2, sizeof b2);
        snprintf(buf, sizeof buf, "  %s..%s %s\n", b1, b2, mp), put(buf);
    } else {
        put("  (none)\n");
    }
}

static int step(char **w, int n, char *res, size_t len) {
    uint64_t a, l, p, f;
    if (!strcmp(w[0], "mmap") && n == 5) {
        if (!addr(w[1], &a) || !num(w[2], &l) || !prot(w[3], &p) || !flags(w[4], &f)) return 0;
        long r = sc(SYS_mmap, a, l, p, f, -1, 0);
        if (r < 0 && r > -4096) err(r, res, len);
        else last = r, show(r, res, len);
    } else if (!strcmp(w[0], "munmap") && n == 3) {
        if (!addr(w[1], &a) || !num(w[2], &l)) return 0;
        long r = sc(SYS_munmap, a, l, 0, 0, 0, 0);
        if (r) err(r, res, len); else snprintf(res, len, "0");
    } else if (!strcmp(w[0], "mprotect") && n == 4) {
        if (!addr(w[1], &a) || !num(w[2], &l) || !prot(w[3], &p)) return 0;
        long r = sc(SYS_mprotect, a, l, p, 0, 0, 0);
        if (r) err(r, res, len); else snprintf(res, len, "0");
    } else if (!strcmp(w[0], "brk") && n == 2) {
        if (!addr(w[1], &a)) return 0;
        show(sc(SYS_brk, a, 0, 0, 0, 0, 0), res, len);
    } else if (!strcmp(w[0], "store") && n == 3) {
        if (!addr(w[1], &a) || !num(w[2], &l)) return 0;
        snprintf(res, len, store(a, (uint8_t)l) ? "ok" : "fault");
    } else if (!strcmp(w[0], "load") && n == 2) {
        uint8_t b;
        if (!addr(w[1], &a)) return 0;
        if (load(a, &b)) snprintf(res, len, "0x%x", b); else snprintf(res, len, "fault");
    } else if (!strcmp(w[0], "madvise") && n == 4) {
        if (!addr(w[1], &a) || !num(w[2], &l) || !advice(w[3], &p)) return 0;
        long r = sc(SYS_madvise, a, l, p, 0, 0, 0);
        if (r) err(r, res, len); else snprintf(res, len, "0");
    } else if (!strcmp(w[0], "mremap") && n == 6) {
        uint64_t nl, na;
        if (!addr(w[1], &a) || !num(w[2], &l) || !num(w[3], &nl) || !rflags(w[4], &f) || !addr(w[5], &na)) return 0;
        long r = sc(SYS_mremap, a, l, nl, f, na, 0);
        if (r < 0 && r > -4096) err(r, res, len);
        else last = r, show(r, res, len);
    } else if (!strcmp(w[0], "mincore") && n == 3) {
        static unsigned char vec[256];
        if (!addr(w[1], &a) || !num(w[2], &l) || l > sizeof vec * 4096ULL) return 0;
        long r = sc(SYS_mincore, a, l, (long)vec, 0, 0, 0);
        if (r) {
            err(r, res, len);
        } else {
            size_t k = (l + 4095) / 4096, i;
            for (i = 0; i < k && i + 1 < len; i++) res[i] = '0' + (vec[i] & 1);
            res[i] = 0;
        }
    } else if ((!strcmp(w[0], "maps") || !strcmp(w[0], "heap")) && n == 1) {
        res[0] = 0;
    } else {
        return 0;
    }
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s mm-ops.txt\n", argv[0]);
        return 2;
    }
    int fd = open(argv[1], O_RDONLY);
    ssize_t n = 0, r;
    while (fd >= 0 && (r = read(fd, script + n, sizeof script - 1 - n)) > 0) n += r;
    if (fd < 0 || n <= 0) {
        perror(argv[1]);
        return 2;
    }
    close(fd);
    struct sigaction sa = {0};
    sa.sa_handler = segv;
    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);
    heap = sc(SYS_brk, 0, 0, 0, 0, 0, 0);
    for (char *line = script; *line;) {
        char *nl = strchr(line, '\n');
        if (nl) *nl = 0;
        size_t ll = strlen(line);
        while (ll && (line[ll - 1] == ' ' || line[ll - 1] == '\r')) line[--ll] = 0;
        if (ll && line[0] != '#') {
            char copy[512], res[128], *w[8] = {0};
            int wn = 0;
            if (ll >= sizeof copy) ll = sizeof copy - 1;
            memcpy(copy, line, ll);
            copy[ll] = 0;
            char *save;
            for (char *p = strtok_r(copy, " ", &save); p && wn < 8; p = strtok_r(NULL, " ", &save)) w[wn++] = p;
            if (wn == 0 || !step(w, wn, res, sizeof res)) snprintf(res, sizeof res, "?? bad line");
            put(line), put(" => "), put(res), put("\n");
            if (wn && !strcmp(w[0], "maps")) ranges(WINDOW, WINDOW + WINDOW_LEN);
            if (wn && !strcmp(w[0], "heap")) ranges(heap, heap + HEAP_LEN);
        }
        if (!nl) break;
        line = nl + 1;
    }
    for (size_t done = 0; done < outn;) {
        ssize_t w = write(1, out + done, outn - done);
        if (w <= 0) return 1;
        done += w;
    }
    return 0;
}
