/* AB-17: process_vm_readv / process_vm_writev. Data across page edges and segments between a parent and its
 * forked child (the same addresses on both sides after fork), short counts, Linux's error order (flags, local
 * iovecs, an empty side, remote iovecs, pid, permission), a non-dumpable child (EPERM), a zombie (ESRCH), a
 * thread id, and a write into a private file mapping that must not reach the file. Only the parent reads its
 * child (allowed with Yama's ptrace_scope 1 too); run as root, it first becomes nobody (so EPERM is EPERM). */
#include "abi.h"
#include <fcntl.h>
#include <pthread.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/uio.h>
#include <sys/wait.h>

#define NO_PID 0x3ffffff0
#define PG 4096

static char area[4 * PG] __attribute__((aligned(PG)));
static char into[4 * PG] __attribute__((aligned(PG)));
static int to_child[2], to_parent[2];

static long rd(pid_t pid, const struct iovec *l, unsigned long ln, const struct iovec *r, unsigned long rn, unsigned long fl) {
    return syscall(SYS_process_vm_readv, pid, l, ln, r, rn, fl);
}

static void fill(char *p, int n, int seed) {
    for (int i = 0; i < n; i++)
        p[i] = (char)(i * 31 + seed);
}
static int same(const char *p, int n, int seed) {
    for (int i = 0; i < n; i++)
        if (p[i] != (char)(i * 31 + seed))
            return 0;
    return 1;
}

/* The child: answers one command byte at a time until 'q'. */
static void child_loop(int map_fd) {
    char *rw = mmap(0, PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, map_fd, 0);
    char *ro = mmap(0, PG, PROT_READ, MAP_PRIVATE, map_fd, 0);
    char c;
    while (read(to_child[0], &c, 1) == 1) {
        char out = 'n';
        if (c == 'a') { /* addresses of the file maps */
            write(to_parent[1], &rw, sizeof rw);
            write(to_parent[1], &ro, sizeof ro);
            continue;
        }
        if (c == 'w') /* the parent wrote seed 9 at area+100 */
            out = same(area + 100, 3 * PG, 9) ? 'y' : 'n';
        if (c == 'm') /* the private map after the parent's write */
            out = memcmp(rw, "ooooNEWBYTES", 12) == 0 && ro[0] == 'o' ? 'y' : 'n';
        if (c == 'd')
            out = prctl(PR_SET_DUMPABLE, 0) == 0 ? 'y' : 'n';
        if (c == 'q')
            _exit(0);
        write(to_parent[1], &out, 1);
    }
    _exit(1);
}

static char ask(char c) {
    char r = '?';
    write(to_child[1], &c, 1);
    read(to_parent[0], &r, 1);
    return r;
}

static void *thread_main(void *arg) {
    *(pid_t *)arg = (pid_t)syscall(SYS_gettid);
    return 0;
}
static volatile pid_t live_tid;
static volatile int stop_thread;
static void *spin_thread(void *arg) {
    (void)arg;
    live_tid = (pid_t)syscall(SYS_gettid);
    while (!stop_thread)
        usleep(1000);
    return 0;
}

/* A process whose leader thread ended while another thread runs: the leader's pid names a task without
 * memory (ESRCH), the running thread's id still works. */
static pid_t leader;
static void *after_leader(void *arg) {
    (void)arg;
    struct iovec l = {into, 16}, r = {area, 16};
    long res = 16;
    for (int i = 0; i < 3000 && res >= 0; i++)
        if ((res = rd(leader, &l, 1, &r, 1, 0)) >= 0)
            usleep(1000);
    yes("an ended leader's pid fails with ESRCH", res < 0 && errno == ESRCH);
    SYS("the running thread's id", SYS_process_vm_readv, syscall(SYS_gettid), &l, 1, &r, 1, 0);
    _exit(0);
}
static void ended_leader(void) {
    pid_t p = fork();
    if (p == 0) {
        leader = getpid();
        pthread_t th;
        if (pthread_create(&th, 0, after_leader, 0))
            _exit(1);
        syscall(SYS_exit, 0);
    }
    waitpid(p, 0, 0);
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    if (geteuid() == 0 && (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))) {
        printf("cannot drop root\n");
        return 1;
    }
    prctl(PR_SET_DUMPABLE, 1);
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-procvm-%d", getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    char o64[64];
    memset(o64, 'o', sizeof o64);
    write(fd, o64, sizeof o64);
    fill(area, sizeof area, 5);
    if (pipe(to_child) || pipe(to_parent))
        return 1;
    pid_t pid = fork();
    if (pid == 0)
        child_loop(fd);
    char *crw, *cro;
    write(to_child[1], "a", 1);
    read(to_parent[0], &crw, sizeof crw);
    read(to_parent[0], &cro, sizeof cro);

    /* Data: 3 pages from the child at an odd offset into two local segments that cross page edges. */
    memset(into, 0, sizeof into);
    struct iovec l2[2] = {{into + 1001, 5000}, {into + 6101, 3 * PG - 5000}};
    struct iovec r3[3] = {{area + 100, 3}, {area, 0}, {area + 103, 3 * PG - 3}};
    SYS("readv 3 pages", SYS_process_vm_readv, pid, l2, 2, r3, 3, 0);
    char cat[3 * PG];
    memcpy(cat, into + 1001, 5000);
    memcpy(cat + 5000, into + 6101, 3 * PG - 5000);
    char want[3 * PG];
    memcpy(want, area + 100, sizeof want);
    yes("readv data", memcmp(cat, want, sizeof cat) == 0);
    /* writev seed 9 back to area+100 in the child. */
    fill(cat, sizeof cat, 9);
    memcpy(into + 1001, cat, 5000);
    memcpy(into + 6101, cat + 5000, 3 * PG - 5000);
    struct iovec w1 = {area + 100, 3 * PG};
    SYS("writev 3 pages", SYS_process_vm_writev, pid, l2, 2, &w1, 1, 0);
    printf("child sees the bytes: %c\n", ask('w'));
    yes("own area unchanged", same(area, sizeof area, 5));

    /* Sizes: remote longer than local, a short count, nothing moved. */
    struct iovec l16 = {into, 16};
    struct iovec big = {area, 1ul << 40};
    SYS("remote longer than local", SYS_process_vm_readv, pid, &l16, 1, &big, 1, 0);
    struct iovec rs[2] = {{area, 8}, {KERNEL_ADDR, 8}};
    SYS("short (second remote faults)", SYS_process_vm_readv, pid, &l16, 1, rs, 2, 0);
    struct iovec rk = {KERNEL_ADDR, 16};
    SYS("remote kernel address", SYS_process_vm_readv, pid, &l16, 1, &rk, 1, 0);
    struct iovec ls[2] = {{into, 8}, {KERNEL_ADDR, 8}};
    struct iovec r16 = {area, 16};
    SYS("local segment fails access_ok", SYS_process_vm_readv, pid, ls, 2, &r16, 1, 0);
    char *lro = mmap(0, PG, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    struct iovec lr = {lro, 16};
    SYS("read into a read-only local page", SYS_process_vm_readv, pid, &lr, 1, &r16, 1, 0);
    struct iovec lpart[2] = {{into, 8}, {lro, 8}};
    SYS("read: local page fails after 8", SYS_process_vm_readv, pid, lpart, 2, &r16, 1, 0);

    /* The error order. */
    SYS("flags 1", SYS_process_vm_readv, pid, &l16, 1, &r16, 1, 1);
    SYS("flags before bad local", SYS_process_vm_readv, NO_PID, KERNEL_ADDR, 1, &r16, 1, 1);
    SYS("local array", SYS_process_vm_readv, NO_PID, KERNEL_ADDR, 1, &r16, 1, 0);
    SYS("local array (2)", SYS_process_vm_readv, NO_PID, KERNEL_ADDR, 2, &r16, 1, 0);
    SYS("local count 1025", SYS_process_vm_readv, NO_PID, &l16, 1025, &r16, 1, 0);
    SYS("local count 2^32 (an unsigned int)", SYS_process_vm_readv, NO_PID, &l16, 1ul << 32, &r16, 1, 0);
    struct iovec lneg = {into, 1ul << 63};
    SYS("negative local length", SYS_process_vm_readv, NO_PID, &lneg, 1, &r16, 1, 0);
    struct iovec lempty[2] = {{into, 0}, {KERNEL_ADDR, 0}};
    SYS("empty local (bad pid, bad remote)", SYS_process_vm_readv, NO_PID, lempty, 2, KERNEL_ADDR, 5000, 0);
    SYS("no local", SYS_process_vm_readv, NO_PID, &l16, 0, KERNEL_ADDR, 1, 0);
    SYS("remote array", SYS_process_vm_readv, NO_PID, &l16, 1, KERNEL_ADDR, 1, 0);
    SYS("remote count 1025", SYS_process_vm_readv, NO_PID, &l16, 1, &r16, 1025, 0);
    struct iovec rneg = {area, 1ul << 63};
    SYS("negative remote length", SYS_process_vm_readv, NO_PID, &l16, 1, &rneg, 1, 0);
    struct iovec rempty = {KERNEL_ADDR, 0};
    SYS("empty remote (bad pid)", SYS_process_vm_readv, NO_PID, &l16, 1, &rempty, 1, 0);
    SYS("no remote (bad pid)", SYS_process_vm_writev, NO_PID, &l16, 1, &r16, 0, 0);
    SYS("no such pid", SYS_process_vm_readv, NO_PID, &l16, 1, &r16, 1, 0);
    SYS("pid 0", SYS_process_vm_readv, 0, &l16, 1, &r16, 1, 0);
    SYS("pid -1", SYS_process_vm_writev, -1, &l16, 1, &r16, 1, 0);

    /* Its own memory and a thread id. */
    struct iovec own = {area, 16};
    SYS("own pid", SYS_process_vm_readv, getpid(), &l16, 1, &own, 1, 0);
    yes("own bytes", memcmp(into, area, 16) == 0);
    pthread_t th;
    if (pthread_create(&th, 0, spin_thread, 0) == 0) {
        while (!live_tid)
            usleep(1000);
        SYS("a live thread's id", SYS_process_vm_readv, live_tid, &l16, 1, &own, 1, 0);
        stop_thread = 1;
        pthread_join(th, 0);
    }
    pid_t tid = 0;
    if (pthread_create(&th, 0, thread_main, &tid) == 0) {
        pthread_join(th, 0);
        long r = rd(tid, &l16, 1, &own, 1, 0);
        yes("an ended thread's id fails with ESRCH", r < 0 && errno == ESRCH);
    }
    ended_leader();

    /* A private file mapping of the child: the write stays in the mapping. */
    memcpy(into, "NEWBYTES", 8);
    struct iovec l8 = {into, 8};
    struct iovec pm = {crw + 4, 8};
    SYS("writev into a private file map", SYS_process_vm_writev, pid, &l8, 1, &pm, 1, 0);
    struct iovec pro = {cro, 8};
    SYS("writev into a read-only private file map", SYS_process_vm_writev, pid, &l8, 1, &pro, 1, 0);
    SYS("readv from it", SYS_process_vm_readv, pid, &l8, 1, &pro, 1, 0);
    printf("child's maps: %c\n", ask('m'));
    char back[64];
    pread(fd, back, sizeof back, 0);
    yes("the file is unchanged", memcmp(back, o64, sizeof back) == 0);

    /* Not dumpable: EPERM before anything is copied. */
    printf("child undumpable: %c\n", ask('d'));
    SYS("readv undumpable", SYS_process_vm_readv, pid, &l16, 1, &r16, 1, 0);
    SYS("writev undumpable", SYS_process_vm_writev, pid, &l16, 1, &r16, 1, 0);
    SYS("undumpable, kernel remote address", SYS_process_vm_readv, pid, &l16, 1, &rk, 1, 0);
    SYS("undumpable, empty remote", SYS_process_vm_readv, pid, &l16, 1, &rempty, 1, 0);

    /* A zombie (ended, not yet collected), then collected. */
    write(to_child[1], "q", 1);
    siginfo_t si;
    waitid(P_PID, pid, &si, WEXITED | WNOWAIT);
    SYS("zombie", SYS_process_vm_readv, pid, &l16, 1, &r16, 1, 0);
    waitpid(pid, 0, 0);
    close(fd);
    unlink(path);
    return 0;
}
