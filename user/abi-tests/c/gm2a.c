/* GM-2a CR 3, 4, 5: calls Steam's helpers make, as an ordinary user: ptrace without tracing, the mlock family under a lowered
 * RLIMIT_MEMLOCK, personality, the xattr errors that come before a file system's own answer (the host's file systems have
 * xattrs, V2's none: the rest is in crates/vfs/src/sys_xattr_tests.rs), readahead and sync_file_range. */
#include "abi.h"
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/personality.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/wait.h>

#define P 4096ul
#ifndef PTRACE_SEIZE
#define PTRACE_SEIZE 0x4206
#endif

static void ptrace_calls(void) {
    long me = getpid(), parent = getppid();
    SYS("ptrace(PEEKDATA, missing pid)", SYS_ptrace, 2, 0x7ffffff0, 0, 0);
    SYS("ptrace(unknown, missing pid)", SYS_ptrace, 0x7777, 0x7ffffff0, 0, 0);
    SYS("ptrace(ATTACH, missing pid)", SYS_ptrace, 16, 0x7ffffff0, 0, 0);
    SYS("ptrace(PEEKDATA, 0)", SYS_ptrace, 2, 0, 0, 0);
    SYS("ptrace(PEEKDATA, parent)", SYS_ptrace, 2, parent, 0, 0);
    SYS("ptrace(POKEDATA, parent)", SYS_ptrace, 5, parent, 0, 0);
    SYS("ptrace(unknown, parent)", SYS_ptrace, 0x7777, parent, 0, 0);
    SYS("ptrace(ATTACH, self)", SYS_ptrace, 16, me, 0, 0);
    SYS("ptrace(SEIZE, self)", SYS_ptrace, PTRACE_SEIZE, me, 0, 0);
    SYS("ptrace(SEIZE, self, addr)", SYS_ptrace, PTRACE_SEIZE, me, 8, 0);
    SYS("ptrace(SEIZE, self, bad options)", SYS_ptrace, PTRACE_SEIZE, me, 0, 0x100ul);
}

static long vmlck(void) {
    FILE *f = fopen("/proc/self/status", "r");
    char line[256];
    long v = -1;
    while (f && fgets(line, sizeof line, f))
        if (sscanf(line, "VmLck: %ld", &v) == 1)
            break;
    if (f)
        fclose(f);
    return v;
}

/* The smaps rows of the VMA at `at` that mlock changes: Locked in kB, and `lo` and `lf` of VmFlags. */
static void smaps_of(const char *what, const void *at) {
    FILE *f = fopen("/proc/self/smaps", "r");
    char line[256];
    long locked = -1, lo = -1, lf = -1;
    int in = 0;
    while (f && fgets(line, sizeof line, f)) {
        unsigned long a, b;
        if (sscanf(line, "%lx-%lx ", &a, &b) == 2)
            in = a <= (unsigned long)at && (unsigned long)at < b;
        if (!in)
            continue;
        sscanf(line, "Locked: %ld", &locked);
        if (!strncmp(line, "VmFlags:", 8)) {
            lo = strstr(line, " lo ") != 0;
            lf = strstr(line, " lf ") != 0;
        }
    }
    if (f)
        fclose(f);
    printf("smaps %s: Locked %ld kB, lo %ld, lf %ld\n", what, locked, lo, lf);
}

static void limit(unsigned long bytes) {
    struct rlimit r;
    getrlimit(RLIMIT_MEMLOCK, &r);
    r.rlim_cur = bytes;
    show("setrlimit(RLIMIT_MEMLOCK)", setrlimit(RLIMIT_MEMLOCK, &r));
}

static void mlock_calls(void) {
    char *m = mmap(0, 8 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *n = mmap(0, P, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *hole = mmap(0, 3 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    munmap(hole + P, P);

    limit(0);
    SYS("mlock, limit 0", SYS_mlock, m, P);
    SYS("mlock2, limit 0", SYS_mlock2, m, P, 0);
    SYS("mlockall, limit 0", SYS_mlockall, 1);
    SYS("munlock, limit 0", SYS_munlock, m, P);
    SYS("munlockall, limit 0", SYS_munlockall);

    limit(4 * P);
    SYS("mlock2(bad flags)", SYS_mlock2, m, P, 2);
    SYS("mlock, above the limit", SYS_mlock, m, 5 * P);
    printf("VmLck %ld\n", vmlck());
    SYS("mlock, the limit", SYS_mlock, m, 4 * P);
    printf("VmLck %ld\n", vmlck());
    SYS("mlock, again", SYS_mlock, m, 4 * P);
    SYS("mlock, one more", SYS_mlock, m + 4 * P, P);
    SYS("munlock, the middle", SYS_munlock, m + P, 2 * P);
    printf("VmLck %ld\n", vmlck());
    SYS("munlock, all", SYS_munlock, m, 8 * P);
    SYS("mlock, unaligned, length 0", SYS_mlock, m + 16, 0);
    printf("VmLck %ld\n", vmlck());
    SYS("mlock, length 0", SYS_mlock, m + P, 0);
    SYS("munlock, unaligned", SYS_munlock, m + 16, 1);
    printf("VmLck %ld\n", vmlck());
    SYS("mlock, past 2^64", SYS_mlock, m, ~0ul - 8192);
    SYS("munlock, past 2^64", SYS_munlock, m, ~0ul - 8192);
    SYS("mlock, over a hole", SYS_mlock, hole, 3 * P);
    printf("VmLck %ld\n", vmlck());
    SYS("munlock, over a hole", SYS_munlock, hole, 3 * P);
    printf("VmLck %ld\n", vmlck());
    SYS("mlock, in the hole", SYS_mlock, hole + P, P);
    SYS("mlock, PROT_NONE", SYS_mlock, n, P);
    printf("VmLck %ld\n", vmlck());
    SYS("munlock, PROT_NONE", SYS_munlock, n, P);
    SYS("mlock2(ONFAULT)", SYS_mlock2, m, 2 * P, 1);
    printf("VmLck %ld\n", vmlck());
    SYS("munlock, onfault", SYS_munlock, m, 2 * P);

    SYS("madvise(DONTNEED), plain", SYS_madvise, m, P, MADV_DONTNEED);
    SYS("mlock, for madvise", SYS_mlock, m, P);
    SYS("madvise(DONTNEED), locked", SYS_madvise, m, P, MADV_DONTNEED);
    SYS("madvise(FREE), locked", SYS_madvise, m, P, MADV_FREE);
    SYS("madvise(DONTNEED_LOCKED), locked", SYS_madvise, m, P, 24);
    SYS("msync(INVALIDATE), locked", SYS_msync, m, P, MS_INVALIDATE);
    pid_t c = fork();
    if (c == 0) {
        printf("VmLck in a fork %ld\n", vmlck());
        fflush(stdout);
        _exit(0);
    }
    waitpid(c, 0, 0);
    SYS("munlockall", SYS_munlockall);
    printf("VmLck %ld\n", vmlck());

    SYS("mlockall(0)", SYS_mlockall, 0);
    SYS("mlockall(8)", SYS_mlockall, 8);
    SYS("mlockall(MCL_ONFAULT)", SYS_mlockall, 4);
    SYS("mlockall(MCL_CURRENT), above the limit", SYS_mlockall, 1);
    SYS("mlockall(MCL_FUTURE)", SYS_mlockall, 2);
    printf("VmLck %ld\n", vmlck());
    char *f = mmap(0, 2 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    yes("a new mapping under MCL_FUTURE", f != MAP_FAILED);
    printf("VmLck %ld\n", vmlck());
    errno = 0;
    char *g = mmap(0, 3 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    printf("a mapping above the limit: %s\n", g == MAP_FAILED ? ename(errno) : "mapped");
    /* The limit counts as it is at the mapping's call, not as it was at the mlockall. */
    limit(16 * P);
    g = mmap(0, 3 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    yes("a mapping after the limit was raised", g != MAP_FAILED);
    printf("VmLck %ld\n", vmlck());
    limit(4 * P);
    errno = 0;
    g = mmap(0, P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    printf("a mapping after the limit was lowered: %s\n", g == MAP_FAILED ? ename(errno) : "mapped");
    long heap = syscall(SYS_brk, 0);
    yes("brk refused under the lowered limit", syscall(SYS_brk, heap + 2 * P) == heap);
    limit(16 * P);
    yes("brk moves under the raised limit", syscall(SYS_brk, heap + 2 * P) == heap + 2 * (long)P);
    syscall(SYS_brk, heap);
    SYS("munlockall", SYS_munlockall);
    printf("VmLck %ld\n", vmlck());
    limit(4 * P);

    /* smaps of a locked mapping and of one locked on fault. */
    char *s = mmap(0, 6 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    SYS("mlock, for smaps", SYS_mlock, s, 2 * P);
    smaps_of("locked", s);
    SYS("munlock, for smaps", SYS_munlock, s, 2 * P);
    SYS("mlock2(ONFAULT), for smaps", SYS_mlock2, s + 3 * P, 2 * P, 1);
    s[3 * P] = 1;
    smaps_of("locked on fault", s + 3 * P);
    smaps_of("plain", s + 5 * P);
    SYS("munlock, onfault for smaps", SYS_munlock, s + 3 * P, 2 * P);
}

static void personality_calls(void) {
    long old = SYS("personality(read)", SYS_personality, 0xffffffffl);
    SYS("personality(ADDR_NO_RANDOMIZE)", SYS_personality, ADDR_NO_RANDOMIZE);
    SYS("personality(read)", SYS_personality, 0xffffffffl);
    char b[32] = "";
    int fd = open("/proc/self/personality", O_RDONLY);
    if (fd >= 0) {
        long n = read(fd, b, sizeof b - 1);
        b[n > 0 ? n : 0] = 0;
        close(fd);
    }
    printf("/proc/self/personality %s", b);
    pid_t c = fork();
    if (c == 0) {
        show("in a fork", personality(0xffffffff));
        fflush(stdout);
        _exit(0);
    }
    waitpid(c, 0, 0);
    SYS("personality(back)", SYS_personality, old);
    SYS("personality(read)", SYS_personality, 0xffffffffl);
}

static void xattr_calls(void) {
    char path[64], name[300];
    snprintf(path, sizeof path, "/tmp/gm2a-%d", getpid());
    int fd = open(path, O_CREAT | O_RDWR, 0644);
    char *ok = "user.a";
    static char big[65537];
    memset(name, 'a', sizeof name);
    name[256] = 0;

    SYS("setxattr(missing, bad flags)", SYS_setxattr, "/tmp/gm2a-none", 0, 0, 0, 7);
    SYS("lsetxattr(missing)", SYS_lsetxattr, "/tmp/gm2a-none", ok, "v", 1, 0);
    SYS("getxattr(missing, bad name)", SYS_getxattr, "/tmp/gm2a-none", 0, 0, 0);
    SYS("lgetxattr(missing)", SYS_lgetxattr, "/tmp/gm2a-none", ok, 0, 0);
    SYS("listxattr(missing)", SYS_listxattr, "/tmp/gm2a-none", 0, 0);
    SYS("llistxattr(missing)", SYS_llistxattr, "/tmp/gm2a-none", 0, 0);
    SYS("removexattr(missing)", SYS_removexattr, "/tmp/gm2a-none", ok);
    SYS("lremovexattr(missing)", SYS_lremovexattr, "/tmp/gm2a-none", ok);
    SYS("getxattr(path fault)", SYS_getxattr, KERNEL_ADDR, ok, 0, 0);

    SYS("removexattr(missing, bad name)", SYS_removexattr, "/tmp/gm2a-none", 0);
    SYS("fsetxattr(bad fd, bad flags)", SYS_fsetxattr, 99, ok, "v", 1, 7);
    SYS("fsetxattr(bad fd, bad name)", SYS_fsetxattr, 99, 0, "v", 1, 0);
    SYS("fgetxattr(bad fd, bad name)", SYS_fgetxattr, 99, 0, 0, 0);
    SYS("fremovexattr(bad fd, bad name)", SYS_fremovexattr, 99, 0);
    SYS("fsetxattr(bad fd)", SYS_fsetxattr, 99, ok, "v", 1, 0);
    SYS("fgetxattr(bad fd)", SYS_fgetxattr, 99, ok, 0, 0);
    SYS("flistxattr(bad fd)", SYS_flistxattr, 99, 0, 0);
    SYS("fremovexattr(bad fd)", SYS_fremovexattr, 99, ok);
    int op = open(path, O_PATH);
    SYS("fgetxattr(O_PATH)", SYS_fgetxattr, op, ok, 0, 0);
    SYS("fsetxattr(O_PATH)", SYS_fsetxattr, op, ok, "v", 1, 0);
    close(op);

    SYS("setxattr(bad flags)", SYS_setxattr, path, ok, "v", 1, 4);
    SYS("setxattr(name fault)", SYS_setxattr, path, KERNEL_ADDR, "v", 1, 0);
    SYS("setxattr(empty name)", SYS_setxattr, path, "", "v", 1, 0);
    SYS("setxattr(name of 256)", SYS_setxattr, path, name, "v", 1, 0);
    SYS("setxattr(size)", SYS_setxattr, path, ok, big, sizeof big, 0);
    SYS("setxattr(value fault)", SYS_setxattr, path, ok, KERNEL_ADDR, 1, 0);
    SYS("fsetxattr(empty name)", SYS_fsetxattr, fd, "", "v", 1, 0);
    SYS("getxattr(empty name)", SYS_getxattr, path, "", 0, 0);
    SYS("getxattr(name of 256)", SYS_getxattr, path, name, 0, 0);
    SYS("fgetxattr(name fault)", SYS_fgetxattr, fd, KERNEL_ADDR, 0, 0);
    SYS("removexattr(empty name)", SYS_removexattr, path, "");
    SYS("fremovexattr(name of 256)", SYS_fremovexattr, fd, name);
    close(fd);
    unlink(path);
}

static void file_calls(void) {
    char path[64];
    int p[2];
    snprintf(path, sizeof path, "/tmp/gm2a-r-%d", getpid());
    int fd = open(path, O_CREAT | O_RDWR, 0644);
    int wr = open(path, O_WRONLY);
    int dir = open("/tmp", O_RDONLY);
    pipe(p);
    (void)!write(fd, "hello", 5);

    SYS("readahead", SYS_readahead, fd, 0, 4096);
    SYS("readahead(negative offset)", SYS_readahead, fd, -1l, 0);
    SYS("readahead(bad fd)", SYS_readahead, 99, 0, 0);
    SYS("readahead(write only)", SYS_readahead, wr, 0, 0);
    SYS("readahead(pipe)", SYS_readahead, p[0], 0, 0);
    SYS("readahead(directory)", SYS_readahead, dir, 0, 0);
    SYS("readahead(negative count)", SYS_readahead, fd, 0, -1l);

    SYS("sync_file_range", SYS_sync_file_range, fd, 0, 0, 0);
    SYS("sync_file_range(all flags)", SYS_sync_file_range, fd, 0, 4096, 7);
    SYS("sync_file_range(directory)", SYS_sync_file_range, dir, 0, 0, 2);
    SYS("sync_file_range(bad fd)", SYS_sync_file_range, 99, 0, 0, 0);
    SYS("sync_file_range(bad flags)", SYS_sync_file_range, fd, 0, 0, 8);
    SYS("sync_file_range(negative offset)", SYS_sync_file_range, fd, -1l, 0, 0);
    SYS("sync_file_range(negative end)", SYS_sync_file_range, fd, 1, -2l, 0);
    SYS("sync_file_range(wrap)", SYS_sync_file_range, fd, 0x7ffffffffffffffful, 1, 0);
    SYS("sync_file_range(pipe)", SYS_sync_file_range, p[0], 0, 0, 0);
    unlink(path);
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    ptrace_calls();
    mlock_calls();
    personality_calls();
    xattr_calls();
    file_calls();
    return 0;
}
