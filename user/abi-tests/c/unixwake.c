/* FS-17: a compositor and its clients under load (the lost wake-up bitscomp showed on 27.10.). The parent is
 * the compositor: one AF_UNIX stream socketpair per client, the client sockets in four inner epolls (as
 * wayland-backend's own epoll) inside one outer epoll (as calloop's), each inner epoll watched differently:
 *   0: outer EPOLLET, inner EPOLLET and level-triggered by turns, drained until the inner epoll_wait gives 0
 *      (at most 9 batches, then the loop comes back without sleeping);
 *   1: outer level-triggered, inner level-triggered, one inner epoll_wait per outer event (Smithay's
 *      Mode::Level): reading to EAGAIN leaves stale items on the inner ready list;
 *   2: outer EPOLLONESHOT re-armed by EPOLL_CTL_MOD, inner EPOLLET|EPOLLONESHOT re-armed after reading;
 *   3: outer EPOLLET, inner level-triggered, one inner epoll_wait per outer event (a full batch: again without
 *      sleeping).
 * Plus a 1 ms timerfd in the outer epoll and a bit of work in some loops (a busy compositor), and a second
 * compositor thread with one client in the tightest shape (see `tight`). The parent sends bursts of 1-4 pings
 * per client, some with a file descriptor attached (SCM_RIGHTS), some two in one write; a client waits in
 * poll() or in a blocking recvmsg and answers each ping with a pong, some with a descriptor, some split in two
 * writes. Busy processes keep every CPU loaded and fault pages in. Every ping must come back in order (a new
 * burst goes out only when the last one is back); a pong older than STALL_S seconds is a stall (a lost wake-up):
 * the program then says on which side the data sat and which epoll still knew it.
 * Usage: abi-unixwake [seconds [clients [busy]]]. Without arguments (the ABI diff) it runs 3 s and prints
 * relations only; with arguments it also prints counts (rounds, pongs later than 1 s, the longest wait). */
#include "abi.h"
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/timerfd.h>
#include <sys/wait.h>

#define MAGIC 0x57414b45u
#define PING 1u
#define PONG 2u
#define QUIT 3u
#define WINDOW 4
#define STALL_S 5
#define NINNER 4
#define MSG 16

struct msg {
    uint32_t magic, kind, seq, nfd;
};

static uint64_t now_ns(void) { return ns_of(CLOCK_MONOTONIC); }

static uint64_t rng = 0x9E3779B97F4A7C15ull;
static uint32_t rnd(void) {
    rng ^= rng << 13;
    rng ^= rng >> 7;
    rng ^= rng << 17;
    return (uint32_t)rng;
}

/* Sends `n` messages (in one write) with `fd` attached when fd >= 0. Blocking writes. */
static int send_msgs(int s, struct msg *m, int n, int fd) {
    struct iovec iov = {m, (size_t)n * MSG};
    char cbuf[CMSG_SPACE(sizeof(int))];
    struct msghdr h = {0};
    h.msg_iov = &iov;
    h.msg_iovlen = 1;
    if (fd >= 0) {
        memset(cbuf, 0, sizeof cbuf);
        h.msg_control = cbuf;
        h.msg_controllen = sizeof cbuf;
        struct cmsghdr *c = CMSG_FIRSTHDR(&h);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(c), &fd, sizeof(int));
    }
    size_t want = iov.iov_len;
    while (want) {
        ssize_t r = sendmsg(s, &h, MSG_NOSIGNAL);
        if (r < 0) {
            if (errno == EINTR) continue;
            if (errno == EAGAIN) {
                struct pollfd p = {s, POLLOUT, 0};
                poll(&p, 1, 1000);
                continue;
            }
            return -1;
        }
        want -= (size_t)r;
        iov.iov_base = (char *)iov.iov_base + r;
        iov.iov_len = want;
        h.msg_control = 0;
        h.msg_controllen = 0;
    }
    return 0;
}

/* One recvmsg into buf; closes received descriptors and counts them in *fds. Returns bytes, 0 at EOF, -1. */
static ssize_t recv_some(int s, char *buf, size_t room, int flags, long *fds) {
    struct iovec iov = {buf, room};
    char cbuf[CMSG_SPACE(sizeof(int) * 8)];
    struct msghdr h = {0};
    h.msg_iov = &iov;
    h.msg_iovlen = 1;
    h.msg_control = cbuf;
    h.msg_controllen = sizeof cbuf;
    ssize_t r = recvmsg(s, &h, flags | MSG_CMSG_CLOEXEC);
    if (r < 0) return -1;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&h); c; c = CMSG_NXTHDR(&h, c)) {
        if (c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS) continue;
        int n = (int)((c->cmsg_len - CMSG_LEN(0)) / sizeof(int));
        for (int i = 0; i < n; i++) {
            int fd;
            memcpy(&fd, CMSG_DATA(c) + i * sizeof(int), sizeof(int));
            close(fd);
            (*fds)++;
        }
    }
    return r;
}

/* ---- client: answers every ping; exits 0 at QUIT with everything in order ---- */

static int client(int s, int k) {
    int blocking = k % 2; /* odd clients sleep in recvmsg, even ones in poll() then read to EAGAIN */
    int pfd[2];
    if (pipe(pfd)) return 10;
    rng ^= (uint64_t)(k + 1) * 0x2545F4914F6CDD1Dull;
    char buf[4096];
    size_t have = 0;
    uint32_t want = 0;
    long fds = 0, fds_said = 0;
    for (;;) {
        if (!blocking) {
            struct pollfd p = {s, POLLIN, 0};
            int r = poll(&p, 1, -1);
            if (r < 0 && errno == EINTR) continue;
            if (r < 0) return 11;
        }
        for (;;) {
            ssize_t r = recv_some(s, buf + have, sizeof buf - have, blocking ? 0 : MSG_DONTWAIT, &fds);
            if (r < 0 && errno == EINTR) continue;
            if (r < 0 && errno == EAGAIN) break;
            if (r <= 0) return 12;
            have += (size_t)r;
            size_t off = 0;
            for (; have - off >= MSG; off += MSG) {
                struct msg m;
                memcpy(&m, buf + off, MSG);
                if (m.magic != MAGIC) return 13;
                if (m.kind == QUIT) return fds == fds_said + m.nfd ? 0 : 14;
                if (m.kind != PING || m.seq != want) return 15;
                fds_said += m.nfd;
                want++;
                struct msg p = {MAGIC, PONG, m.seq, 0};
                uint32_t x = rnd();
                if (x % 8 == 0) {
                    p.nfd = 1;
                    if (send_msgs(s, &p, 1, pfd[0])) return 16;
                } else if (x % 8 == 1) {
                    /* split: the compositor sees half a message first */
                    if (write(s, &p, 6) != 6) return 17;
                    sched_yield();
                    if (write(s, (char *)&p + 6, MSG - 6) != MSG - 6) return 17;
                } else if (send_msgs(s, &p, 1, -1)) {
                    return 16;
                }
            }
            memmove(buf, buf + off, have - off);
            have -= off;
            if (blocking) break;
        }
    }
}

/* ---- busy: every CPU loaded, pages faulted in and given back ---- */

static void busy(int k, pid_t parent) {
    volatile uint64_t x = (uint64_t)k;
    for (;;) {
        for (int i = 0; i < 200000; i++) x = x * 6364136223846793005ull + 1442695040888963407ull;
        char *p = mmap(0, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p != MAP_FAILED) {
            for (int o = 0; o < 1 << 20; o += 4096) p[o] = (char)x;
            munmap(p, 1 << 20);
        }
        if (getppid() != parent) _exit(0); /* the compositor is gone (killed by a test's guard) */
    }
}

/* ---- the compositor ---- */

struct cl {
    int s, inner;
    uint32_t inner_ev;
    pid_t pid;
    uint32_t next, back; /* pings sent, pongs received */
    uint64_t sent_at[WINDOW];
    char buf[4096];
    size_t have;
    long fds, fds_said, late, bad;
};

static int nc;
static struct cl *cls;
static int ep_in[NINNER], ep_out, tfd, pipefd[2];
static long rounds, late, stalls, bad;
static uint64_t worst;
static int stopping;

/* A new burst of 1..WINDOW pings (one or two per write) once the last one was answered: nothing else wakes
 * the client's side again, so a lost wake-up stays lost until the stall check (as bitscomp's one ping). */
static void ping(struct cl *c) {
    if (stopping || c->next != c->back) return;
    int burst = 1 + (int)(rnd() % WINDOW);
    while (burst > 0) {
        struct msg m[2] = {{MAGIC, PING, c->next, 0}, {MAGIC, PING, c->next + 1, 0}};
        int two = burst >= 2 && rnd() % 2 == 0;
        int fd = rnd() % 8 == 0 ? pipefd[0] : -1;
        if (fd >= 0) m[0].nfd = 1;
        for (int i = 0; i < 1 + two; i++) c->sent_at[(c->next + i) % WINDOW] = now_ns();
        if (send_msgs(c->s, m, 1 + two, fd)) {
            c->bad++;
            return;
        }
        c->next += 1 + two;
        burst -= 1 + two;
    }
}

/* Reads one client to EAGAIN (all = 1) or once. */
static void dispatch(struct cl *c, int all) {
    do {
        ssize_t r = recv_some(c->s, c->buf + c->have, sizeof c->buf - c->have, MSG_DONTWAIT, &c->fds);
        if (r < 0 && errno == EINTR) continue;
        if (r < 0 && errno == EAGAIN) break;
        if (r <= 0) {
            c->bad++;
            break;
        }
        c->have += (size_t)r;
        size_t off = 0;
        for (; c->have - off >= MSG; off += MSG) {
            struct msg m;
            memcpy(&m, c->buf + off, MSG);
            if (m.magic != MAGIC || m.kind != PONG || m.seq != c->back) {
                c->bad++;
                continue;
            }
            uint64_t took = now_ns() - c->sent_at[m.seq % WINDOW];
            if (took > worst) worst = took;
            if (took > 1000000000ull) c->late++;
            c->fds_said += m.nfd;
            c->back++;
            rounds++;
        }
        memmove(c->buf, c->buf + off, c->have - off);
        c->have -= off;
    } while (all);
    if (c->inner_ev & EPOLLONESHOT) {
        struct epoll_event e = {c->inner_ev, {.u64 = (uint64_t)(c - cls)}};
        epoll_ctl(c->inner, EPOLL_CTL_MOD, c->s, &e);
    }
    ping(c);
}

/* An inner epoll left with events after its batches (its outer item is edge-triggered: no new edge comes for
 * them, so the loop comes back to it without sleeping, as a compositor that bounds its dispatch does). */
static int left[NINNER];

static void run_inner(int i) {
    struct epoll_event ev[4];
    left[i] = 0;
    for (int batch = 0;; batch++) {
        /* small batches: items left over go back on the inner ready list */
        int max = i == 1 || i == 3 ? 4 : 2;
        int n = epoll_wait(ep_in[i], ev, max, 0);
        for (int j = 0; j < n; j++) dispatch(&cls[ev[j].data.u64], 1);
        if (i == 3 && n == max) left[i] = 1;
        if (i == 1 || i == 3 || n <= 0) break;
        if (batch == 8) {
            /* bounded: the clients answer as fast as they are read, so draining to 0 might never end */
            left[i] = i == 0;
            break;
        }
    }
    if (i == 2) {
        struct epoll_event e = {EPOLLIN | EPOLLONESHOT, {.u64 = 1000 + 2}};
        epoll_ctl(ep_out, EPOLL_CTL_MOD, ep_in[2], &e);
    }
}

/* A pong older than STALL_S: where does the data sit? */
static uint64_t seen_at[NINNER]; /* when the outer epoll last reported each inner one */
static long loops;

/* A pong older than STALL_S: where does the data sit, and which epoll still knows it? */
static void stall(struct cl *c, uint64_t age) {
    int k = (int)(c - cls), i = k % NINNER;
    int inq = -1;
    ioctl(c->s, FIONREAD, &inq);
    struct pollfd p = {c->s, POLLIN, 0};
    poll(&p, 1, 0);
    struct pollfd pi = {c->inner, POLLIN, 0};
    poll(&pi, 1, 0);
    /* Harvest both levels now (what a wake would have made happen): does each still report it? */
    struct epoll_event ev[16];
    int outer_has = 0, inner_has = 0;
    int n = epoll_wait(ep_out, ev, 16, 0);
    for (int j = 0; j < n; j++) outer_has |= ev[j].data.u64 == 1000 + (uint64_t)i;
    n = epoll_wait(c->inner, ev, 16, 0);
    for (int j = 0; j < n; j++) {
        inner_has |= ev[j].data.u64 == (uint64_t)k;
        if (ev[j].data.u64 != (uint64_t)k) dispatch(&cls[ev[j].data.u64], 1);
    }
    printf("stall: client %d (inner %d, events %#x): ping %u waits %llu ms; compositor socket %s (%d bytes), "
           "poll %#x, inner epoll fd poll %#x; now the outer epoll %s the inner one (last %llu ms ago), the inner "
           "one %s the socket; loops %ld\n",
           k, i, c->inner_ev, c->back, (unsigned long long)(age / 1000000),
           inq > 0 ? "has unread data: its wake-up was lost" : "is empty: the client did not answer", inq,
           p.revents, pi.revents, outer_has ? "reports" : "does not report",
           (unsigned long long)((now_ns() - seen_at[i]) / 1000000), inner_has ? "reports" : "does not report", loops);
    fflush(stdout);
    stalls++;
    for (int j = 0; j < NINNER; j++) run_inner(j);
}

/* ---- the tight pair: a second compositor thread with one client and nothing in between: ping, look at its
 * outer epoll (EPOLLET on an inner epoll with the socket level-triggered) without sleeping until it reports,
 * one inner harvest, read to EAGAIN. The client's read wakes the compositor's socket (its charge drops) just
 * before the pong comes: the look at the inner epoll then races with the pong's wake every round. ---- */

static volatile int tight_stop;
static long tight_rounds, tight_stalls, tight_bad;
static int tight_s, tight_in, tight_out;

static void *tight(void *arg) {
    (void)arg;
    char b[256];
    long fds = 0;
    for (uint32_t seq = 0; !tight_stop; seq++) {
        struct msg m = {MAGIC, PING, seq, 0};
        if (send_msgs(tight_s, &m, 1, -1)) {
            tight_bad++;
            return 0;
        }
        uint64_t t0 = now_ns();
        size_t got = 0;
        while (got < MSG) {
            struct epoll_event ev[2];
            if (epoll_wait(tight_out, ev, 2, 0) <= 0) {
                if (now_ns() - t0 < (uint64_t)STALL_S * 1000000000ull) continue;
                int inq = -1;
                ioctl(tight_s, FIONREAD, &inq);
                struct pollfd pi = {tight_in, POLLIN, 0};
                poll(&pi, 1, 0);
                printf("stall: tight pair: pong %u waits %d s; compositor socket %s (%d bytes), "
                       "inner epoll fd poll %#x\n",
                       seq, STALL_S, inq > 0 ? "has unread data: its wake-up was lost" : "is empty", inq, pi.revents);
                tight_stalls++;
                t0 = now_ns();
            } else if (epoll_wait(tight_in, ev, 2, 0) <= 0) {
                continue;
            }
            for (;;) {
                ssize_t r = recv_some(tight_s, b + got, sizeof b - got, MSG_DONTWAIT, &fds);
                if (r <= 0) break;
                got += (size_t)r;
            }
        }
        memcpy(&m, b, MSG);
        if (got != MSG || m.magic != MAGIC || m.kind != PONG || m.seq != seq) tight_bad++;
        tight_rounds++;
    }
    return 0;
}

int main(int argc, char **argv) {
    int secs = argc > 1 ? atoi(argv[1]) : 3;
    nc = argc > 2 ? atoi(argv[2]) : 8;
    int nbusy = argc > 3 ? atoi(argv[3]) : 2;
    int verbose = argc > 1;
    if (secs < 1) secs = 1;
    if (nc < 1) nc = 1;
    signal(SIGPIPE, SIG_IGN);
    setvbuf(stdout, 0, _IOLBF, 0);
    if (pipe(pipefd)) return 2;
    cls = calloc((size_t)nc, sizeof *cls);
    pid_t *busy_pids = calloc((size_t)nbusy + 1, sizeof(pid_t));
    if (!cls || !busy_pids) return 2;
    ep_out = epoll_create1(EPOLL_CLOEXEC);
    static const uint32_t outer_ev[NINNER] = {EPOLLIN | EPOLLET, EPOLLIN, EPOLLIN | EPOLLONESHOT, EPOLLIN | EPOLLET};
    static const uint32_t inner_evs[NINNER] = {EPOLLIN | EPOLLET, EPOLLIN, EPOLLIN | EPOLLET | EPOLLONESHOT, EPOLLIN};
    for (int i = 0; i < NINNER; i++) {
        ep_in[i] = epoll_create1(EPOLL_CLOEXEC);
        struct epoll_event e = {outer_ev[i], {.u64 = 1000 + (uint64_t)i}};
        if (epoll_ctl(ep_out, EPOLL_CTL_ADD, ep_in[i], &e)) return 3;
    }
    tfd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC);
    struct itimerspec its = {{0, 1000000}, {0, 1000000}};
    timerfd_settime(tfd, 0, &its, 0);
    struct epoll_event te = {EPOLLIN, {.u64 = 2000}};
    epoll_ctl(ep_out, EPOLL_CTL_ADD, tfd, &te);
    for (int k = 0; k < nc; k++) {
        int sv[2];
        if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv)) return 4;
        pid_t p = fork();
        if (p == 0) {
            close(sv[0]);
            _exit(client(sv[1], k));
        }
        close(sv[1]);
        struct cl *c = &cls[k];
        c->s = sv[0];
        c->pid = p;
        c->inner = ep_in[k % NINNER];
        c->inner_ev = inner_evs[k % NINNER];
        /* every second client of the edge-triggered group is level-triggered (a level item in an EPOLLET
         * outer item: the shape the host test catches fastest) */
        if (k % NINNER == 0 && (k / NINNER) % 2) c->inner_ev = EPOLLIN;
        fcntl(c->s, F_SETFL, O_NONBLOCK);
        struct epoll_event e = {c->inner_ev, {.u64 = (uint64_t)k}};
        if (epoll_ctl(c->inner, EPOLL_CTL_ADD, c->s, &e)) return 5;
    }
    int tv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, tv)) return 4;
    pid_t tight_pid = fork();
    if (tight_pid == 0) {
        close(tv[0]);
        _exit(client(tv[1], 1));
    }
    close(tv[1]);
    tight_s = tv[0];
    fcntl(tight_s, F_SETFL, O_NONBLOCK);
    tight_in = epoll_create1(EPOLL_CLOEXEC);
    tight_out = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event e1 = {EPOLLIN, {.u64 = 1}}, e2 = {EPOLLIN | EPOLLET, {.u64 = 2}};
    if (epoll_ctl(tight_in, EPOLL_CTL_ADD, tight_s, &e1) || epoll_ctl(tight_out, EPOLL_CTL_ADD, tight_in, &e2))
        return 5;
    pid_t me = getpid();
    for (int b = 0; b < nbusy; b++) {
        pid_t p = fork();
        if (p == 0) busy(b, me);
        busy_pids[b] = p;
    }
    pthread_t tt;
    if (pthread_create(&tt, 0, tight, 0)) return 6;
    for (int k = 0; k < nc; k++) ping(&cls[k]);
    uint64_t t0 = now_ns(), end = t0 + (uint64_t)secs * 1000000000ull, quiet_end = 0;
    for (;;) {
        uint64_t t = now_ns();
        if (!stopping && t >= end) {
            stopping = 1;
            quiet_end = t + (uint64_t)STALL_S * 2 * 1000000000ull;
        }
        int open = 0;
        for (int k = 0; k < nc; k++) {
            struct cl *c = &cls[k];
            if (c->next == c->back) continue;
            open = 1;
            uint64_t at = c->sent_at[c->back % WINDOW];
            uint64_t age = t > at ? t - at : 0;
            if (age > (uint64_t)STALL_S * 1000000000ull) {
                stall(c, age);
                /* read it by hand (as a later wake would): the run goes on, the stall is counted */
                c->sent_at[c->back % WINDOW] = t;
                dispatch(c, 1);
            }
        }
        if (stopping && (!open || t >= quiet_end)) break;
        struct epoll_event ev[8];
        /* Half the loops only look (timeout 0), as a compositor busy rendering checks its sources: it is then
         * already running when a client's read wakes the compositor's socket and its write follows. */
        int n = epoll_wait(ep_out, ev, 8, left[0] || left[3] || rnd() % 2 ? 0 : 100);
        loops++;
        for (int i = 0; i < NINNER; i++)
            if (left[i]) run_inner(i);
        for (int j = 0; j < n; j++) {
            uint64_t d = ev[j].data.u64;
            if (d == 2000) {
                uint64_t x;
                while (read(tfd, &x, 8) == 8) {}
                for (int k = 0; k < nc; k++) ping(&cls[k]);
            } else if (d >= 1000 && d < 1000 + NINNER) {
                seen_at[d - 1000] = now_ns();
                run_inner((int)(d - 1000));
            }
        }
        /* the compositor's own work between dispatches (rendering), in one loop of four: in the others it goes
         * straight back to the outer epoll while the clients answer, the moment a look at an inner epoll races
         * with a client's write */
        uint32_t r = rnd();
        if (r % 4 == 0) {
            uint64_t w = now_ns() + r % 200000;
            while (now_ns() < w) {}
        }
    }
    tight_stop = 1;
    pthread_join(tt, 0);
    long fds_bad = 0, lost = 0;
    {
        struct msg q = {MAGIC, QUIT, 0, 0};
        send_msgs(tight_s, &q, 1, -1);
        int st = 0;
        waitpid(tight_pid, &st, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) tight_bad++;
    }
    for (int k = 0; k < nc; k++) {
        struct cl *c = &cls[k];
        lost += c->next - c->back;
        bad += c->bad;
        late += c->late;
        struct msg q = {MAGIC, QUIT, c->next, 0};
        send_msgs(c->s, &q, 1, -1);
    }
    int clients_ok = 1;
    for (int k = 0; k < nc; k++) {
        int st = 0;
        /* the client sends nothing after QUIT: whatever it still had for us was read above */
        waitpid(cls[k].pid, &st, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) {
            clients_ok = 0;
            if (verbose) printf("client %d ended with %#x\n", k, st);
        }
        dispatch(&cls[k], 1);
        if (cls[k].fds != cls[k].fds_said) fds_bad++;
    }
    for (int b = 0; b < nbusy; b++) {
        kill(busy_pids[b], SIGKILL);
        waitpid(busy_pids[b], 0, 0);
    }
    yes("every ping answered", lost == 0);
    yes("pongs in order and whole", bad == 0 && tight_bad == 0);
    yes("every descriptor arrived", fds_bad == 0);
    yes("clients saw everything in order", clients_ok);
    yes("no stall", stalls == 0 && tight_stalls == 0);
    yes("pongs came back", rounds > 0 && tight_rounds > 0);
    if (verbose)
        printf("counts: rounds=%ld late=%ld stalls=%ld worst_ms=%llu tight_rounds=%ld tight_stalls=%ld clients=%d "
               "busy=%d secs=%d\n",
               rounds, late, stalls, (unsigned long long)(worst / 1000000), tight_rounds, tight_stalls, nc, nbusy,
               secs);
    return lost == 0 && bad == 0 && tight_bad == 0 && fds_bad == 0 && clients_ok && stalls == 0 && tight_stalls == 0
               ? 0
               : 1;
}
