/* Wave A: ids, uname, set_tid_address, robust list, rlimits, getrandom, arch_prctl, sched_yield,
 * sched_getaffinity. */
#include "abi.h"
#include <ctype.h>
#include <sched.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/utsname.h>

#define ARCH_SET_GS 0x1001
#define ARCH_SET_FS 0x1002
#define ARCH_GET_FS 0x1003
#define ARCH_GET_GS 0x1004
#define ARCH_GET_CPUID 0x1011

static void ids(void) {
    pid_t pid = getpid();
    yes("gettid == getpid", syscall(SYS_gettid) == pid);
    yes("getppid >= 0", getppid() >= 0);
    yes("getuid == AT_UID", getuid() == getauxval(AT_UID));
    yes("geteuid == AT_EUID", geteuid() == getauxval(AT_EUID));
    yes("getgid == AT_GID", getgid() == getauxval(AT_GID));
    yes("getegid == AT_EGID", getegid() == getauxval(AT_EGID));
    uid_t r = 1, e = 1, s = 1;
    yes("getresuid = uid, euid, uid", getresuid(&r, &e, &s) == 0 && r == getuid() && e == geteuid() && s == getuid());
    gid_t gr = 1, ge = 1, gs = 1;
    yes("getresgid = gid, egid, gid", getresgid(&gr, &ge, &gs) == 0 && gr == getgid() && ge == getegid() && gs == getgid());
    SYS("getresuid(bad third)", SYS_getresuid, &r, &e, KERNEL_ADDR);
    SYS("getgroups(-1)", SYS_getgroups, -1, NULL);
    yes("getgroups(0) >= 0", syscall(SYS_getgroups, 0, NULL) >= 0);
}

static void uname_tid_robust(void) {
    struct utsname u;
    memset(&u, 0x55, sizeof u);
    SYS("uname", SYS_uname, &u);
    printf("sysname=%s machine=%s\n", u.sysname, u.machine);
    yes("release starts with a digit", isdigit((unsigned char)u.release[0]));
    yes("fields NUL-terminated", memchr(u.nodename, 0, sizeof u.nodename) && memchr(u.version, 0, sizeof u.version) && memchr(u.domainname, 0, sizeof u.domainname));
    SYS("uname(kernel address)", SYS_uname, KERNEL_ADDR);
    int word = 0;
    yes("set_tid_address returns the tid", syscall(SYS_set_tid_address, &word) == syscall(SYS_gettid));
    struct { void *next; long off; void *pending; } head = { &head, 0, 0 };
    SYS("set_robust_list(len 23)", SYS_set_robust_list, &head, 23);
    SYS("set_robust_list(len 24)", SYS_set_robust_list, &head, 24);
    void *hp = 0;
    size_t len = 0;
    SYS("get_robust_list(0)", SYS_get_robust_list, 0, &hp, &len);
    yes("robust head and length read back", hp == (void *)&head && len == 24);
    SYS("get_robust_list(bad len pointer)", SYS_get_robust_list, 0, &hp, KERNEL_ADDR);
    SYS("get_robust_list(no such pid)", SYS_get_robust_list, 4000000, &hp, &len);
}

static void limits(void) {
    struct rlimit l;
    getrlimit(RLIMIT_STACK, &l);
    printf("STACK cur=%llu\n", (unsigned long long)l.rlim_cur);
    /* (RLIMIT_CORE and NPROC are what the host's login set up: not printed.) */
    /* Lowering is always allowed: bring NOFILE to known values first. */
    struct rlimit n = { 256, 512 };
    SYS("setrlimit(NOFILE 256/512)", SYS_setrlimit, RLIMIT_NOFILE, &n);
    SYS("getrlimit(NOFILE)", SYS_getrlimit, RLIMIT_NOFILE, &l);
    printf("NOFILE %llu/%llu\n", (unsigned long long)l.rlim_cur, (unsigned long long)l.rlim_max);
    struct rlimit o = { 0, 0 };
    SYS("prlimit64(0, NOFILE, NULL, &old)", SYS_prlimit64, 0, RLIMIT_NOFILE, NULL, &o);
    printf("prlimit64 NOFILE %llu/%llu\n", (unsigned long long)o.rlim_cur, (unsigned long long)o.rlim_max);
    SYS("prlimit64(self pid)", SYS_prlimit64, getpid(), RLIMIT_NOFILE, NULL, &o);
    n = (struct rlimit){ 256, 1024 };
    SYS("setrlimit(raise the hard limit)", SYS_setrlimit, RLIMIT_NOFILE, &n);
    n = (struct rlimit){ 600, 512 };
    SYS("setrlimit(cur > max)", SYS_setrlimit, RLIMIT_NOFILE, &n);
    n = (struct rlimit){ 1, (1 << 20) + 1 };
    SYS("setrlimit(above nr_open)", SYS_setrlimit, RLIMIT_NOFILE, &n);
    SYS("getrlimit(16)", SYS_getrlimit, 16, &l);
    SYS("prlimit64(bad new)", SYS_prlimit64, 0, RLIMIT_NOFILE, KERNEL_ADDR, NULL);
    SYS("prlimit64(bad old)", SYS_prlimit64, 0, RLIMIT_NOFILE, NULL, KERNEL_ADDR);
    SYS("prlimit64(no such pid)", SYS_prlimit64, 4000000, RLIMIT_NOFILE, NULL, &o);
    /* The limit acts: fds up to 255, then EMFILE. */
    int last = -1, fd, e = 0;
    while ((fd = dup(0)) >= 0)
        last = fd;
    e = errno;
    printf("dup until it fails: highest fd %d, then %s\n", last, ename(e));
    for (fd = 3; fd <= last; fd++)
        close(fd);
}

static void randomness(void) {
    unsigned char b[256];
    memset(b, 0, sizeof b);
    SYS("getrandom(0 bytes)", SYS_getrandom, b, 0, 0);
    SYS("getrandom(flags 8)", SYS_getrandom, b, 16, 8);
    SYS("getrandom(INSECURE|RANDOM)", SYS_getrandom, b, 16, 6);
    SYS("getrandom(256)", SYS_getrandom, b, 256, 0);
    int zeros = 0;
    for (int i = 0; i < 256; i++)
        zeros += b[i] == 0;
    yes("random bytes are not all zero", zeros < 32);
    SYS("getrandom(16, GRND_NONBLOCK)", SYS_getrandom, b, 16, 1);
    SYS("getrandom(16, GRND_INSECURE)", SYS_getrandom, b, 16, 4);
    SYS("getrandom(16, GRND_RANDOM)", SYS_getrandom, b, 16, 2);
    SYS("getrandom(kernel address)", SYS_getrandom, KERNEL_ADDR, 16, 0);
    SYS("getrandom(NULL)", SYS_getrandom, NULL, 16, 0);
    unsigned char *p = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    munmap(p + 4096, 4096);
    SYS("getrandom(200 bytes, 100 before an unmapped page)", SYS_getrandom, p + 4096 - 100, 200, 0);
    munmap(p, 4096);
}

static void arch_and_sched(void) {
    unsigned long fs = 0, gs = 1;
    void *self;
    __asm__ volatile("mov %%fs:0, %0" : "=r"(self));
    SYS("arch_prctl(ARCH_GET_FS)", SYS_arch_prctl, ARCH_GET_FS, &fs);
    yes("ARCH_GET_FS is the thread pointer", (void *)fs == self);
    SYS("arch_prctl(ARCH_SET_GS)", SYS_arch_prctl, ARCH_SET_GS, 0x12345000ul);
    SYS("arch_prctl(ARCH_GET_GS)", SYS_arch_prctl, ARCH_GET_GS, &gs);
    yes("GS base read back", gs == 0x12345000ul);
    SYS("arch_prctl(ARCH_SET_GS, 0)", SYS_arch_prctl, ARCH_SET_GS, 0ul);
    SYS("arch_prctl(0x1234)", SYS_arch_prctl, 0x1234, 0);
    SYS("arch_prctl(ARCH_SET_FS, kernel address)", SYS_arch_prctl, ARCH_SET_FS, KERNEL_ADDR);
    SYS("arch_prctl(ARCH_GET_FS, kernel address)", SYS_arch_prctl, ARCH_GET_FS, KERNEL_ADDR);
    SYS("arch_prctl(ARCH_GET_CPUID)", SYS_arch_prctl, ARCH_GET_CPUID, 0);
    SYS("sched_yield", SYS_sched_yield);
    long n = sysconf(_SC_NPROCESSORS_ONLN);
    yes("online CPUs >= 1", n >= 1);
    cpu_set_t set;
    memset(&set, 0, sizeof set);
    long r = syscall(SYS_sched_getaffinity, 0, sizeof set, &set);
    yes("sched_getaffinity size is a multiple of 8", r > 0 && r % 8 == 0);
    yes("affinity CPUs == sysconf(_SC_NPROCESSORS_ONLN)", CPU_COUNT(&set) == n);
    SYS("sched_getaffinity(len 12)", SYS_sched_getaffinity, 0, 12, &set);
    SYS("sched_getaffinity(no such pid)", SYS_sched_getaffinity, 4000000, sizeof set, &set);
    SYS("sched_getaffinity(kernel address)", SYS_sched_getaffinity, 0, sizeof set, KERNEL_ADDR);
}

int main(void) {
    ids();
    uname_tid_robust();
    limits();
    randomness();
    arch_and_sched();
    return 0;
}
