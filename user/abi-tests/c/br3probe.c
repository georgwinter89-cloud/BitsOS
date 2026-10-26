/* BR-3: BR-1's forecast rows that Brave's start on V2 cannot show by itself (the serial log only names ENOSYS and
 * ENOTTY), probed one by one; one part per argv[1]:
 *   pkey      pkey_alloc/pkey_mprotect/pkey_free lines that are the same with and without PKU (mm/mprotect.c:690-731,
 *             :851-890; BR-1 row 14)
 *   anonname  PR_SET_VMA_ANON_NAME (kernel/sys.c:2364; Brave calls it ~125 times per 30 s; BR-1 row 22)
 *   probes    rows 9, 10, 19, 25, 26, 27: oom_score_adj, clear_refs, TCP_INFO, bad-argument probes, ioctls
 *   env       rows 11-13, 21, 23: files and routes that depend on the machine (a difference here is not a bug
 *             by itself; the transcript records V2's answer)
 * Every line is a result or an errno name; nothing names a pid, an address or this machine. */
#include "sock.h"
#include <fcntl.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <linux/futex.h>
#include <linux/netlink.h>

#define PR_SET_VMA_ 0x53564d41
#define PR_SET_VMA_ANON_NAME_ 0
#define TCGETS2_ 0x802c542aul
#define F2FS_IOC_GET_FEATURES_ 0x8004f50cul

static void pkey(void) {
    char *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    SYS("pkey_alloc(flags 1)", SYS_pkey_alloc, 1, 0);
    SYS("pkey_alloc(init_val 4)", SYS_pkey_alloc, 0, 4);
    SYS("pkey_mprotect(pkey -1) = mprotect", SYS_pkey_mprotect, p, 4096, PROT_READ, -1);
    /* The mprotect argument checks come before the key check (mm/mprotect.c:708-729). */
    SYS("pkey_mprotect(unaligned, pkey 15)", SYS_pkey_mprotect, p + 1, 4096, PROT_READ, 15);
    SYS("pkey_mprotect(len 0, pkey 15)", SYS_pkey_mprotect, p, 0, PROT_READ, 15);
    SYS("pkey_mprotect(wrapping end, pkey 15)", SYS_pkey_mprotect, p, -4096L, PROT_READ, 15);
    SYS("pkey_mprotect(pkey 15)", SYS_pkey_mprotect, p, 4096, PROT_READ, 15);
    SYS("pkey_mprotect(pkey -2)", SYS_pkey_mprotect, p, 4096, PROT_READ, -2);
    SYS("pkey_free(15)", SYS_pkey_free, 15);
    SYS("pkey_free(-1)", SYS_pkey_free, -1);
    munmap(p, 4096);
}

static void anonname(void) {
    char *p = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    SYS("PR_SET_VMA_ANON_NAME br3", SYS_prctl, PR_SET_VMA_, PR_SET_VMA_ANON_NAME_, p, 8192, "br3");
    SYS("PR_SET_VMA_ANON_NAME with a bad character", SYS_prctl, PR_SET_VMA_, PR_SET_VMA_ANON_NAME_, p, 8192, "a$b");
    SYS("PR_SET_VMA_ANON_NAME NULL (clear)", SYS_prctl, PR_SET_VMA_, PR_SET_VMA_ANON_NAME_, p, 8192, 0);
    SYS("PR_SET_VMA_ANON_NAME br3 again", SYS_prctl, PR_SET_VMA_, PR_SET_VMA_ANON_NAME_, p, 8192, "br3");
    FILE *f = fopen("/proc/self/maps", "r");
    char line[512];
    int named = 0;
    while (f && fgets(line, sizeof line, f))
        named |= strstr(line, "[anon:br3]") != 0;
    if (f)
        fclose(f);
    yes("/proc/self/maps shows [anon:br3]", named);
    munmap(p, 8192);
}

static void wr(const char *label, int fd, const char *text) {
    lseek(fd, 0, SEEK_SET);
    long n = write(fd, text, strlen(text));
    if (n == (long)strlen(text))
        printf("%s = all of it\n", label);
    else
        res(label, n);
}

static void probes(void) {
    /* Row 9: the zygote's creat() on oom_score_adj, then values in and out of range (base.c oom_score_adj_write). */
    char cur[16] = "";
    int fd = open("/proc/self/oom_score_adj", O_RDONLY);
    if (fd >= 0) {
        long n = read(fd, cur, sizeof cur - 1);
        cur[n > 0 ? n : 0] = 0;
        close(fd);
    }
    fd = R("open oom_score_adj O_WRONLY|O_CREAT|O_TRUNC", open("/proc/self/oom_score_adj", O_WRONLY | O_CREAT | O_TRUNC, 0600));
    if (fd >= 0) {
        wr("write the current value back", fd, cur);
        wr("write 1001", fd, "1001");
        wr("write -1001", fd, "-1001");
        close(fd);
    }
    /* Row 10. */
    fd = R("open /proc/self/clear_refs O_WRONLY", open("/proc/self/clear_refs", O_WRONLY));
    if (fd >= 0) {
        wr("write 5 (reset peak RSS)", fd, "5");
        close(fd);
    }
    /* Row 19: TCP_INFO on a socket that never connected. */
    int t = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, IPPROTO_TCP);
    struct tcp_info ti;
    memset(&ti, 0xaa, sizeof ti);
    socklen_t tl = sizeof ti;
    R("getsockopt TCP_INFO", getsockopt(t, IPPROTO_TCP, TCP_INFO, &ti, &tl));
    printf("TCP_INFO state %d (7 = TCP_CLOSE)\n", ti.tcpi_state);
    close(t);
    /* Row 25: feature probes with bad pointers or flags. */
    SYS("prlimit64(NOFILE, old = kernel pointer)", SYS_prlimit64, 0, RLIMIT_NOFILE, 0, KERNEL_ADDR);
    SYS("eventfd2(flags 2)", SYS_eventfd2, 0, 2);
    SYS("memfd_create(flags 0x100)", SYS_memfd_create, "br3", 0x100);
    char *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    SYS("madvise(advice 99)", SYS_madvise, p, 4096, 99);
    munmap(p, 4096);
    /* Row 26. */
    struct { void *h; unsigned long f; void *r; unsigned long m; } old;
    SYS("rt_sigaction(65, NULL, &old)", SYS_rt_sigaction, 65, 0, &old, 8);
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv);
    int one = 1;
    R("setsockopt(AF_UNIX stream, TCP_NODELAY)", setsockopt(sv[0], IPPROTO_TCP, TCP_NODELAY, &one, sizeof one));
    unsigned word = 0;
    SYS("FUTEX_UNLOCK_PI_PRIVATE on a word nobody owns", SYS_futex, &word, FUTEX_UNLOCK_PI | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        if (geteuid() == 0 && (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534)))
            _exit(3);
        int n = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
        int big = 1 << 20;
        R("unprivileged setsockopt(NETLINK, SO_RCVBUFFORCE)", setsockopt(n, SOL_SOCKET, SO_RCVBUFFORCE, &big, sizeof big));
        fflush(stdout);
        _exit(0);
    }
    int st = 0;
    waitpid(c, &st, 0);
    printf("unprivileged child: exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    /* Row 27: Mojo's FIONREAD, isatty on a log file, SQLite's f2fs probe. */
    R("write 3 bytes to AF_UNIX stream", write(sv[1], "abc", 3));
    int q = -1;
    R("FIONREAD AF_UNIX stream", ioctl(sv[0], FIONREAD, &q));
    printf("FIONREAD = %d\n", q);
    close(sv[0]);
    close(sv[1]);
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-br3probe-%ld", (long)getpid());
    fd = open(path, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    char tio[64];
    R("TCGETS2 on a regular file", ioctl(fd, TCGETS2_, tio));
    unsigned feat = 0;
    R("F2FS_IOC_GET_FEATURES on a regular file", ioctl(fd, F2FS_IOC_GET_FEATURES_, &feat));
    close(fd);
    unlink(path);
}

static void acc(const char *path) {
    printf("access %s = %s\n", path, access(path, R_OK) == 0 ? "0" : sename(errno));
}

static void env(void) {
    acc("/proc/sys/kernel/yama/ptrace_scope");
    acc("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq");
    acc("/sys/devices/system/cpu/cpu0/cpu_capacity");
    acc("/sys/bus/cpu/devices/cpu0/microcode/version");
    acc("/sys/class/dmi/id/sys_vendor");
    acc("/sys/devices/virtual/dmi/id/sys_vendor");
    acc("/dev/dri");
    acc("/sys/dev/char/226:0");
    int s = socket(AF_INET6, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    struct sockaddr_in6 a = {.sin6_family = AF_INET6, .sin6_port = htons(53)};
    inet_pton(AF_INET6, "2001:4860:4860::8888", &a.sin6_addr);
    R("IPv6 UDP connect 2001:4860:4860::8888", connect(s, (struct sockaddr *)&a, sizeof a));
    close(s);
}

int main(int argc, char **argv) {
    const char *p = argc > 1 ? argv[1] : "";
    if (!strcmp(p, "pkey"))
        pkey();
    else if (!strcmp(p, "anonname"))
        anonname();
    else if (!strcmp(p, "probes"))
        probes();
    else if (!strcmp(p, "env"))
        env();
    else
        printf("usage: abi-br3probe pkey|anonname|probes|env\n");
    return 0;
}
