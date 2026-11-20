/* SELFOPT-1: perf_event_open as an unprivileged program sees it (kernel.perf_event_paranoid 2, Linux's default
 * and CachyOS's): the attribute checks, the refusals, a task-clock of the program itself (read formats, ioctls,
 * poll, the ring buffer's control page, its own user samples and a FORK record). Runs as an ordinary user; as
 * root it first becomes nobody, so both kernels answer the same caller. */
#include "abi.h"
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>

#define NR_PERF 298
#define ATTR_SIZE 144
/* attr byte offsets */
#define A_TYPE 0
#define A_SIZE 4
#define A_CONFIG 8
#define A_SAMPLE 16
#define A_STYPE 24
#define A_RFMT 32
#define A_FLAGS 40
#define A_WAKEUP 48
#define A_MAXSTACK 108
/* flag bits */
#define F_DISABLED (1ul << 0)
#define F_PINNED (1ul << 2)
#define F_INHERIT (1ul << 1)
#define F_EXCL_USER (1ul << 4)
#define F_EXCL_KERNEL (1ul << 5)
#define F_EXCL_HV (1ul << 6)
#define F_FREQ (1ul << 10)
#define F_TASK (1ul << 13)
#define F_SAMPLE_ID_ALL (1ul << 18)
#define F_EXCL_CC_KERNEL (1ul << 21)
#define F_CONTEXT_SWITCH (1ul << 26)
#define S_IP (1ul << 0)
#define S_TID (1ul << 1)
#define S_TIME (1ul << 2)
#define S_CALLCHAIN (1ul << 5)
#define S_PERIOD (1ul << 8)
#define R_ENABLED 1ul
#define R_RUNNING 2ul
#define R_ID 4ul
#define R_GROUP 8ul
#define IOC_ENABLE 0x2400
#define IOC_DISABLE 0x2401
#define IOC_RESET 0x2403
#define IOC_SET_OUTPUT 0x2405
#define IOC_MODIFY_ATTRIBUTES 0x4008240bul
#define IOC_PERIOD 0x40082404
#define IOC_ID 0x80082407
#define CTX_USER ((uint64_t)-512)

static unsigned char attr[256];

static void put64(int off, uint64_t v) { memcpy(attr + off, &v, 8); }
static void put32(int off, uint32_t v) { memcpy(attr + off, &v, 4); }

static void base(uint64_t config, uint64_t flags) {
    memset(attr, 0, sizeof attr);
    put32(A_TYPE, 1);
    put32(A_SIZE, ATTR_SIZE);
    put64(A_CONFIG, config);
    put64(A_FLAGS, flags);
}

static long po(int pid, int cpu, int group, unsigned long flags) { return syscall(NR_PERF, attr, pid, cpu, group, flags); }

static void spin_ms(int ms) {
    uint64_t end = ns_of(CLOCK_MONOTONIC) + (uint64_t)ms * 1000000u;
    volatile uint64_t x = 1;
    while (ns_of(CLOCK_MONOTONIC) < end)
        for (int i = 0; i < 10000; i++) x = x * 6364136223846793005u + i;
}

static void refusals(void) {
    base(1, F_EXCL_KERNEL | F_EXCL_HV);
    show("flags outside PERF_FLAG_ALL", po(0, -1, -1, 1ul << 4));
    put32(A_SIZE, 40);
    show("size 40", po(0, -1, -1, 0));
    uint32_t back;
    memcpy(&back, attr + A_SIZE, 4);
    printf("size written back = %u\n", back);
    base(1, F_EXCL_KERNEL | F_EXCL_HV);
    put32(A_SIZE, 160);
    attr[150] = 1;
    show("non-zero bytes past the kernel's attribute", po(0, -1, -1, 0));
    base(1, F_EXCL_KERNEL | F_EXCL_HV | (1ul << 63));
    show("reserved flag bit", po(0, -1, -1, 0));
    base(1, F_EXCL_KERNEL | F_EXCL_HV);
    put64(A_STYPE, 1ul << 40);
    show("unknown sample bit", po(0, -1, -1, 0));
    base(1, F_EXCL_KERNEL | F_EXCL_HV);
    show("pid -1, cpu -1", po(-1, -1, -1, 0));
    show("cpu 4096", po(0, 4096, -1, 0));
    show("CPU-wide, unprivileged", po(-1, 0, -1, 0));
    show("missing pid", po(4000000, -1, -1, 0));
    show("group fd 0 (no event)", po(0, -1, 0, 0));
    base(1000, F_EXCL_KERNEL | F_EXCL_HV);
    show("software config 1000", po(0, -1, -1, 0));
    base(1, 0);
    show("kernel counting, unprivileged", po(0, -1, -1, 0));
    base(1, F_EXCL_KERNEL | F_EXCL_HV | F_FREQ);
    put64(A_SAMPLE, 200000);
    show("frequency above the maximum", po(0, -1, -1, 0));
    base(1, F_EXCL_KERNEL | F_EXCL_HV);
    put64(A_SAMPLE, 1000000);
    put64(A_STYPE, S_IP | S_CALLCHAIN);
    show("kernel callchain, unprivileged", po(0, -1, -1, 0));
    put64(A_FLAGS, F_EXCL_KERNEL | F_EXCL_HV | F_EXCL_CC_KERNEL);
    attr[A_MAXSTACK] = 200;
    show("sample_max_stack above the sysctl", po(0, -1, -1, 0));
    show("bad attribute pointer", syscall(NR_PERF, KERNEL_ADDR, 0, -1, -1, 0));
    /* Permission comes before the features: EACCES, not EOPNOTSUPP or EINVAL. */
    base(1, F_EXCL_KERNEL | F_EXCL_HV | F_CONTEXT_SWITCH);
    show("CPU-wide with context_switch, unprivileged", po(-1, 0, -1, 0));
}

static void counting(void) {
    base(1, F_EXCL_KERNEL | F_EXCL_HV | F_DISABLED);
    put64(A_RFMT, R_ENABLED | R_RUNNING | R_ID);
    int fd = (int)po(0, -1, -1, 0);
    yes("task-clock of itself opens", fd >= 0);
    char link[64] = {0}, path[32];
    snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    readlink(path, link, sizeof link - 1);
    printf("link = %s\n", link);
    printf("F_GETFL = %#lx\n", (long)fcntl(fd, F_GETFL));
    uint64_t v[8] = {0};
    show("read 8 bytes (short)", read(fd, v, 8));
    show("read 64 bytes", read(fd, v, sizeof v));
    yes("a disabled event counts 0", v[0] == 0);
    show("ENABLE", ioctl(fd, IOC_ENABLE, 0));
    spin_ms(30);
    read(fd, v, sizeof v);
    yes("it counts its run time", v[0] > 10000000);
    yes("time enabled and running are set", v[1] > 10000000 && v[2] > 10000000);
    uint64_t id = 0;
    show("ID", ioctl(fd, IOC_ID, &id));
    yes("ID is the read's id", id != 0 && id == v[3]);
    show("DISABLE", ioctl(fd, IOC_DISABLE, 0));
    read(fd, v, sizeof v);
    uint64_t before = v[0];
    spin_ms(5);
    read(fd, v, sizeof v);
    yes("disabled: no more counting", v[0] == before);
    show("RESET", ioctl(fd, IOC_RESET, 0));
    read(fd, v, sizeof v);
    yes("RESET sets the count to 0", v[0] == 0);
    show("PERIOD of a counting event", ioctl(fd, IOC_PERIOD, &id));
    show("unknown ioctl", ioctl(fd, 0x247f, 0));
    show("write", write(fd, v, 8));
    show("lseek", lseek(fd, 0, SEEK_SET));
    struct pollfd p = {fd, POLLIN, 0};
    show("poll without a buffer", poll(&p, 1, 0));
    printf("revents = %#x\n", p.revents);
    close(fd);
    base(1, F_EXCL_KERNEL | F_EXCL_HV);
    put64(A_RFMT, R_GROUP | R_ID);
    fd = (int)po(0, -1, -1, 0);
    show("group read of one", read(fd, v, sizeof v));
    printf("nr = %lu\n", (unsigned long)v[0]);
    show("a group of two", po(0, -1, fd, 0));
    base(1, F_EXCL_KERNEL | F_EXCL_HV | F_DISABLED | F_PINNED);
    show("a pinned group member", po(0, -1, fd, 0));
    base(1, F_EXCL_KERNEL | F_EXCL_HV);
    show("MODIFY_ATTRIBUTES of a task-clock", ioctl(fd, IOC_MODIFY_ATTRIBUTES, attr));
    show("MODIFY_ATTRIBUTES with a bad pointer", ioctl(fd, IOC_MODIFY_ATTRIBUTES, (void *)8));
    close(fd);
    base(9, F_EXCL_KERNEL | F_EXCL_HV);
    fd = (int)po(0, -1, -1, 0);
    yes("a dummy event opens", fd >= 0);
    show("MODIFY_ATTRIBUTES of a dummy", ioctl(fd, IOC_MODIFY_ATTRIBUTES, attr));
    put32(A_TYPE, 0);
    show("MODIFY_ATTRIBUTES of a dummy with another type", ioctl(fd, IOC_MODIFY_ATTRIBUTES, attr));
    close(fd);
}

static sigjmp_buf jump;
static void on_fault(int sig) { siglongjmp(jump, sig); }

/* A store to a data page of the ring: SIGBUS (only the control page is writable). */
static void store_to_data(unsigned char *m) {
    struct sigaction sa, bus, segv;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_fault;
    sigaction(SIGBUS, &sa, &bus);
    sigaction(SIGSEGV, &sa, &segv);
    int sig = sigsetjmp(jump, 1);
    if (!sig)
        *(volatile unsigned char *)(m + 4096) = 1;
    sigaction(SIGBUS, &bus, NULL);
    sigaction(SIGSEGV, &segv, NULL);
    printf("a store to a data page = %s\n", sig == SIGBUS ? "SIGBUS" : sig == SIGSEGV ? "SIGSEGV" : "went through");
}

static void *child(void *a) { return a; }

static void ring(void) {
    base(1, F_EXCL_KERNEL | F_EXCL_HV | F_FREQ | F_DISABLED | F_TASK | F_SAMPLE_ID_ALL | F_EXCL_CC_KERNEL);
    put64(A_SAMPLE, 1000);
    put64(A_STYPE, S_IP | S_TID | S_TIME | S_PERIOD | S_CALLCHAIN);
    put32(A_WAKEUP, 1);
    int fd = (int)po(0, -1, -1, 0);
    yes("a sampling task-clock of itself opens", fd >= 0);
    yes("1 + 3 pages", mmap(NULL, 4 * 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) == MAP_FAILED);
    printf("errno = %s\n", ename(errno));
    yes("MAP_PRIVATE", mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0) == MAP_FAILED);
    printf("errno = %s\n", ename(errno));
    yes("offset 4096", mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 4096) == MAP_FAILED);
    printf("errno = %s\n", ename(errno));
    unsigned char *m = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    yes("1 + 2 pages", m != MAP_FAILED);
    if (m == MAP_FAILED)
        return;
    yes("another size afterwards", mmap(NULL, 5 * 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) == MAP_FAILED);
    printf("errno = %s\n", ename(errno));
    uint32_t u32;
    uint64_t u64;
    memcpy(&u32, m + 0, 4);
    printf("version = %u\n", u32);
    memcpy(&u32, m + 4, 4);
    printf("compat_version = %u\n", u32);
    memcpy(&u64, m + 40, 8);
    printf("cap_bit0, cap_bit0_is_deprecated = %lu, %lu\n", (unsigned long)(u64 & 1), (unsigned long)(u64 >> 1 & 1));
    memcpy(&u32, m + 72, 4);
    printf("size = %u\n", u32);
    memcpy(&u64, m + 1040, 8);
    printf("data_offset = %lu\n", (unsigned long)u64);
    memcpy(&u64, m + 1048, 8);
    printf("data_size = %lu\n", (unsigned long)u64);
    show("ENABLE", ioctl(fd, IOC_ENABLE, 0));
    pthread_t t;
    pthread_create(&t, NULL, child, NULL);
    pthread_join(t, NULL);
    spin_ms(100);
    show("DISABLE", ioctl(fd, IOC_DISABLE, 0));
    struct pollfd p = {fd, POLLIN, 0};
    show("poll after samples", poll(&p, 1, 0));
    printf("revents = %#x\n", p.revents);
    uint64_t head = *(volatile uint64_t *)(m + 1024), tail = *(volatile uint64_t *)(m + 1032);
    __sync_synchronize();
    int samples = 0, user = 0, own = 0, period = 0, mark = 0, fork = 0;
    unsigned char *data = m + 4096;
    uint64_t at = tail;
    while (at + 8 <= head) {
        unsigned char rec[1024];
        uint16_t size;
        memcpy(&size, data + (at + 6) % 8192, 2);
        if (size < 8 || size > sizeof rec)
            break;
        for (int i = 0; i < size; i++) rec[i] = data[(at + i) % 8192];
        uint32_t type, pid;
        uint16_t misc;
        memcpy(&type, rec, 4);
        memcpy(&misc, rec + 4, 2);
        if (type == 9) {
            uint64_t ip, per, nr, c0;
            memcpy(&ip, rec + 8, 8);
            memcpy(&pid, rec + 16, 4);
            memcpy(&per, rec + 32, 8);
            memcpy(&nr, rec + 40, 8);
            memcpy(&c0, rec + 48, 8);
            samples++;
            user += (misc & 7) == 2;
            own += pid == (uint32_t)getpid();
            period += per == 1000000;
            mark += nr >= 2 && c0 == CTX_USER;
        }
        if (type == 7) {
            uint32_t ppid;
            memcpy(&pid, rec + 8, 4);
            memcpy(&ppid, rec + 12, 4);
            fork += pid == (uint32_t)getpid() && ppid == (uint32_t)getpid();
        }
        at += size;
    }
    *(volatile uint64_t *)(m + 1032) = head;
    yes("samples", samples > 20);
    yes("all in user mode", samples > 0 && user == samples);
    yes("all of this process", samples > 0 && own == samples);
    yes("period 1000000 ns", samples > 0 && period == samples);
    yes("callchains start with the user mark", samples > 0 && mark == samples);
    yes("a FORK record for the new thread", fork == 1);
    show("SET_OUTPUT to itself", ioctl(fd, IOC_SET_OUTPUT, fd));
    show("SET_OUTPUT -1 while mapped", ioctl(fd, IOC_SET_OUTPUT, -1L));
    show("SET_OUTPUT 0xffffffff (no fd)", ioctl(fd, IOC_SET_OUTPUT, 0xfffffffful));
    base(1, F_EXCL_KERNEL | F_EXCL_HV | F_DISABLED);
    int other = (int)po(0, -1, -1, 0);
    show("SET_OUTPUT of another event of this task into the buffer", ioctl(other, IOC_SET_OUTPUT, fd));
    close(other);
    store_to_data(m);
    munmap(m, 3 * 4096);
    close(fd);
}

int main(void) {
    if (geteuid() == 0) {
        setgid(65534);
        setuid(65534);
    }
    char b[16] = {0};
    FILE *f = fopen("/proc/sys/kernel/perf_event_paranoid", "r");
    if (f) {
        fgets(b, sizeof b, f);
        fclose(f);
    }
    printf("perf_event_paranoid = %s", b);
    f = fopen("/proc/kallsyms", "r");
    char line[256] = {0};
    if (f) {
        fgets(line, sizeof line, f);
        fclose(f);
    }
    yes("kallsyms hides addresses", strncmp(line, "0000000000000000 ", 17) == 0);
    refusals();
    counting();
    ring();
    return 0;
}
