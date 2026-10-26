/* Wave A: poll and ppoll. stdin is a pipe whose writer wrote everything and closed (cases.txt gives the
 * bytes), stdout and stderr are pipes the harness reads. */
#include "abi.h"
#include <fcntl.h>
#include <poll.h>
#include <signal.h>

static void entries(void) {
    int null = open("/dev/null", O_RDWR);
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-poll-%d", (int)getpid());
    int file = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    struct pollfd p[8] = {
        { 0, POLLIN, 0x5555 },        /* stdin with bytes left and no writer */
        { 1, POLLOUT, 0x5555 },       /* stdout pipe */
        { null, 0xffff, 0x5555 },     /* /dev/null asked for everything */
        { file, 0xffff, 0x5555 },     /* a tmpfs file */
        { -1, 0xffff, 0x5555 },       /* skipped */
        { 77, POLLIN, 0x5555 },       /* not open */
        { 1, 0, 0x5555 },             /* asked for nothing */
        { 2, POLLIN, 0x5555 },        /* stderr pipe asked for input it never has */
    };
    SYS("poll(8 entries, 0)", SYS_poll, p, 8, 0);
    for (int i = 0; i < 8; i++)
        printf("  entry %d: events %#x revents %#x\n", i, (unsigned short)p[i].events, (unsigned short)p[i].revents);
    char b[64];
    long n = read(0, b, sizeof b);
    printf("read stdin: %ld bytes\n", n);
    struct pollfd e = { 0, POLLIN, 0 };
    SYS("poll(stdin at end of file)", SYS_poll, &e, 1, 0);
    printf("  revents %#x\n", (unsigned short)e.revents);
    e = (struct pollfd){ 0, POLLIN | POLLRDHUP, 0 };
    SYS("poll(stdin, POLLIN|POLLRDHUP)", SYS_poll, &e, 1, 0);
    printf("  revents %#x\n", (unsigned short)e.revents);
    close(file);
    unlink(path);
    close(null);
}

static void waits(void) {
    struct pollfd none = { -1, POLLIN, 0 };
    SYS("poll(nothing ready, 0)", SYS_poll, &none, 1, 0);
    uint64_t t0 = ns_of(CLOCK_MONOTONIC);
    SYS("poll(nothing ready, 50 ms)", SYS_poll, &none, 1, 50);
    yes("waited at least 50 ms", ns_of(CLOCK_MONOTONIC) - t0 >= 50000000);
    t0 = ns_of(CLOCK_MONOTONIC);
    SYS("poll(NULL, 0, 30 ms)", SYS_poll, NULL, 0, 30);
    yes("waited at least 30 ms", ns_of(CLOCK_MONOTONIC) - t0 >= 30000000);
    struct pollfd out = { 1, POLLOUT, 0 };
    SYS("poll(stdout, -1)", SYS_poll, &out, 1, -1);
    SYS("poll(bad array)", SYS_poll, KERNEL_ADDR, 1, 0);
    SYS("poll(unmapped array)", SYS_poll, (void *)0x10, 1, 0);
    /* More entries than RLIMIT_NOFILE allows (lowered to 64 first, as the hard limit may differ). */
    struct { unsigned long cur, max; } lim = { 64, 64 };
    syscall(SYS_prlimit64, 0, 7, &lim, 0);
    static struct pollfd many[65];
    for (int i = 0; i < 65; i++)
        many[i] = (struct pollfd){ -1, 0, 0 };
    SYS("poll(65 entries, RLIMIT_NOFILE 64)", SYS_poll, many, 65, 0);
    SYS("poll(64 entries, RLIMIT_NOFILE 64)", SYS_poll, many, 64, 0);
}

static void ppolls(void) {
    struct pollfd none = { -1, POLLIN, 0 };
    struct timespec ts = { 0, 1000000000 };
    SYS("ppoll(nsec 1e9)", SYS_ppoll, &none, 1, &ts, 0, 8);
    ts = (struct timespec){ -1, 0 };
    SYS("ppoll(sec -1)", SYS_ppoll, &none, 1, &ts, 0, 8);
    SYS("ppoll(bad timespec)", SYS_ppoll, &none, 1, KERNEL_ADDR, 0, 8);
    unsigned long mask = 1ul << (SIGUSR1 - 1);
    ts = (struct timespec){ 0, 30000000 };
    SYS("ppoll(sigsetsize 4)", SYS_ppoll, &none, 1, &ts, &mask, 4);
    SYS("ppoll(bad sigmask)", SYS_ppoll, &none, 1, &ts, KERNEL_ADDR, 8);
    ts = (struct timespec){ 0, 30000000 };
    uint64_t t0 = ns_of(CLOCK_MONOTONIC);
    SYS("ppoll(nothing ready, 30 ms, a mask)", SYS_ppoll, &none, 1, &ts, &mask, 8);
    yes("waited at least 30 ms", ns_of(CLOCK_MONOTONIC) - t0 >= 30000000);
    yes("time left written back and below 30 ms", ts.tv_sec == 0 && ts.tv_nsec >= 0 && ts.tv_nsec < 30000000);
    unsigned long now = 0x55;
    syscall(SYS_rt_sigprocmask, SIG_BLOCK, 0, &now, 8);
    printf("mask after ppoll %#lx\n", now);
    struct pollfd out = { 1, POLLOUT, 0 };
    ts = (struct timespec){ 5, 0 };
    SYS("ppoll(stdout, 5 s)", SYS_ppoll, &out, 1, &ts, 0, 0);
    yes("time left between 4 and 5 s", ts.tv_sec == 4 || (ts.tv_sec == 5 && ts.tv_nsec == 0));
    ts = (struct timespec){ 0, 0 };
    SYS("ppoll(stdout, zero timeout)", SYS_ppoll, &out, 1, &ts, 0, 0);
    printf("zero timeout kept: %d %ld\n", (int)ts.tv_sec, ts.tv_nsec);
    SYS("ppoll(stdout, NULL timeout)", SYS_ppoll, &out, 1, 0, 0, 0);
}

int main(void) {
    entries();
    waits();
    ppolls();
    return 0;
}
