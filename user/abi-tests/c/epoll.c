/* fs-epoll: poll, ppoll, select, pselect6 and epoll (FS-4) as programs see them, for the ABI diff (CachyOS is
 * the oracle; user/abi-tests/c/epoll.c is the same file) and the ktests fs.epoll.* (a program that sleeps in a
 * wait until the kernel writes into its stdin). Prints results, errno names and yes/no relations only. Raw
 * syscalls, so the answers are the kernel's own. Modes (argv[1]):
 *   basic     epoll_create/create1/ctl/wait argument rules, the epoll file itself, EPERM for files without poll
 *   modes     level-triggered, EPOLLET, EPOLLONESHOT, the order of ready items, maxevents, masks, MOD, data
 *   close     registrations belong to the open file description: dup, close, DEL by either number
 *   nested    epoll in epoll (readiness, wake-ups, loops ELOOP, chains deeper than EP_MAX_NESTS)
 *   flags     EPOLLEXCLUSIVE rules, EPOLLWAKEUP, EPOLLRDHUP, EPOLLPRI
 *   timeouts  epoll_wait/epoll_pwait/epoll_pwait2 timeouts and signal masks, ppoll/pselect6 masks
 *   select    select and pselect6: sets, EBADF, the fd-table cap, the time written back, argument rules
 *   poll      poll masks of eventfd, epoll, /dev/random, a pipe end asked for the other direction
 *   waiter K  waits with K (epoll, epoll-et, poll, ppoll, select, pselect) for stdin without a timeout, reads
 *             what came, until end of file: "K got <bytes> sum <checksum>" (ktests write into the pipe later)
 *   sigwait   (INT-A2a, FS-4 CR8) a handled SIGUSR1 sent by a forked child ends a blocked epoll_wait,
 *             epoll_pwait, epoll_pwait2, poll, ppoll, select and pselect6 with EINTR, with and without SA_RESTART
 *             (none of them restarts after a handler); ppoll/pselect6/epoll_pwait with a temporary mask: the
 *             handler runs under the temporary mask and the old mask is back afterwards
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_epoll_pwait2
#define SYS_epoll_pwait2 441
#endif
#ifndef EPOLLEXCLUSIVE
#define EPOLLEXCLUSIVE (1u << 28)
#endif
#define KADDR ((void *)0xffff888000000000ul)

static const char *ename(int e) {
    static char other[24];
    switch (e) {
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case EINTR: return "EINTR";
    case EBADF: return "EBADF";
    case EAGAIN: return "EAGAIN";
    case ENOMEM: return "ENOMEM";
    case EFAULT: return "EFAULT";
    case EEXIST: return "EEXIST";
    case EINVAL: return "EINVAL";
    case EMFILE: return "EMFILE";
    case ENOTTY: return "ENOTTY";
    case ESPIPE: return "ESPIPE";
    case ELOOP: return "ELOOP";
    case ENOSYS: return "ENOSYS";
    }
    snprintf(other, sizeof other, "errno %d", e);
    return other;
}

static long show(const char *label, long r) {
    if (r == -1) printf("%s = -%s\n", label, ename(errno));
    else printf("%s = %ld\n", label, r);
    return r;
}
#define SYS(label, ...) show(label, syscall(__VA_ARGS__))

static void yes(const char *label, int c) {
    printf("%s: %s\n", label, c ? "yes" : "no");
}

static uint64_t mono_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
}

static int ep_create(void) {
    return (int)syscall(SYS_epoll_create1, 0);
}

static long ctl(int ep, int op, int fd, uint32_t events, uint64_t data) {
    struct epoll_event e;
    memset(&e, 0, sizeof e);
    e.events = events;
    e.data.u64 = data;
    return syscall(SYS_epoll_ctl, ep, op, fd, &e);
}

/* "label = n: [events/data ...]" for one epoll_wait. */
static long wait_show(const char *label, int ep, int max, int timeout) {
    struct epoll_event ev[16];
    memset(ev, 0x5a, sizeof ev);
    long n = syscall(SYS_epoll_wait, ep, ev, max, timeout);
    if (n < 0) {
        printf("%s = -%s\n", label, ename(errno));
        return n;
    }
    printf("%s = %ld:", label, n);
    for (long i = 0; i < n; i++)
        printf(" %#x/%llu", ev[i].events, (unsigned long long)ev[i].data.u64);
    printf("\n");
    return n;
}

static void pipe_nb(int p[2]) {
    if (syscall(SYS_pipe2, p, O_NONBLOCK) != 0) {
        printf("pipe2 failed\n");
        exit(1);
    }
}

static void drain(int fd) {
    char b[4096];
    while (read(fd, b, sizeof b) > 0) {
    }
}

static int revents(int fd, short events) {
    struct pollfd p = {fd, events, 0};
    long r = syscall(SYS_poll, &p, 1, 0);
    return r < 0 ? -errno : p.revents;
}

/* ---- basic ---- */

static void basic(void) {
    SYS("epoll_create(0)", SYS_epoll_create, 0);
    SYS("epoll_create(-1)", SYS_epoll_create, -1);
    int a = (int)SYS("epoll_create(1) is fd 3", SYS_epoll_create, 1);
    SYS("epoll_create1(0x1)", SYS_epoll_create1, 1);
    SYS("epoll_create1(O_NONBLOCK)", SYS_epoll_create1, O_NONBLOCK);
    int b = (int)SYS("epoll_create1(EPOLL_CLOEXEC) is fd 4", SYS_epoll_create1, EPOLL_CLOEXEC);
    printf("F_GETFD create1(CLOEXEC) %d, create(1) %d\n", fcntl(b, F_GETFD), fcntl(a, F_GETFD));
    printf("F_GETFL %#o\n", fcntl(b, F_GETFL));
    struct stat st;
    fstat(b, &st);
    printf("fstat mode %#o nlink %lu\n", st.st_mode, (unsigned long)st.st_nlink);
    char buf[16];
    SYS("read(epoll)", SYS_read, b, buf, sizeof buf);
    SYS("write(epoll)", SYS_write, b, buf, 8);
    SYS("lseek(epoll, 5, SEEK_SET)", SYS_lseek, b, 5, SEEK_SET);

    int p[2];
    pipe_nb(p);
    struct epoll_event e = {EPOLLIN, {.u64 = 1}};
    SYS("ctl(bad epfd)", SYS_epoll_ctl, 99, EPOLL_CTL_ADD, p[0], &e);
    SYS("ctl(bad fd)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, 99, &e);
    SYS("ctl(negative fd)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, -1, &e);
    SYS("ctl(epfd not an epoll)", SYS_epoll_ctl, p[1], EPOLL_CTL_ADD, p[0], &e);
    SYS("ctl(add itself)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, b, &e);
    SYS("ctl(event in kernel memory)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, p[0], KADDR);
    SYS("ctl(DEL, event in kernel memory, not there)", SYS_epoll_ctl, b, EPOLL_CTL_DEL, p[0], KADDR);
    SYS("ctl(bad op 0)", SYS_epoll_ctl, b, 0, p[0], &e);
    SYS("ctl(bad op 4)", SYS_epoll_ctl, b, 4, p[0], &e);
    SYS("ctl(MOD, not there)", SYS_epoll_ctl, b, EPOLL_CTL_MOD, p[0], &e);
    SYS("ctl(DEL, not there)", SYS_epoll_ctl, b, EPOLL_CTL_DEL, p[0], NULL);
    SYS("ctl(ADD)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, p[0], &e);
    SYS("ctl(ADD again)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, p[0], &e);
    SYS("ctl(bad op 4, there)", SYS_epoll_ctl, b, 4, p[0], &e);
    SYS("ctl(DEL, NULL event)", SYS_epoll_ctl, b, EPOLL_CTL_DEL, p[0], NULL);
    SYS("ctl(DEL again)", SYS_epoll_ctl, b, EPOLL_CTL_DEL, p[0], NULL);

    char fpath[64];
    snprintf(fpath, sizeof fpath, "/tmp/fs-epoll-%d", (int)getpid());
    int f = open(fpath, O_RDWR | O_CREAT | O_TRUNC, 0600);
    SYS("ctl(ADD regular file)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, f, &e);
    SYS("ctl(DEL regular file)", SYS_epoll_ctl, b, EPOLL_CTL_DEL, f, NULL);
    int d = open("/tmp", O_RDONLY | O_DIRECTORY);
    SYS("ctl(ADD directory)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, d, &e);
    int null = open("/dev/null", O_RDWR);
    SYS("ctl(ADD /dev/null)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, null, &e);
    int zero = open("/dev/zero", O_RDONLY);
    SYS("ctl(ADD /dev/zero)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, zero, &e);
    int ur = open("/dev/urandom", O_RDONLY);
    SYS("ctl(ADD /dev/urandom)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, ur, &e);
    int rnd = open("/dev/random", O_RDONLY);
    SYS("ctl(ADD /dev/random)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, rnd, &e);
    wait_show("wait(/dev/random)", b, 4, 0);
    SYS("ctl(DEL /dev/random)", SYS_epoll_ctl, b, EPOLL_CTL_DEL, rnd, NULL);
    int opath = open("/tmp", O_PATH);
    SYS("ctl(ADD O_PATH fd)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, opath, &e);
    SYS("ctl(O_PATH epfd)", SYS_epoll_ctl, opath, EPOLL_CTL_ADD, p[0], &e);
    int efd = (int)syscall(SYS_eventfd2, 0, 0);
    SYS("ctl(ADD eventfd)", SYS_epoll_ctl, b, EPOLL_CTL_ADD, efd, &e);

    struct epoll_event out[4];
    SYS("wait(maxevents 0)", SYS_epoll_wait, b, out, 0, 0);
    SYS("wait(maxevents -1)", SYS_epoll_wait, b, out, -1, 0);
    SYS("wait(maxevents INT_MAX)", SYS_epoll_wait, b, out, 0x7fffffff, 0);
    SYS("wait(maxevents INT_MAX/12+1)", SYS_epoll_wait, b, out, 0x7fffffff / 12 + 1, 0);
    SYS("wait(events in kernel memory)", SYS_epoll_wait, b, KADDR, 1, 0);
    SYS("wait(bad epfd)", SYS_epoll_wait, 99, out, 1, 0);
    SYS("wait(not an epoll)", SYS_epoll_wait, p[0], out, 1, 0);
    SYS("wait(events NULL, nothing ready)", SYS_epoll_wait, b, NULL, 1, 0);
    uint64_t one = 1;
    write(efd, &one, 8);
    SYS("wait(events NULL, one ready)", SYS_epoll_wait, b, NULL, 1, 0);
    SYS("wait(events unmapped, one ready)", SYS_epoll_wait, b, (void *)0x1000, 1, 0);
    wait_show("wait(after the faults)", b, 4, 0);
    SYS("epoll_pwait(sigsetsize 4)", SYS_epoll_pwait, b, out, 1, 0, &one, 4);
    SYS("epoll_pwait(mask in kernel memory)", SYS_epoll_pwait, b, out, 1, 0, KADDR, 8);
    SYS("epoll_pwait(NULL mask, sigsetsize 3)", SYS_epoll_pwait, b, out, 1, 0, NULL, 3);
    close(f);
    unlink(fpath);
    close(d);
    close(null);
    close(zero);
    close(ur);
    close(rnd);
    close(opath);
    close(efd);
    close(p[0]);
    close(p[1]);
    close(a);
    close(b);
}

/* ---- modes ---- */

static void modes(void) {
    int ep = ep_create();
    int a[2], b[2], c[2];
    pipe_nb(a);
    pipe_nb(b);
    pipe_nb(c);
    /* Level-triggered: stays ready until read. */
    show("ctl(ADD a LT)", ctl(ep, EPOLL_CTL_ADD, a[0], EPOLLIN, 10));
    wait_show("LT nothing", ep, 8, 0);
    write(a[1], "x", 1);
    wait_show("LT a", ep, 8, 0);
    wait_show("LT a again", ep, 8, 0);
    drain(a[0]);
    wait_show("LT a drained", ep, 8, 0);
    /* EPOLLET: one report per wake (every write wakes a pipe's readers). */
    show("ctl(ADD b ET)", ctl(ep, EPOLL_CTL_ADD, b[0], EPOLLIN | EPOLLET, 20));
    write(b[1], "y", 1);
    wait_show("ET b", ep, 8, 0);
    wait_show("ET b again", ep, 8, 0);
    write(b[1], "y", 1);
    wait_show("ET b second write", ep, 8, 0);
    drain(b[0]);
    wait_show("ET b drained", ep, 8, 0);
    /* Registered after the event: still reported once, also with EPOLLET. */
    write(c[1], "z", 1);
    show("ctl(ADD c ET, already readable)", ctl(ep, EPOLL_CTL_ADD, c[0], EPOLLIN | EPOLLET, 30));
    wait_show("ET c added ready", ep, 8, 0);
    wait_show("ET c again", ep, 8, 0);
    /* MOD re-arms an ET item that is ready now. */
    show("ctl(MOD c ET)", ctl(ep, EPOLL_CTL_MOD, c[0], EPOLLIN | EPOLLET, 31));
    wait_show("ET c after MOD", ep, 8, 0);
    drain(c[0]);
    /* EPOLLONESHOT: one report, then disabled until MOD. */
    show("ctl(MOD a ONESHOT)", ctl(ep, EPOLL_CTL_MOD, a[0], EPOLLIN | EPOLLONESHOT, 11));
    write(a[1], "x", 1);
    wait_show("ONESHOT a", ep, 8, 0);
    write(a[1], "x", 1);
    wait_show("ONESHOT a disabled", ep, 8, 0);
    show("ctl(MOD a ONESHOT again)", ctl(ep, EPOLL_CTL_MOD, a[0], EPOLLIN | EPOLLONESHOT, 12));
    wait_show("ONESHOT a re-armed", ep, 8, 0);
    show("ctl(MOD a LT)", ctl(ep, EPOLL_CTL_MOD, a[0], EPOLLIN, 13));
    /* Order: level-triggered items go to the end after each report; maxevents takes from the front. */
    write(b[1], "y", 1);
    write(c[1], "z", 1);
    show("ctl(MOD b LT)", ctl(ep, EPOLL_CTL_MOD, b[0], EPOLLIN, 21));
    show("ctl(MOD c LT)", ctl(ep, EPOLL_CTL_MOD, c[0], EPOLLIN, 32));
    wait_show("three ready", ep, 8, 0);
    wait_show("max 1 (1st)", ep, 1, 0);
    wait_show("max 1 (2nd)", ep, 1, 0);
    wait_show("max 1 (3rd)", ep, 1, 0);
    wait_show("max 2", ep, 2, 0);
    wait_show("max 2 again", ep, 2, 0);
    drain(a[0]);
    drain(b[0]);
    drain(c[0]);
    wait_show("all drained", ep, 8, 0);
    /* Masks: the write end asked for IN and OUT, HUP after the writer goes, ERR after the reader goes. */
    show("ctl(ADD a write end IN|OUT)", ctl(ep, EPOLL_CTL_ADD, a[1], EPOLLIN | EPOLLOUT, 14));
    wait_show("write end", ep, 8, 0);
    show("ctl(MOD a write end IN|OUT|RDNORM|WRNORM)", ctl(ep, EPOLL_CTL_MOD, a[1], EPOLLIN | EPOLLOUT | EPOLLRDNORM | EPOLLWRNORM, 15));
    wait_show("write end with NORM bits", ep, 8, 0);
    show("ctl(MOD a write end 0)", ctl(ep, EPOLL_CTL_MOD, a[1], 0, 16));
    wait_show("write end, no events asked", ep, 8, 0);
    show("ctl(MOD a read end RDNORM)", ctl(ep, EPOLL_CTL_MOD, a[0], EPOLLRDNORM, 17));
    write(a[1], "x", 1);
    wait_show("read end RDNORM only", ep, 8, 0);
    drain(a[0]);
    show("ctl(MOD a read end 0)", ctl(ep, EPOLL_CTL_MOD, a[0], 0, 18));
    close(a[1]);
    wait_show("writer gone, read end asked 0 (HUP comes anyway)", ep, 8, 0);
    close(b[0]);
    show("ctl(ADD b write end OUT)", ctl(ep, EPOLL_CTL_ADD, b[1], EPOLLOUT, 22));
    wait_show("reader gone, write end", ep, 8, 0);
    /* Data: all 64 bits come back. */
    int e2 = ep_create();
    int efd = (int)syscall(SYS_eventfd2, 3, 0);
    show("ctl(ADD eventfd, data ~0)", ctl(e2, EPOLL_CTL_ADD, efd, EPOLLIN | EPOLLOUT, ~0ull));
    wait_show("eventfd", e2, 8, 0);
    uint64_t v;
    read(efd, &v, 8);
    wait_show("eventfd read", e2, 8, 0);
    v = 0xfffffffffffffffeull;
    write(efd, &v, 8);
    wait_show("eventfd at max-1", e2, 8, 0);
    /* An eventfd write wakes with EPOLLIN only: an ET item for OUT does not see it. */
    read(efd, &v, 8);
    show("ctl(MOD eventfd OUT|ET)", ctl(e2, EPOLL_CTL_MOD, efd, EPOLLOUT | EPOLLET, 5));
    wait_show("eventfd OUT|ET first", e2, 8, 0);
    v = 1;
    write(efd, &v, 8);
    wait_show("eventfd OUT|ET after a write", e2, 8, 0);
    read(efd, &v, 8);
    wait_show("eventfd OUT|ET after a read", e2, 8, 0);
    close(efd);
    close(e2);
    close(a[0]);
    close(b[1]);
    close(c[0]);
    close(c[1]);
    close(ep);
}

/* ---- close ---- */

static void closes(void) {
    int ep = ep_create();
    int p[2];
    pipe_nb(p);
    int d = dup(p[0]);
    show("ctl(ADD read end)", ctl(ep, EPOLL_CTL_ADD, p[0], EPOLLIN, 1));
    write(p[1], "x", 1);
    wait_show("ready", ep, 8, 0);
    close(p[0]);
    wait_show("its fd closed, a dup keeps the file", ep, 8, 0);
    show("ctl(DEL by the closed number)", ctl(ep, EPOLL_CTL_DEL, p[0], 0, 0));
    show("ctl(DEL by the dup's number)", ctl(ep, EPOLL_CTL_DEL, d, 0, 0));
    show("ctl(MOD by the dup's number)", ctl(ep, EPOLL_CTL_MOD, d, EPOLLIN, 2));
    show("ctl(ADD by the dup's number)", ctl(ep, EPOLL_CTL_ADD, d, EPOLLIN, 3));
    wait_show("two registrations of one file", ep, 8, 0);
    /* A new file under the old number is a new registration. */
    int q[2];
    pipe_nb(q);
    printf("new pipe's read end reuses the number: %s\n", q[0] == p[0] ? "yes" : "no");
    show("ctl(ADD new file, same number)", ctl(ep, EPOLL_CTL_ADD, q[0], EPOLLIN, 4));
    write(q[1], "q", 1);
    wait_show("old and new", ep, 8, 0);
    close(d);
    wait_show("last fd of the first file closed", ep, 8, 0);
    show("ctl(DEL new file)", ctl(ep, EPOLL_CTL_DEL, q[0], 0, 0));
    show("ctl(DEL new file again)", ctl(ep, EPOLL_CTL_DEL, q[0], 0, 0));
    wait_show("empty", ep, 8, 0);
    /* dup2 over a registered number closes that file: the registration goes with it. */
    int r[2];
    pipe_nb(r);
    show("ctl(ADD r)", ctl(ep, EPOLL_CTL_ADD, r[0], EPOLLIN, 5));
    write(r[1], "r", 1);
    dup2(q[1], r[0]);
    wait_show("after dup2 over it", ep, 8, 0);
    /* The epoll fd itself: dup keeps it, closing every number ends it. */
    int ep2 = dup(ep);
    close(ep);
    show("ctl(ADD on the dup of the epoll)", ctl(ep2, EPOLL_CTL_ADD, q[0], EPOLLIN, 6));
    wait_show("through the dup", ep2, 8, 0);
    close(ep2);
    close(r[0]);
    close(r[1]);
    close(q[0]);
    close(q[1]);
    close(p[1]);
}

/* ---- nested ---- */

static void nested(void) {
    int inner = ep_create(), outer = ep_create();
    int p[2];
    pipe_nb(p);
    show("ctl(ADD pipe into inner)", ctl(inner, EPOLL_CTL_ADD, p[0], EPOLLIN, 1));
    show("ctl(ADD inner into outer)", ctl(outer, EPOLL_CTL_ADD, inner, EPOLLIN | EPOLLOUT, 2));
    wait_show("outer, nothing", outer, 8, 0);
    printf("poll(inner) %#x\n", revents(inner, POLLIN | POLLOUT | POLLRDNORM));
    write(p[1], "x", 1);
    printf("poll(inner) after write %#x\n", revents(inner, POLLIN | POLLOUT | POLLRDNORM));
    wait_show("outer", outer, 8, 0);
    wait_show("outer again (LT)", outer, 8, 0);
    wait_show("inner", inner, 8, 0);
    drain(p[0]);
    wait_show("outer drained", outer, 8, 0);
    printf("poll(inner) drained %#x\n", revents(inner, POLLIN));
    show("ctl(MOD inner in outer ET)", ctl(outer, EPOLL_CTL_MOD, inner, EPOLLIN | EPOLLET, 3));
    write(p[1], "x", 1);
    wait_show("outer ET", outer, 8, 0);
    wait_show("outer ET again", outer, 8, 0);
    write(p[1], "x", 1);
    wait_show("outer ET after a second write", outer, 8, 0);
    drain(p[0]);
    show("ctl(ADD outer into inner: loop)", ctl(inner, EPOLL_CTL_ADD, outer, EPOLLIN, 4));
    int third = ep_create();
    show("ctl(ADD outer into third)", ctl(third, EPOLL_CTL_ADD, outer, EPOLLIN, 5));
    show("ctl(ADD third into inner: longer loop)", ctl(inner, EPOLL_CTL_ADD, third, EPOLLIN, 6));
    show("ctl(ADD inner into third: no loop)", ctl(third, EPOLL_CTL_ADD, inner, EPOLLIN, 7));
    close(third);
    close(outer);
    close(inner);
    /* A chain of empty epolls: each new head goes into a fresh epoll until the chain is too deep. */
    int e[12];
    e[0] = ep_create();
    for (int i = 1; i < 12; i++) {
        e[i] = ep_create();
        char l[64];
        snprintf(l, sizeof l, "chain of empty epolls: %d into %d", i - 1, i);
        if (show(l, ctl(e[i], EPOLL_CTL_ADD, e[i - 1], EPOLLIN, (uint64_t)i)) < 0)
            break;
    }
    for (int i = 0; i < 12; i++)
        close(e[i]);
    /* The same with a pipe at the bottom. */
    e[0] = ep_create();
    show("ctl(ADD pipe into chain bottom)", ctl(e[0], EPOLL_CTL_ADD, p[0], EPOLLIN, 0));
    for (int i = 1; i < 12; i++) {
        e[i] = ep_create();
        char l[64];
        snprintf(l, sizeof l, "chain over a pipe: %d into %d", i - 1, i);
        if (show(l, ctl(e[i], EPOLL_CTL_ADD, e[i - 1], EPOLLIN, (uint64_t)i)) < 0)
            break;
    }
    for (int i = 0; i < 12; i++)
        close(e[i]);
    /* Depth counts both ways (the epoll inserted into may itself be nested): a <- b <- c, d <- e, f <- g <- h. */
    int ea = ep_create(), eb = ep_create(), ec = ep_create(), ed = ep_create(), ee = ep_create(), ef = ep_create(), eg = ep_create(), eh = ep_create();
    ctl(ea, EPOLL_CTL_ADD, eb, EPOLLIN, 0);
    ctl(eb, EPOLL_CTL_ADD, ec, EPOLLIN, 0);
    ctl(ed, EPOLL_CTL_ADD, ee, EPOLLIN, 0);
    ctl(ef, EPOLL_CTL_ADD, eg, EPOLLIN, 0);
    ctl(eg, EPOLL_CTL_ADD, eh, EPOLLIN, 0);
    show("ctl(d, 1 below, into c, 2 above)", ctl(ec, EPOLL_CTL_ADD, ed, EPOLLIN, 0));
    show("ctl(f, 2 below, into c, 2 above)", ctl(ec, EPOLL_CTL_ADD, ef, EPOLLIN, 0));
    show("ctl(f, 2 below, into b, 1 above)", ctl(eb, EPOLL_CTL_ADD, ef, EPOLLIN, 0));
    show("ctl(e, 0 below, into h, 4 above)", ctl(eh, EPOLL_CTL_ADD, ee, EPOLLIN, 0));
    show("ctl(h, 0 below, into e, 4 above)", ctl(ee, EPOLL_CTL_ADD, eh, EPOLLIN, 0));
    close(ea);
    close(eb);
    close(ec);
    close(ed);
    close(ee);
    close(ef);
    close(eg);
    close(eh);
    /* A chain first, then a file added at its bottom. */
    for (int i = 0; i < 6; i++) {
        e[i] = ep_create();
        if (i > 0) ctl(e[i], EPOLL_CTL_ADD, e[i - 1], EPOLLIN, (uint64_t)i);
    }
    show("ctl(ADD pipe below 5 epolls)", ctl(e[0], EPOLL_CTL_ADD, p[0], EPOLLIN, 0));
    show("ctl(ADD pipe below 4 epolls)", ctl(e[1], EPOLL_CTL_ADD, p[0], EPOLLIN, 0));
    write(p[1], "w", 1);
    wait_show("top of the chain", e[5], 8, 0);
    drain(p[0]);
    for (int i = 0; i < 6; i++)
        close(e[i]);
    close(p[0]);
    close(p[1]);
}

/* ---- flags ---- */

static void flags(void) {
    int ep = ep_create(), other = ep_create();
    int p[2];
    pipe_nb(p);
    show("ADD EXCLUSIVE|IN", ctl(ep, EPOLL_CTL_ADD, p[0], EPOLLEXCLUSIVE | EPOLLIN, 1));
    show("MOD of an exclusive item", ctl(ep, EPOLL_CTL_MOD, p[0], EPOLLIN, 2));
    show("MOD with EXCLUSIVE", ctl(ep, EPOLL_CTL_MOD, p[0], EPOLLIN | EPOLLEXCLUSIVE, 2));
    write(p[1], "x", 1);
    wait_show("exclusive item reports", ep, 8, 0);
    show("DEL exclusive", ctl(ep, EPOLL_CTL_DEL, p[0], 0, 0));
    show("ADD EXCLUSIVE|ONESHOT", ctl(ep, EPOLL_CTL_ADD, p[0], EPOLLEXCLUSIVE | EPOLLIN | EPOLLONESHOT, 3));
    show("ADD EXCLUSIVE|RDNORM", ctl(ep, EPOLL_CTL_ADD, p[0], EPOLLEXCLUSIVE | EPOLLIN | EPOLLRDNORM, 3));
    show("ADD EXCLUSIVE|ET|ERR|HUP|OUT", ctl(ep, EPOLL_CTL_ADD, p[0], EPOLLEXCLUSIVE | EPOLLIN | EPOLLET | EPOLLERR | EPOLLHUP | EPOLLOUT, 3));
    show("DEL", ctl(ep, EPOLL_CTL_DEL, p[0], 0, 0));
    show("ADD EXCLUSIVE of an epoll", ctl(ep, EPOLL_CTL_ADD, other, EPOLLEXCLUSIVE | EPOLLIN, 4));
    show("MOD EXCLUSIVE, not there", ctl(ep, EPOLL_CTL_MOD, p[1], EPOLLEXCLUSIVE | EPOLLIN, 4));
    show("ADD EPOLLWAKEUP", ctl(ep, EPOLL_CTL_ADD, p[0], EPOLLWAKEUP | EPOLLIN, 5));
    wait_show("wakeup item", ep, 8, 0);
    show("ADD RDHUP|PRI|MSG|RDBAND|WRBAND", ctl(ep, EPOLL_CTL_ADD, p[1], EPOLLRDHUP | EPOLLPRI | EPOLLMSG | EPOLLRDBAND | EPOLLWRBAND | EPOLLOUT, 6));
    wait_show("odd bits", ep, 8, 0);
    show("ADD with every bit", ctl(other, EPOLL_CTL_ADD, p[0], 0xffffffffu & ~EPOLLEXCLUSIVE, 7));
    wait_show("every bit", other, 8, 0);
    close(p[1]);
    wait_show("every bit, writer gone", other, 8, 0);
    close(p[0]);
    close(other);
    close(ep);
}

/* ---- timeouts ---- */

static void timeouts(void) {
    int ep = ep_create();
    struct epoll_event out[2];
    uint64_t t0 = mono_ns();
    SYS("epoll_wait(nothing, 30 ms)", SYS_epoll_wait, ep, out, 2, 30);
    yes("waited at least 30 ms", mono_ns() - t0 >= 30000000);
    unsigned long mask = 1ul << (SIGUSR1 - 1), now;
    t0 = mono_ns();
    SYS("epoll_pwait(nothing, 20 ms, a mask)", SYS_epoll_pwait, ep, out, 2, 20, &mask, 8);
    yes("waited at least 20 ms", mono_ns() - t0 >= 20000000);
    syscall(SYS_rt_sigprocmask, SIG_BLOCK, NULL, &now, 8);
    printf("mask after epoll_pwait %#lx\n", now);
    struct timespec ts = {0, 25000000};
    t0 = mono_ns();
    SYS("epoll_pwait2(nothing, 25 ms)", SYS_epoll_pwait2, ep, out, 2, &ts, NULL, 8);
    yes("waited at least 25 ms", mono_ns() - t0 >= 25000000);
    printf("timespec kept: %ld %ld\n", (long)ts.tv_sec, ts.tv_nsec);
    ts = (struct timespec){0, 0};
    SYS("epoll_pwait2(zero)", SYS_epoll_pwait2, ep, out, 2, &ts, NULL, 8);
    SYS("epoll_pwait2(NULL timeout, ready)", SYS_epoll_pwait2, ep, out, 0, NULL, NULL, 8);
    ts = (struct timespec){0, 1000000000};
    SYS("epoll_pwait2(nsec 1e9)", SYS_epoll_pwait2, ep, out, 2, &ts, NULL, 8);
    ts = (struct timespec){-1, 0};
    SYS("epoll_pwait2(sec -1)", SYS_epoll_pwait2, ep, out, 2, &ts, NULL, 8);
    SYS("epoll_pwait2(timeout in kernel memory)", SYS_epoll_pwait2, ep, out, 2, KADDR, NULL, 8);
    ts = (struct timespec){0, 0};
    SYS("epoll_pwait2(sigsetsize 4)", SYS_epoll_pwait2, ep, out, 2, &ts, &mask, 4);
    SYS("epoll_pwait2(bad epfd after a bad timeout)", SYS_epoll_pwait2, 99, out, 2, KADDR, NULL, 8);
    SYS("epoll_pwait(bad epfd, bad mask)", SYS_epoll_pwait, 99, out, 2, 0, KADDR, 8);
    /* A wait that something ends: an eventfd written before; a long timeout returns at once. */
    int efd = (int)syscall(SYS_eventfd2, 1, 0);
    ctl(ep, EPOLL_CTL_ADD, efd, EPOLLIN, 9);
    t0 = mono_ns();
    wait_show("epoll_wait(ready, 5 s)", ep, 2, 5000);
    yes("returned before 1 s", mono_ns() - t0 < 1000000000ull);
    wait_show("epoll_wait(ready, -1)", ep, 2, -1);
    /* ppoll/pselect6 masks are put back. */
    struct pollfd pf = {efd, POLLIN, 0};
    ts = (struct timespec){0, 10000000};
    SYS("ppoll(ready, mask)", SYS_ppoll, &pf, 1, &ts, &mask, 8);
    syscall(SYS_rt_sigprocmask, SIG_BLOCK, NULL, &now, 8);
    printf("mask after ppoll %#lx\n", now);
    close(efd);
    close(ep);
}

/* ---- select ---- */

static void selects(void) {
    int p[2];
    pipe_nb(p);
    fd_set r, w, x;
    FD_ZERO(&r);
    FD_ZERO(&w);
    FD_ZERO(&x);
    FD_SET(p[0], &r);
    FD_SET(p[1], &w);
    FD_SET(p[0], &x);
    struct timeval tv = {0, 0};
    SYS("select(read end, write end, zero timeout)", SYS_select, p[1] + 1, &r, &w, &x, &tv);
    printf("  r %d w %d x %d\n", FD_ISSET(p[0], &r), FD_ISSET(p[1], &w), FD_ISSET(p[0], &x));
    write(p[1], "s", 1);
    FD_ZERO(&r);
    FD_ZERO(&w);
    FD_SET(p[0], &r);
    FD_SET(p[1], &r);
    FD_SET(p[1], &w);
    SYS("select(read and write, NULL timeout)", SYS_select, p[1] + 1, &r, &w, NULL, NULL);
    printf("  r(p0) %d r(p1) %d w %d\n", FD_ISSET(p[0], &r), FD_ISSET(p[1], &r), FD_ISSET(p[1], &w));
    drain(p[0]);
    FD_ZERO(&r);
    FD_SET(p[0], &r);
    tv = (struct timeval){0, 20000};
    uint64_t t0 = mono_ns();
    SYS("select(nothing, 20 ms)", SYS_select, p[0] + 1, &r, NULL, NULL, &tv);
    yes("waited at least 20 ms", mono_ns() - t0 >= 20000000);
    printf("  set cleared %d, timeval %ld %ld\n", FD_ISSET(p[0], &r), (long)tv.tv_sec, (long)tv.tv_usec);
    FD_ZERO(&r);
    FD_SET(p[1], &w);
    tv = (struct timeval){5, 0};
    SYS("select(write end, 5 s)", SYS_select, p[1] + 1, NULL, &w, NULL, &tv);
    yes("time left 4..5 s", tv.tv_sec == 4 || (tv.tv_sec == 5 && tv.tv_usec == 0));
    FD_ZERO(&r);
    FD_SET(60, &r);
    tv = (struct timeval){5, 0};
    SYS("select(closed fd 60)", SYS_select, 61, &r, NULL, NULL, &tv);
    yes("timeval rewritten after EBADF", tv.tv_sec == 4 || (tv.tv_sec == 5 && tv.tv_usec == 0));
    /* Bits above the fd table's size (64 at first) are neither looked at nor written. */
    FD_ZERO(&r);
    FD_SET(p[0], &r);
    FD_SET(100, &r);
    write(p[1], "s", 1);
    SYS("select(1024, fd 100 not open, beyond the table)", SYS_select, 1024, &r, NULL, NULL, NULL);
    printf("  bit 100 kept %d, read end %d\n", FD_ISSET(100, &r), FD_ISSET(p[0], &r));
    drain(p[0]);
    SYS("select(-1)", SYS_select, -1, NULL, NULL, NULL, NULL);
    tv = (struct timeval){0, -1};
    SYS("select(usec -1)", SYS_select, 1, NULL, NULL, NULL, &tv);
    tv = (struct timeval){-1, 0};
    SYS("select(sec -1)", SYS_select, 1, NULL, NULL, NULL, &tv);
    t0 = mono_ns();
    SYS("select(no fds, 15 ms)", SYS_select, 0, NULL, NULL, NULL, (&(struct timeval){0, 15000}));
    yes("select with no fds waits", mono_ns() - t0 >= 15000000);
    SYS("select(set in kernel memory)", SYS_select, 1, KADDR, NULL, NULL, NULL);
    SYS("select(timeval in kernel memory)", SYS_select, 1, NULL, NULL, NULL, KADDR);
    FD_ZERO(&r);
    FD_SET(p[1], &w);
    SYS("select(write end in exceptfds only)", SYS_select, p[1] + 1, NULL, NULL, &w, &(struct timeval){0, 0});
    printf("  x %d\n", FD_ISSET(p[1], &w));
    /* pselect6: { sigset *, size } */
    unsigned long mask = 1ul << (SIGUSR1 - 1), now;
    struct { void *p; size_t n; } sp = {&mask, 8}, bad = {&mask, 4}, kern = {KADDR, 8};
    struct timespec ts = {0, 10000000};
    FD_ZERO(&w);
    FD_SET(p[1], &w);
    SYS("pselect6(write end, mask)", SYS_pselect6, p[1] + 1, NULL, &w, NULL, &ts, &sp);
    syscall(SYS_rt_sigprocmask, SIG_BLOCK, NULL, &now, 8);
    printf("  mask after %#lx\n", now);
    SYS("pselect6(sigsetsize 4)", SYS_pselect6, p[1] + 1, NULL, &w, NULL, &ts, &bad);
    SYS("pselect6(mask pointer in kernel memory)", SYS_pselect6, p[1] + 1, NULL, &w, NULL, &ts, &kern);
    SYS("pselect6(argpack in kernel memory)", SYS_pselect6, p[1] + 1, NULL, &w, NULL, &ts, KADDR);
    ts = (struct timespec){0, 1000000000};
    SYS("pselect6(nsec 1e9)", SYS_pselect6, p[1] + 1, NULL, &w, NULL, &ts, NULL);
    ts = (struct timespec){0, 20000000};
    FD_ZERO(&r);
    FD_SET(p[0], &r);
    SYS("pselect6(nothing, 20 ms)", SYS_pselect6, p[0] + 1, &r, NULL, NULL, &ts, NULL);
    printf("  timespec left %ld %ld\n", (long)ts.tv_sec, ts.tv_nsec);
    close(p[0]);
    close(p[1]);
}

/* ---- poll ---- */

static void polls(void) {
    int efd = (int)syscall(SYS_eventfd2, 0, 0);
    printf("eventfd empty %#x\n", revents(efd, 0x7fff));
    uint64_t one = 1;
    write(efd, &one, 8);
    printf("eventfd one %#x\n", revents(efd, 0x7fff));
    int ep = ep_create();
    printf("epoll empty %#x\n", revents(ep, 0x7fff));
    ctl(ep, EPOLL_CTL_ADD, efd, EPOLLIN, 1);
    printf("epoll with a ready eventfd %#x\n", revents(ep, 0x7fff));
    ctl(ep, EPOLL_CTL_MOD, efd, EPOLLOUT | EPOLLET, 1);
    struct epoll_event ev;
    syscall(SYS_epoll_wait, ep, &ev, 1, 0);
    printf("epoll after its ET event was taken %#x\n", revents(ep, 0x7fff));
    int rnd = open("/dev/random", O_RDONLY);
    printf("/dev/random %#x\n", revents(rnd, 0x7fff));
    int null = open("/dev/null", O_RDWR);
    printf("/dev/null %#x\n", revents(null, 0x7fff));
    int p[2];
    pipe_nb(p);
    printf("read end asked OUT %#x, write end asked IN %#x\n", revents(p[0], POLLOUT), revents(p[1], POLLIN));
    struct pollfd two[2] = {{p[1], POLLOUT, 0}, {p[1], POLLOUT | POLLWRNORM, 0}};
    SYS("poll(the same fd twice)", SYS_poll, two, 2, 0);
    printf("  %#x %#x\n", two[0].revents, two[1].revents);
    close(p[0]);
    printf("write end, reader gone %#x\n", revents(p[1], POLLOUT));
    close(p[1]);
    close(rnd);
    close(null);
    close(ep);
    close(efd);
}

/* ---- waiter (ktests) ---- */

static void waiter(const char *k) {
    fcntl(0, F_SETFL, fcntl(0, F_GETFL) | O_NONBLOCK);
    int ep = -1;
    if (!strncmp(k, "epoll", 5)) {
        ep = ep_create();
        ctl(ep, EPOLL_CTL_ADD, 0, EPOLLIN | (!strcmp(k, "epoll-et") ? EPOLLET : 0), 0);
    }
    unsigned long long bytes = 0, sum = 0;
    for (;;) {
        long r;
        if (ep >= 0) {
            struct epoll_event ev;
            r = syscall(SYS_epoll_wait, ep, &ev, 1, -1);
        } else if (!strcmp(k, "poll")) {
            struct pollfd pf = {0, POLLIN, 0};
            r = syscall(SYS_poll, &pf, 1, -1);
        } else if (!strcmp(k, "ppoll")) {
            struct pollfd pf = {0, POLLIN, 0};
            r = syscall(SYS_ppoll, &pf, 1, NULL, NULL, 8);
        } else {
            fd_set s;
            FD_ZERO(&s);
            FD_SET(0, &s);
            r = !strcmp(k, "select") ? syscall(SYS_select, 1, &s, NULL, NULL, NULL) : syscall(SYS_pselect6, 1, &s, NULL, NULL, NULL, NULL);
        }
        if (r < 0) {
            printf("%s: wait -%s\n", k, ename(errno));
            return;
        }
        int eof = 0;
        for (;;) {
            unsigned char b[4096];
            long n = read(0, b, sizeof b);
            if (n == 0) eof = 1;
            if (n <= 0) break;
            for (long i = 0; i < n; i++) sum = sum * 31 + b[i];
            bytes += n;
        }
        if (eof) break;
    }
    printf("%s got %llu sum %llu\n", k, bytes, sum);
}

/* ---- sigwait (FS-4 CR8: handled signals end the waits) ---- */

static volatile sig_atomic_t handled, usr2_blocked_in_handler;

static void on_usr1(int sig) {
    (void)sig;
    sigset_t now;
    sigprocmask(SIG_BLOCK, NULL, &now);
    usr2_blocked_in_handler = sigismember(&now, SIGUSR2);
    handled++;
}

/* A child that sends SIGUSR1 to this process after 30 ms. */
static pid_t poke_later(void) {
    pid_t me = getpid();
    pid_t c = fork();
    if (c == 0) {
        struct timespec ts = {0, 30 * 1000 * 1000};
        nanosleep(&ts, NULL);
        kill(me, SIGUSR1);
        _exit(0);
    }
    return c;
}

/* One wait on a pipe nobody writes; a 3 s bound so a wait that is not interrupted prints instead of hanging. */
static long wait_kind(const char *k, int rfd, const sigset_t *temp) {
    struct pollfd pf = {rfd, POLLIN, 0};
    struct timespec ts = {3, 0};
    struct timeval tv = {3, 0};
    fd_set s;
    FD_ZERO(&s);
    FD_SET(rfd, &s);
    struct epoll_event ev;
    int ep = -1;
    if (!strncmp(k, "epoll", 5)) {
        ep = ep_create();
        ctl(ep, EPOLL_CTL_ADD, rfd, EPOLLIN, 0);
    }
    long r;
    /* The sizes as full words: a sixth argument travels on the stack, where an int's upper half is undefined. */
    const long ss = 8;
    if (!strcmp(k, "epoll_wait")) r = syscall(SYS_epoll_wait, ep, &ev, 1, 3000);
    else if (!strcmp(k, "epoll_pwait")) r = syscall(SYS_epoll_pwait, ep, &ev, 1, 3000, temp, ss);
    else if (!strcmp(k, "epoll_pwait2")) r = syscall(SYS_epoll_pwait2, ep, &ev, 1, &ts, temp, ss);
    else if (!strcmp(k, "poll")) r = syscall(SYS_poll, &pf, 1, 3000);
    else if (!strcmp(k, "ppoll")) r = syscall(SYS_ppoll, &pf, 1, &ts, temp, ss);
    else if (!strcmp(k, "select")) r = syscall(SYS_select, rfd + 1, &s, NULL, NULL, &tv);
    else {
        struct { const sigset_t *set; size_t len; } sm = {temp, 8};
        r = syscall(SYS_pselect6, rfd + 1, &s, NULL, NULL, &ts, &sm);
    }
    int e = errno;
    if (ep >= 0) close(ep);
    errno = e;
    return r;
}

static void sigwait_modes(void) {
    static const char *kinds[] = {"epoll_wait", "epoll_pwait", "epoll_pwait2", "poll", "ppoll", "select", "pselect6"};
    int p[2];
    if (pipe(p) < 0) {
        printf("pipe -%s\n", ename(errno));
        return;
    }
    for (int restart = 0; restart < 2; restart++) {
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_handler = on_usr1;
        sa.sa_flags = restart ? SA_RESTART : 0;
        sigaction(SIGUSR1, &sa, NULL);
        for (unsigned i = 0; i < sizeof kinds / sizeof kinds[0]; i++) {
            handled = 0;
            pid_t c = poke_later();
            long r = wait_kind(kinds[i], p[0], NULL);
            int e = errno;
            waitpid(c, NULL, 0);
            printf("%s SA_RESTART=%d: %ld%s%s handled %d\n", kinds[i], restart, r, r < 0 ? " -" : "", r < 0 ? ename(e) : "", (int)handled);
        }
    }
    /* The temporary mask: SIGUSR1 and SIGUSR2 blocked, the wait's mask empty. */
    static const char *masked[] = {"epoll_pwait", "epoll_pwait2", "ppoll", "pselect6"};
    sigset_t block, empty, after;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    sigaddset(&block, SIGUSR2);
    sigemptyset(&empty);
    for (unsigned i = 0; i < sizeof masked / sizeof masked[0]; i++) {
        sigprocmask(SIG_SETMASK, &block, NULL);
        handled = 0;
        usr2_blocked_in_handler = -1;
        pid_t c = poke_later();
        long r = wait_kind(masked[i], p[0], &empty);
        int e = errno;
        waitpid(c, NULL, 0);
        sigprocmask(SIG_BLOCK, NULL, &after);
        printf("%s with an empty mask: %ld%s%s handled %d, SIGUSR2 blocked in the handler: %s, SIGUSR1 blocked after: %s\n",
               masked[i], r, r < 0 ? " -" : "", r < 0 ? ename(e) : "", (int)handled,
               usr2_blocked_in_handler < 0 ? "no handler" : usr2_blocked_in_handler ? "yes" : "no",
               sigismember(&after, SIGUSR1) ? "yes" : "no");
        sigprocmask(SIG_SETMASK, &empty, NULL);
    }
    /* Blocked and not in the temporary mask: the signal waits, the wait times out after its own limit. */
    sigprocmask(SIG_SETMASK, &block, NULL);
    handled = 0;
    pid_t c = poke_later();
    struct pollfd pf = {p[0], POLLIN, 0};
    struct timespec ts = {0, 200 * 1000 * 1000};
    long r = syscall(SYS_ppoll, &pf, 1, &ts, &block, 8L);
    waitpid(c, NULL, 0);
    printf("ppoll with SIGUSR1 kept blocked: %ld handled %d\n", r, (int)handled);
    sigprocmask(SIG_SETMASK, &empty, NULL);
    printf("after unblocking: handled %d\n", (int)handled);
    close(p[0]);
    close(p[1]);
    printf("sigwait: done\n");
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    const char *m = argc > 1 ? argv[1] : "basic";
    if (!strcmp(m, "basic")) basic();
    else if (!strcmp(m, "modes")) modes();
    else if (!strcmp(m, "close")) closes();
    else if (!strcmp(m, "nested")) nested();
    else if (!strcmp(m, "flags")) flags();
    else if (!strcmp(m, "timeouts")) timeouts();
    else if (!strcmp(m, "select")) selects();
    else if (!strcmp(m, "poll")) polls();
    else if (!strcmp(m, "waiter") && argc > 2) waiter(argv[2]);
    else if (!strcmp(m, "sigwait")) sigwait_modes();
    else {
        printf("unknown mode %s\n", m);
        return 2;
    }
    return 0;
}
