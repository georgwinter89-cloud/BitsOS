// YT-4a: the calls behind V2's nplb failures, one line each (`name result errno`), so the same program on CachyOS
// gives the Linux reference answer (no sound, no device, nothing written outside the working directory).
// Build: gcc -O2 -static -pthread -o nplb-probe host/tests/yt/nplb-probe.c
#define _GNU_SOURCE
#include <errno.h>
#include <netinet/in.h>
#include <pthread.h>
#include <netinet/tcp.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static void line(const char *what, long r) {
    printf("%s %ld %s\n", what, r, r < 0 ? strerrorname_np(errno) : "-");
}

static void opts(int family, int type, const char *tag) {
    int fd = socket(family, type, 0);
    char w[80];
    snprintf(w, sizeof w, "%s.socket", tag);
    line(w, fd < 0 ? -1 : 0);
    if (fd < 0) return;
    int one = 1, sz = 65536, secs = 30;
    struct { int level, name; void *v; const char *n; } o[] = {
        {SOL_SOCKET, SO_REUSEADDR, &one, "SO_REUSEADDR"}, {SOL_SOCKET, SO_BROADCAST, &one, "SO_BROADCAST"},
        {SOL_SOCKET, SO_RCVBUF, &sz, "SO_RCVBUF"}, {SOL_SOCKET, SO_SNDBUF, &sz, "SO_SNDBUF"},
        {SOL_SOCKET, SO_KEEPALIVE, &one, "SO_KEEPALIVE"}, {IPPROTO_TCP, TCP_KEEPIDLE, &secs, "TCP_KEEPIDLE"},
        {IPPROTO_TCP, TCP_KEEPINTVL, &secs, "TCP_KEEPINTVL"}, {IPPROTO_TCP, TCP_NODELAY, &one, "TCP_NODELAY"},
    };
    for (unsigned i = 0; i < sizeof o / sizeof o[0]; i++) {
        if (type == SOCK_DGRAM && o[i].level == IPPROTO_TCP) continue;
        errno = 0;
        snprintf(w, sizeof w, "%s.%s", tag, o[i].n);
        line(w, setsockopt(fd, o[i].level, o[i].name, o[i].v, sizeof(int)));
    }
    close(fd);
}

static void *nop(void *arg) {
    return arg;
}

static void *other(void *arg) {
    (void)arg;
    sleep(1);
    return NULL;
}

int main(int argc, char **argv) {
    char buf[256];
    errno = 0;
    line("readlink.empty", readlink("", buf, sizeof buf));
    long n = readlink("/etc/localtime", buf, sizeof buf - 1);
    line("readlink./etc/localtime", n);
    if (n > 0) printf("  -> %.*s\n", (int)n, buf);
    struct sched_param sp = {.sched_priority = 1};
    errno = 0;
    line("sched_setscheduler.tid.RR", sched_setscheduler((pid_t)syscall(SYS_gettid), SCHED_RR, &sp));
    errno = 0;
    line("sched_setscheduler.pid.RR", sched_setscheduler(getpid(), SCHED_RR, &sp));
    // As nplb's PosixThreadAttrTest.SchedParam: another thread of this process (glibc: sched_setscheduler(its tid)).
    pthread_t th;
    pthread_create(&th, NULL, other, NULL);
    int pr = pthread_setschedparam(th, SCHED_RR, &sp);
    errno = pr;
    line("pthread_setschedparam.other.RR", pr ? -1 : 0);
    sp.sched_priority = 0;
    pr = pthread_setschedparam(th, SCHED_OTHER, &sp);
    errno = pr;
    line("pthread_setschedparam.other.OTHER", pr ? -1 : 0);
    pthread_join(th, NULL);
    // The test's own thread returns at once (NopThreadFunc): when it has ended before the call, glibc answers ESRCH
    // (its tid is 0, INVALID_TD_P), so the answer depends on who runs first. The test's order (OTHER, get, FIFO, RR);
    // how often one of them gives ESRCH, of 100:
    int esrch = 0;
    for (int i = 0; i < 100; i++) {
        pthread_create(&th, NULL, nop, NULL);
        int pol;
        sp.sched_priority = 0;
        int a = pthread_setschedparam(th, SCHED_OTHER, &sp);
        pthread_getschedparam(th, &pol, &sp);
        sp.sched_priority = 1;
        int b = pthread_setschedparam(th, SCHED_FIFO, &sp);
        int c = pthread_setschedparam(th, SCHED_RR, &sp);
        if (a == ESRCH || b == ESRCH || c == ESRCH) esrch++;
        pthread_join(th, NULL);
    }
    printf("pthread_setschedparam.nop_thread ESRCH %d/100\n", esrch);
    struct statvfs v;
    const char *dir = argc > 1 ? argv[1] : "/tmp";
    long r = statvfs(dir, &v);
    line("statvfs", r);
    if (r == 0) printf("  %s f_files=%lu f_ffree=%lu f_blocks=%lu\n", dir, v.f_files, v.f_ffree, v.f_blocks);
    struct stat st;
    const char *exe = argc > 2 ? argv[2] : "/proc/self/exe";
    r = stat(exe, &st);
    line("stat.exe", r);
    if (r == 0) printf("  %s ctime-now=%ld s\n", exe, (long)st.st_ctim.tv_sec - (long)time(NULL));
    // Cobalt's DNS config watch (Chromium FilePathWatcherInotify: a watch on every component of the path).
    int in = inotify_init1(IN_CLOEXEC | IN_NONBLOCK);
    line("inotify_init1", in);
    const unsigned mask = IN_ATTRIB | IN_CREATE | IN_DELETE | IN_CLOSE_WRITE | IN_MOVE | IN_ONLYDIR;
    const char *paths[] = {"/", "/etc", "/etc/resolv.conf", "/etc/nsswitch.conf", "/etc/hosts"};
    for (unsigned i = 0; in >= 0 && i < sizeof paths / sizeof paths[0]; i++) {
        char w[80];
        snprintf(w, sizeof w, "inotify_add_watch.%s%s", paths[i], i < 2 ? ".onlydir" : "");
        errno = 0;
        line(w, inotify_add_watch(in, paths[i], i < 2 ? mask : (mask & ~IN_ONLYDIR) | IN_MODIFY | IN_DELETE_SELF | IN_MOVE_SELF));
    }
    opts(AF_INET, SOCK_STREAM, "tcp4");
    opts(AF_INET6, SOCK_STREAM, "tcp6");
    opts(AF_INET6, SOCK_DGRAM, "udp6");
    return 0;
}
