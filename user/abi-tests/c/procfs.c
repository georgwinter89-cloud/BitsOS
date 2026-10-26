/* AB-7a: /proc/<pid> as a program reads it. Prints only field names, formats and relations (never pids,
 * addresses, times or sizes): stat/status/statm, comm (prctl, writes, threads), cmdline (setproctitle),
 * environ, auxv, maps, smaps, smaps_rollup (AB-7c: the memory numbers and their relations), limits, exe/cwd/root, fd and fdinfo (files, eventfd, timerfd,
 * signalfd, epoll, pipes, memfd), task/<tid>, thread-self, oom_score_adj, zombies, another process's
 * files, personality, cgroup, mem, odd reads and writes (offsets, a bad buffer, NULs), the mount flags of
 * /proc/self/mounts and mountinfo, and getrusage's ru_maxrss.
 * Run as `abi-procfs "a b" c` (cases.txt) by an unprivileged user (uid 1000 on both sides). Another user's
 * process is $ABI_OTHER_PID (the V2 ktest starts one as root), else pid 1. */
#include "abi.h"
#include <ctype.h>
#include <dirent.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/signalfd.h>
#include <sys/stat.h>
#include <sys/timerfd.h>
#include <sys/wait.h>

extern char **environ;

static char buf[1 << 16];

/* The whole file into `buf` (NUL-terminated); its length or -errno. */
static long slurp(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        return -errno;
    long n = 0, r;
    while (n < (long)sizeof buf - 1 && (r = read(fd, buf + n, sizeof buf - 1 - n)) > 0)
        n += r;
    close(fd);
    buf[n] = 0;
    return n;
}

static long write_file(const char *path, const char *s, size_t len) {
    int fd = open(path, O_WRONLY);
    if (fd < 0)
        return -errno;
    long r = write(fd, s, len);
    if (r < 0)
        r = -errno;
    close(fd);
    return r;
}

static void show_open(const char *label, long r) {
    if (r < 0)
        printf("%s = -%s\n", label, ename((int)-r));
    else
        printf("%s = ok\n", label);
}

/* Field `n` (1-based) of a stat line, counted after the ") " that ends the name. */
static long long stat_field(const char *s, int n) {
    const char *p = strrchr(s, ')');
    if (!p || n < 3)
        return -1;
    p += 2;
    for (int i = 3; i < n; i++) {
        p = strchr(p, ' ');
        if (!p)
            return -1;
        p++;
    }
    return strtoll(p, NULL, 10);
}

static int stat_fields(const char *s) {
    const char *p = strrchr(s, ')');
    int n = 2;
    for (p = p ? p + 1 : s; *p; p++)
        if (*p == ' ')
            n++;
    return n;
}

/* The value text of "Key:\t..." in buf (up to the newline), or "-". */
static const char *key(const char *k) {
    static char v[512];
    size_t kl = strlen(k);
    for (char *l = buf; l && *l; l = strchr(l, '\n'), l = l ? l + 1 : NULL) {
        if (!strncmp(l, k, kl) && l[kl] == ':') {
            char *s = l + kl + 1;
            while (*s == '\t' || *s == ' ')
                s++;
            size_t n = strcspn(s, "\n");
            if (n >= sizeof v)
                n = sizeof v - 1;
            memcpy(v, s, n);
            v[n] = 0;
            return v;
        }
    }
    return "-";
}

/* Every "Key:" of buf in order, on one line. */
static void keys(const char *label) {
    printf("%s:", label);
    for (char *l = buf; l && *l; l = strchr(l, '\n'), l = l ? l + 1 : NULL) {
        size_t n = strcspn(l, ":\n");
        if (l[n] == ':')
            printf(" %.*s", (int)n, l);
    }
    printf("\n");
}

/* A text with every digit run replaced by N. */
static void print_masked(const char *label, const char *s) {
    printf("%s: ", label);
    for (; *s; s++) {
        /* ino: and sdev: are hex; a device number like 0xa has no decimal digit. */
        if ((!strncmp(s, "ino:", 4) || !strncmp(s, "sdev:", 5)) && isxdigit((unsigned char)s[*s == 'i' ? 4 : 5])) {
            int n = *s == 'i' ? 4 : 5;
            printf("%.*sN", n, s);
            s += n;
            while (isxdigit((unsigned char)s[1]))
                s++;
        } else if (*s >= '0' && *s <= '9') {
            while (s[1] >= '0' && s[1] <= '9')
                s++;
            putchar('N');
        } else if (*s == '\n')
            printf("\\n");
        else
            putchar(*s);
    }
    printf("\n");
}

static void comm_is(const char *label, const char *path) {
    long n = slurp(path);
    printf("%s = ", label);
    if (n < 0) {
        printf("-%s\n", ename((int)-n));
        return;
    }
    for (long i = 0; i < n; i++)
        if (buf[i] == '\n')
            printf("\\n");
        else
            putchar(buf[i]);
    printf("\n");
}

static void part_stat(void) {
    long n = slurp("/proc/self/stat");
    printf("stat read: %s\n", n > 0 ? "ok" : "failed");
    printf("stat fields = %d\n", stat_fields(buf));
    const char *o = strchr(buf, '(');
    printf("stat comm = %.*s\n", (int)(strrchr(buf, ')') - o + 1), o);
    printf("stat state = %c\n", strrchr(buf, ')')[2]);
    yes("stat pid", atoll(buf) == getpid());
    yes("stat ppid", stat_field(buf, 4) == getppid());
    yes("stat pgrp", stat_field(buf, 5) == getpgrp());
    yes("stat session", stat_field(buf, 6) == getsid(0));
    printf("stat num_threads = %lld\n", stat_field(buf, 20));
    printf("stat exit_signal = %lld\n", stat_field(buf, 38));
    printf("stat exit_code = %lld\n", stat_field(buf, 52));
    yes("stat vsize > 0", stat_field(buf, 23) > 0);
    yes("stat code range", stat_field(buf, 26) > 0 && stat_field(buf, 26) < stat_field(buf, 27));
    yes("stat startstack in the stack", stat_field(buf, 28) > (long long)(uintptr_t)&n - (1 << 20));
    long long as = stat_field(buf, 48), ae = stat_field(buf, 49), es = stat_field(buf, 50), ee = stat_field(buf, 51);
    yes("stat args then env", as < ae && ae <= es && es < ee);
    yes("stat data range", stat_field(buf, 45) <= stat_field(buf, 46));
    long long sbrk0 = stat_field(buf, 47);
    yes("stat start_brk <= brk", sbrk0 > 0 && sbrk0 <= (long long)(uintptr_t)sbrk(0));
    char st[4096];
    memcpy(st, buf, sizeof st);
    slurp("/proc/self/statm");
    long sz, res, sh, text, lib, data, dt;
    int got = sscanf(buf, "%ld %ld %ld %ld %ld %ld %ld", &sz, &res, &sh, &text, &lib, &data, &dt);
    printf("statm fields = %d\n", got);
    yes("statm size = vsize", sz == stat_field(st, 23) / 4096);
    yes("statm text > 0, lib and dt 0", text > 0 && lib == 0 && dt == 0);
}

static void part_status(void) {
    umask(027);
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    sigprocmask(SIG_BLOCK, &s, NULL);
    signal(SIGUSR2, SIG_IGN);
    struct sigaction sa = {0};
    sa.sa_handler = (void (*)(int))abort;
    sigaction(SIGWINCH, &sa, NULL);
    slurp("/proc/self/status");
    keys("status keys");
    printf("Name = %s\n", key("Name"));
    printf("Umask = %s\n", key("Umask"));
    printf("State = %s\n", key("State"));
    yes("Tgid = Pid = getpid", atol(key("Tgid")) == getpid() && atol(key("Pid")) == getpid());
    yes("PPid", atol(key("PPid")) == getppid());
    char uid[64];
    snprintf(uid, sizeof uid, "%d\t%d\t%d\t%d", getuid(), getuid(), getuid(), getuid());
    yes("Uid", !strcmp(key("Uid"), uid));
    printf("Kthread = %s\n", key("Kthread"));
    printf("Threads = %s\n", key("Threads"));
    printf("SigPnd = %s\n", key("SigPnd"));
    printf("SigBlk = %s\n", key("SigBlk"));
    printf("SigIgn = %s\n", key("SigIgn"));
    printf("SigCgt = %s\n", key("SigCgt"));
    printf("CapEff = %s\n", key("CapEff"));
    printf("NoNewPrivs = %s\n", key("NoNewPrivs"));
    printf("Seccomp = %s\n", key("Seccomp"));
    cpu_set_t c;
    sched_getaffinity(0, sizeof c, &c);
    int first = -1, last = -1, holes = 0;
    for (int i = 0; i < CPU_SETSIZE; i++)
        if (CPU_ISSET(i, &c)) {
            if (first < 0)
                first = i;
            else if (i != last + 1)
                holes = 1;
            last = i;
        }
    char want[32];
    snprintf(want, sizeof want, "%d-%d", first, last);
    yes("Cpus_allowed_list = affinity", !holes && !strcmp(key("Cpus_allowed_list"), want));
    printf("Mems_allowed_list = %s\n", key("Mems_allowed_list"));
    prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
    slurp("/proc/self/status");
    printf("NoNewPrivs after prctl = %s\n", key("NoNewPrivs"));
    sigprocmask(SIG_UNBLOCK, &s, NULL);
    umask(022);
}

static void part_comm(void) {
    comm_is("comm", "/proc/self/comm");
    prctl(PR_SET_NAME, "renamed", 0, 0, 0);
    comm_is("comm after PR_SET_NAME", "/proc/self/comm");
    slurp("/proc/self/stat");
    printf("stat comm after PR_SET_NAME = %.*s\n", (int)(strrchr(buf, ')') - strchr(buf, '(') + 1), strchr(buf, '('));
    printf("comm write = %ld\n", write_file("/proc/self/comm", "viaproc", 7));
    char n[17] = {0};
    prctl(PR_GET_NAME, n, 0, 0, 0);
    printf("PR_GET_NAME after the write = %s\n", n);
    printf("comm write long = %ld\n", write_file("/proc/self/comm", "abcdefghijklmnopqrst", 20));
    comm_is("comm after long write", "/proc/self/comm");
    printf("comm write newline = %ld\n", write_file("/proc/self/comm", "nl\n", 3));
    comm_is("comm after newline write", "/proc/self/comm");
    slurp("/proc/self/status");
    printf("status Name after newline write = %s\n", key("Name"));
    prctl(PR_SET_NAME, "abi-procfs", 0, 0, 0);
    struct stat s;
    stat("/proc/self/comm", &s);
    printf("comm mode = %o\n", s.st_mode & 07777);
}

static void part_cmdline(int argc, char **argv) {
    long n = slurp("/proc/self/cmdline");
    long want = 0;
    for (int i = 0; i < argc; i++)
        want += strlen(argv[i]) + 1;
    yes("cmdline = argv", n == want && !memcmp(buf, argv[0], n));
    printf("cmdline args = ");
    for (long i = strlen(buf) + 1; i < n; i++)
        putchar(buf[i] ? buf[i] : '|');
    printf("\n");
    argv[0][0] = 'Z';
    slurp("/proc/self/cmdline");
    printf("cmdline after argv[0][0]='Z' starts Z = %s\n", buf[0] == 'Z' ? "yes" : "no");
    argv[0][0] = 'a';
}

static void part_environ(void) {
    long n = slurp("/proc/self/environ");
    long want = 0;
    int same = 1;
    for (char **e = environ; *e; e++) {
        size_t l = strlen(*e) + 1;
        if (want + (long)l > n || memcmp(buf + want, *e, l))
            same = 0;
        want += l;
    }
    yes("environ = the initial environment", same && n == want);
    struct stat s;
    stat("/proc/self/environ", &s);
    printf("environ mode = %o\n", s.st_mode & 07777);
    n = slurp("/proc/self/auxv");
    uint64_t *a = (uint64_t *)buf;
    yes("auxv pairs end with AT_NULL", n > 16 && n % 16 == 0 && a[n / 8 - 2] == 0);
    int pagesz = 0;
    for (long i = 0; i + 1 < n / 8; i += 2)
        if (a[i] == 6 /* AT_PAGESZ */)
            pagesz = (int)a[i + 1];
    printf("auxv AT_PAGESZ = %d\n", pagesz);
}

/* Lines of /proc/self/maps naming `name` exactly: "perms offset-is-zero" each. */
static void part_maps(void) {
    char exe[512];
    long el = readlink("/proc/self/exe", exe, sizeof exe - 1);
    exe[el > 0 ? el : 0] = 0;
    struct stat es;
    stat("/proc/self/exe", &es);
    void *anon = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    sbrk(4096);
    slurp("/proc/self/maps");
    printf("maps exe lines:");
    int devino_ok = 1, nexe = 0;
    char anon_line[256] = "-";
    int heap = 0, stack = 0, vdso = 0;
    for (char *l = buf; l && *l;) {
        char *e = strchr(l, '\n');
        if (e)
            *e = 0;
        unsigned long s, t, off, ino;
        unsigned maj, min;
        char perms[8], name[512] = "";
        int k = sscanf(l, "%lx-%lx %7s %lx %x:%x %lu %511[^\n]", &s, &t, perms, &off, &maj, &min, &ino, name);
        char *nm = name;
        while (*nm == ' ')
            nm++;
        if (k >= 7 && !strcmp(nm, exe)) {
            printf(" %s/%s", perms, off ? "off" : "0");
            nexe++;
            /* Only the inode: on btrfs the maps line shows the super block's device, stat the subvolume's. */
            if (ino != es.st_ino)
                devino_ok = 0;
        }
        if (k >= 7 && s == (unsigned long)anon) {
            const char *p = strchr(l, ' ');
            snprintf(anon_line, sizeof anon_line, "\"%s\"", p ? p + 1 : l);
        }
        heap |= !strcmp(nm, "[heap]");
        stack |= !strcmp(nm, "[stack]");
        vdso |= !strcmp(nm, "[vdso]");
        if (k >= 7 && strlen(l) > 73 && !*nm)
            printf(" (unnamed long line)");
        l = e ? e + 1 : NULL;
    }
    printf("\n");
    yes("maps exe lines have the file's inode", nexe > 0 && devino_ok);
    printf("maps anonymous line = %s\n", anon_line);
    yes("maps [heap]", heap);
    yes("maps [stack]", stack);
    yes("maps [vdso]", vdso);
    long n = slurp("/proc/self/smaps_rollup");
    char *nl = strchr(buf, '\n');
    yes("smaps_rollup first line ends [rollup]", n > 0 && nl && nl - buf > 8 && !strncmp(nl - 8, "[rollup]", 8));
    memmove(buf, nl ? nl + 1 : buf, strlen(nl ? nl + 1 : buf) + 1);
    keys("smaps_rollup keys");
    yes("smaps_rollup Rss > 0", atol(key("Rss")) > 0);
    munmap(anon, 4096);
}

static void part_limits(void) {
    struct rlimit r = {0, 0};
    setrlimit(RLIMIT_CORE, &r);
    slurp("/proc/self/limits");
    printf("limits header = %.*s|\n", (int)strcspn(buf, "\n"), buf);
    printf("limits names:");
    for (char *l = strchr(buf, '\n'); l && l[1]; l = strchr(l + 1, '\n'))
        printf(" [%.25s]", l + 1);
    printf("\n");
    char *c = strstr(buf, "Max core file size");
    printf("limits core = %.*s|\n", c ? (int)strcspn(c, "\n") : 0, c ? c : "");
    char *n = strstr(buf, "Max nice priority");
    printf("limits nice unit = %s\n", n && n[strcspn(n, "\n") - 1] == ' ' ? "space" : "none");
}

static void part_links(void) {
    char t[512];
    long n;
    chdir("/tmp");
    n = readlink("/proc/self/cwd", t, sizeof t - 1);
    t[n > 0 ? n : 0] = 0;
    printf("cwd -> %s\n", n > 0 ? t : ename(errno));
    n = readlink("/proc/self/root", t, sizeof t - 1);
    t[n > 0 ? n : 0] = 0;
    printf("root -> %s\n", n > 0 ? t : ename(errno));
    struct stat a, b;
    n = readlink("/proc/self/exe", t, sizeof t - 1);
    t[n > 0 ? n : 0] = 0;
    yes("exe names the program", n > 0 && !stat(t, &a) && !stat("/proc/self/exe", &b) && a.st_ino == b.st_ino);
    lstat("/proc/self/exe", &a);
    printf("exe lstat mode = %o\n", a.st_mode);
    chdir("/");
    struct stat ps;
    stat("/proc/self", &ps);
    printf("pid dir mode = %o\n", ps.st_mode);
    DIR *d = opendir("/proc/self");
    int seen = 0;
    const char *want[] = {"task", "fd", "fdinfo", "environ", "auxv", "status", "personality", "limits", "comm", "cmdline",
                          "stat", "statm", "maps", "mem", "cwd", "root", "exe", "mounts", "mountinfo", "smaps",
                          "smaps_rollup", "cgroup", "oom_score", "oom_score_adj"};
    for (struct dirent *e; d && (e = readdir(d));)
        for (unsigned i = 0; i < sizeof want / sizeof *want; i++)
            if (!strcmp(e->d_name, want[i]))
                seen++;
    if (d)
        closedir(d);
    printf("pid dir has the %u names = %d\n", (unsigned)(sizeof want / sizeof *want), seen);
}

static void fdinfo_of(const char *label, int fd) {
    char p[64];
    snprintf(p, sizeof p, "/proc/self/fdinfo/%d", fd);
    long n = slurp(p);
    if (n < 0) {
        printf("%s fdinfo = -%s\n", label, ename((int)-n));
        return;
    }
    keys(label);
    printf("%s flags = %s\n", label, key("flags"));
    printf("%s pos = %s\n", label, key("pos"));
}

static void link_of(const char *label, int fd) {
    char p[64], t[256];
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    long n = readlink(p, t, sizeof t - 1);
    t[n > 0 ? n : 0] = 0;
    print_masked(label, n > 0 ? t : ename(errno));
}

static void part_fds(void) {
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-procfs-%d", getpid());
    int f = open(path, O_RDWR | O_CREAT | O_CLOEXEC, 0600);
    write(f, "hello world", 11);
    lseek(f, 5, SEEK_SET);
    fdinfo_of("file", f);
    link_of("file link has the path", f);
    unlink(path);
    link_of("unlinked file link", f);
    close(f);
    int e = eventfd(3, EFD_CLOEXEC | EFD_NONBLOCK);
    link_of("eventfd link", e);
    fdinfo_of("eventfd", e);
    printf("eventfd count line = %s\n", key("eventfd-count"));
    printf("eventfd semaphore line = %s\n", key("eventfd-semaphore"));
    int t = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC);
    struct itimerspec its = {{1, 0}, {100, 0}};
    timerfd_settime(t, 0, &its, NULL);
    link_of("timerfd link", t);
    fdinfo_of("timerfd", t);
    printf("timerfd clockid = %s\n", key("clockid"));
    printf("timerfd ticks = %s\n", key("ticks"));
    printf("timerfd settime flags = %s\n", key("settime flags"));
    printf("timerfd it_interval = %s\n", key("it_interval"));
    long sec = -1;
    sscanf(key("it_value"), "(%ld,", &sec);
    yes("timerfd it_value about 100 s", sec >= 90 && sec <= 100);
    sigset_t m;
    sigemptyset(&m);
    sigaddset(&m, SIGUSR1);
    sigaddset(&m, SIGKILL);
    int s = signalfd(-1, &m, SFD_CLOEXEC);
    link_of("signalfd link", s);
    fdinfo_of("signalfd", s);
    printf("signalfd sigmask = %s\n", key("sigmask"));
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = {.events = EPOLLIN, .data.u64 = 0x1234};
    epoll_ctl(ep, EPOLL_CTL_ADD, e, &ev);
    link_of("epoll link", ep);
    fdinfo_of("epoll", ep);
    char *tl = strstr(buf, "tfd:");
    print_masked("epoll tfd line", tl ? strtok(tl, "\n") : "-");
    int pp[2];
    pipe2(pp, O_NONBLOCK);
    link_of("pipe link", pp[0]);
    fdinfo_of("pipe read end", pp[0]);
    fdinfo_of("pipe write end", pp[1]);
    int mf = (int)syscall(SYS_memfd_create, "procfs", 1 /* MFD_CLOEXEC */);
    link_of("memfd link", mf);
    fdinfo_of("memfd", mf);
    fdinfo_of("closed fd", 999);
    struct stat st;
    stat("/proc/self/fdinfo", &st);
    printf("fdinfo dir mode = %o\n", st.st_mode & 07777);
    stat("/proc/self/fd", &st);
    printf("fd dir mode = %o\n", st.st_mode & 07777);
    close(e), close(t), close(s), close(ep), close(pp[0]), close(pp[1]), close(mf);
}

static int hold[2];
static volatile long worker_tid;

static void *worker(void *arg) {
    (void)arg;
    worker_tid = syscall(SYS_gettid);
    char c;
    read(hold[0], &c, 1);
    return NULL;
}

static void part_threads(void) {
    pipe(hold);
    pthread_t th;
    pthread_create(&th, NULL, worker, NULL);
    while (!worker_tid)
        sched_yield();
    slurp("/proc/self/status");
    printf("Threads with a worker = %s\n", key("Threads"));
    int n = 0, has_main = 0, has_worker = 0;
    DIR *d = opendir("/proc/self/task");
    for (struct dirent *e; d && (e = readdir(d));) {
        if (e->d_name[0] == '.')
            continue;
        n++;
        has_main |= atol(e->d_name) == getpid();
        has_worker |= atol(e->d_name) == worker_tid;
    }
    if (d)
        closedir(d);
    printf("task entries = %d\n", n);
    yes("task has main and worker", has_main && has_worker);
    char p[128];
    snprintf(p, sizeof p, "/proc/self/task/%ld/stat", worker_tid);
    slurp(p);
    yes("worker stat pid = tid", atoll(buf) == worker_tid);
    printf("worker stat num_threads = %lld\n", stat_field(buf, 20));
    printf("worker stat exit_signal = %lld\n", stat_field(buf, 38));
    printf("worker stat state = %c\n", strrchr(buf, ')')[2]);
    snprintf(p, sizeof p, "/proc/self/task/%ld/comm", worker_tid);
    comm_is("worker comm", p);
    printf("worker comm write = %ld\n", write_file(p, "worker", 6));
    comm_is("worker comm after write", p);
    comm_is("main comm unchanged", "/proc/self/comm");
    snprintf(p, sizeof p, "/proc/self/task/%ld/status", worker_tid);
    slurp(p);
    yes("worker status Pid = tid", atol(key("Pid")) == worker_tid);
    yes("worker status Tgid = pid", atol(key("Tgid")) == getpid());
    printf("worker status Name = %s\n", key("Name"));
    snprintf(p, sizeof p, "/proc/%ld", worker_tid);
    struct stat st;
    printf("/proc/<tid> = %s\n", stat(p, &st) ? ename(errno) : "ok");
    char t[128], want[128];
    long l = readlink("/proc/thread-self", t, sizeof t - 1);
    t[l > 0 ? l : 0] = 0;
    snprintf(want, sizeof want, "%d/task/%d", getpid(), getpid());
    yes("thread-self -> <pid>/task/<tid>", !strcmp(t, want));
    write(hold[1], "x", 1);
    pthread_join(th, NULL);
    close(hold[0]), close(hold[1]);
    slurp("/proc/self/status");
    printf("Threads after join = %s\n", key("Threads"));
}

static void part_oom(void) {
    /* The starting value is the session's (systemd gives 200 to apps): only its shape. */
    long n0 = slurp("/proc/self/oom_score_adj");
    yes("oom_score_adj is a number and a newline", n0 > 1 && buf[n0 - 1] == '\n');
    printf("write 500 = %ld\n", write_file("/proc/self/oom_score_adj", "500", 3));
    comm_is("oom_score_adj now", "/proc/self/oom_score_adj");
    printf("write 100 = %ld\n", write_file("/proc/self/oom_score_adj", "100\n", 4));
    printf("write -1 = %ld\n", write_file("/proc/self/oom_score_adj", "-1", 2));
    printf("write 1001 = %ld\n", write_file("/proc/self/oom_score_adj", "1001", 4));
    printf("write abc = %ld\n", write_file("/proc/self/oom_score_adj", "abc", 3));
    comm_is("oom_score_adj after", "/proc/self/oom_score_adj");
    slurp("/proc/self/oom_score");
    long sc = atol(buf);
    yes("oom_score in 0..2000", sc >= 0 && sc <= 2000);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        slurp("/proc/self/oom_score_adj");
        _exit(atoi(buf) == 100 ? 0 : 1);
    }
    int st;
    waitpid(c, &st, 0);
    yes("child inherits oom_score_adj", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

static void part_others(void) {
    int go[2];
    pipe(go);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        char ch;
        read(go[0], &ch, 1);
        if (ch == 'n')
            prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
        read(go[0], &ch, 1);
        _exit(0);
    }
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/maps", c);
    show_open("child maps", slurp(p));
    snprintf(p, sizeof p, "/proc/%d/environ", c);
    show_open("child environ", slurp(p));
    snprintf(p, sizeof p, "/proc/%d/comm", c);
    printf("child comm write = %ld\n", write_file(p, "x", 1));
    write(go[1], "n", 1);
    usleep(100000);
    snprintf(p, sizeof p, "/proc/%d/maps", c);
    show_open("non-dumpable child maps", slurp(p));
    snprintf(p, sizeof p, "/proc/%d/environ", c);
    show_open("non-dumpable child environ", slurp(p));
    snprintf(p, sizeof p, "/proc/%d/stat", c);
    show_open("non-dumpable child stat", slurp(p));
    printf("non-dumpable child stat startcode = %lld\n", stat_field(buf, 26));
    snprintf(p, sizeof p, "/proc/%d/fdinfo", c);
    DIR *d = opendir(p);
    printf("non-dumpable child fdinfo dir = %s\n", d ? "ok" : ename(errno));
    if (d)
        closedir(d);
    char t[256];
    snprintf(p, sizeof p, "/proc/%d/exe", c);
    printf("non-dumpable child exe readlink = %s\n", readlink(p, t, sizeof t) >= 0 ? "ok" : ename(errno));
    write(go[1], "x", 1);
    /* An ended child before wait: a zombie. */
    siginfo_t si;
    waitid(P_PID, c, &si, WEXITED | WNOWAIT);
    snprintf(p, sizeof p, "/proc/%d/stat", c);
    slurp(p);
    printf("zombie state = %c\n", strrchr(buf, ')') ? strrchr(buf, ')')[2] : '-');
    printf("zombie vsize = %lld\n", stat_field(buf, 23));
    snprintf(p, sizeof p, "/proc/%d/cmdline", c);
    printf("zombie cmdline length = %ld\n", slurp(p));
    snprintf(p, sizeof p, "/proc/%d/status", c);
    slurp(p);
    printf("zombie status VmSize = %s\n", key("VmSize"));
    printf("zombie status State = %s\n", key("State"));
    int st;
    waitpid(c, &st, 0);
    snprintf(p, sizeof p, "/proc/%d/stat", c);
    show_open("reaped child stat", slurp(p));
    close(go[0]), close(go[1]);
}

/* Another user's process (root's): what an unprivileged reader may and may not do. */
static void part_foreign(void) {
    const char *e = getenv("ABI_OTHER_PID");
    int pid = e ? atoi(e) : 1;
    char p[64], t[256];
    struct stat st;
    snprintf(p, sizeof p, "/proc/%d", pid);
    yes("other process is root's", !stat(p, &st) && st.st_uid == 0);
    yes("reader is not root", getuid() != 0);
    const char *files[] = {"maps", "environ", "auxv", "mem", "personality", "smaps", "smaps_rollup", "stat", "status", "cmdline", "comm", "limits", "cgroup"};
    for (unsigned i = 0; i < sizeof files / sizeof *files; i++) {
        snprintf(p, sizeof p, "/proc/%d/%s", pid, files[i]);
        int fd = open(p, O_RDONLY);
        printf("other %s open = %s\n", files[i], fd >= 0 ? "ok" : ename(errno));
        if (fd >= 0)
            close(fd);
    }
    const char *links[] = {"exe", "cwd", "root"};
    for (unsigned i = 0; i < 3; i++) {
        snprintf(p, sizeof p, "/proc/%d/%s", pid, links[i]);
        printf("other %s readlink = %s\n", links[i], readlink(p, t, sizeof t) >= 0 ? "ok" : ename(errno));
        printf("other %s lstat = %s\n", links[i], lstat(p, &st) ? ename(errno) : "ok");
    }
    snprintf(p, sizeof p, "/proc/%d/fd", pid);
    DIR *d = opendir(p);
    printf("other fd dir = %s\n", d ? "ok" : ename(errno));
    if (d)
        closedir(d);
    snprintf(p, sizeof p, "/proc/%d/fdinfo", pid);
    d = opendir(p);
    printf("other fdinfo dir = %s\n", d ? "ok" : ename(errno));
    if (d)
        closedir(d);
    snprintf(p, sizeof p, "/proc/%d/oom_score_adj", pid);
    printf("other oom_score_adj write = %ld\n", write_file(p, "0", 1));
    snprintf(p, sizeof p, "/proc/%d/comm", pid);
    printf("other comm write = %ld\n", write_file(p, "x", 1));
}

static int probe = 0x5a5a1234;

/* Reads at offsets and with a bad buffer, writes with NULs; personality, cgroup and mem of the own process. */
static void part_edges(char **argv) {
    int fd = open("/proc/self/cmdline", O_RDONLY);
    char b[64];
    long n = pread(fd, b, sizeof b, 3);
    long want = (long)strlen(argv[0]) - 3 + 1 + 4 + 2;
    yes("cmdline pread at 3 = the rest", n == want && !memcmp(b, argv[0] + 3, strlen(argv[0]) - 3));
    printf("cmdline pread past the end = %ld\n", pread(fd, b, sizeof b, 1 << 20));
    errno = 0;
    printf("cmdline read into a bad buffer = %ld %s\n", (long)read(fd, (void *)8, 16), ename(errno));
    close(fd);
    fd = open("/proc/self/status", O_RDONLY);
    errno = 0;
    printf("status read into a bad buffer = %ld %s\n", (long)read(fd, (void *)8, 16), ename(errno));
    close(fd);
    printf("comm write with a NUL = %ld\n", write_file("/proc/self/comm", "ab\0cd", 5));
    comm_is("comm after the NUL write", "/proc/self/comm");
    printf("comm write empty = %ld\n", write_file("/proc/self/comm", "", 0));
    comm_is("comm after the empty write", "/proc/self/comm");
    prctl(PR_SET_NAME, "abi-procfs", 0, 0, 0);
    printf("oom_score_adj write 105 NUL junk = %ld\n", write_file("/proc/self/oom_score_adj", "105\0junk", 9));
    comm_is("oom_score_adj after NUL write", "/proc/self/oom_score_adj");
    printf("oom_score_adj write 16 bytes = %ld\n", write_file("/proc/self/oom_score_adj", "100         junk", 16));
    printf("oom_score_adj write 2147483648 = %ld\n", write_file("/proc/self/oom_score_adj", "2147483648", 10));
    printf("oom_score_adj write 0x64 = %ld\n", write_file("/proc/self/oom_score_adj", "0x64", 4));
    comm_is("personality", "/proc/self/personality");
    struct stat st;
    stat("/proc/self/personality", &st);
    printf("personality mode = %o\n", st.st_mode & 07777);
    slurp("/proc/self/cgroup");
    yes("cgroup starts 0::/", !strncmp(buf, "0::/", 4));
    stat("/proc/self/mem", &st);
    printf("mem mode = %o size = %lld\n", st.st_mode & 07777, (long long)st.st_size);
    fd = open("/proc/self/mem", O_RDONLY);
    int v = 0;
    n = pread(fd, &v, sizeof v, (off_t)(uintptr_t)&probe);
    yes("mem pread of a variable", n == sizeof v && v == probe);
    errno = 0;
    printf("mem pread at 0 = %ld %s\n", (long)pread(fd, &v, sizeof v, 0), ename(errno));
    void *gap = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memset(gap, 'q', 8192);
    munmap((char *)gap + 4096, 4096);
    char two[8192];
    printf("mem pread across an unmapped page = %ld\n", (long)pread(fd, two, sizeof two, (off_t)(uintptr_t)gap));
    close(fd);
    munmap(gap, 4096);
}

/* The mount flags of `point` in /proc/self/mounts: rw/ro and the per-mount words, the file system's own
 * options (those with '=' or not in the list) left out. */
static const char *mount_flags(const char *point) {
    static char out[256];
    strcpy(out, "-");
    long n = slurp("/proc/self/mounts");
    for (char *l = buf; n > 0 && l && *l; l = strchr(l, '\n'), l = l ? l + 1 : NULL) {
        char src[256], dir[256], type[64], opts[512];
        if (sscanf(l, "%255s %255s %63s %511s", src, dir, type, opts) != 4 || strcmp(dir, point))
            continue;
        const char *words[] = {"rw", "ro", "nosuid", "nodev", "noexec", "noatime", "nodiratime", "relatime"};
        out[0] = 0;
        for (char *w = strtok(opts, ","); w; w = strtok(NULL, ","))
            for (unsigned i = 0; i < sizeof words / sizeof *words; i++)
                if (!strcmp(w, words[i]))
                    snprintf(out + strlen(out), sizeof out - strlen(out), "%s%s", out[0] ? "," : "", w);
    }
    return out;
}

static void part_mounts(void) {
    long n = slurp("/proc/self/mounts");
    int root = 0, proc = 0;
    for (char *l = buf; n > 0 && l && *l; l = strchr(l, '\n'), l = l ? l + 1 : NULL) {
        char src[256], dir[256], type[64], opts[256];
        int a, b;
        if (sscanf(l, "%255s %255s %63s %255s %d %d", src, dir, type, opts, &a, &b) == 6) {
            root |= !strcmp(dir, "/");
            proc |= !strcmp(dir, "/proc") && !strcmp(type, "proc");
        }
    }
    yes("mounts has /", root);
    yes("mounts has proc on /proc", proc);
    /* AB-7c: the mount flags (not the file systems' own options, which differ: sizes, gid=5). */
    const char *points[] = {"/proc", "/sys", "/dev", "/dev/pts", "/dev/shm"};
    for (unsigned i = 0; i < sizeof points / sizeof *points; i++)
        printf("mount flags of %s = %s\n", points[i], mount_flags(points[i]));
    n = slurp("/proc/self/mounts");
    char *pts = strstr(buf, " /dev/pts devpts ");
    yes("devpts options have mode=600", pts && strstr(pts, ",mode=600,") && strstr(pts, ",mode=600,") < strchr(pts + 1, '\n'));
    n = slurp("/proc/self/mountinfo");
    int shape = n > 0;
    for (char *l = buf; n > 0 && l && *l; l = strchr(l, '\n'), l = l ? l + 1 : NULL) {
        int id, parent;
        unsigned maj, min;
        char root[256], point[256];
        if (sscanf(l, "%d %d %u:%u %255s %255s", &id, &parent, &maj, &min, root, point) != 6 || !strstr(l, " - "))
            shape = 0;
    }
    yes("mountinfo lines have the shape", shape);
    /* mountinfo of /proc: the mount's flags after the point, the super block's after the source. */
    char mo[256] = "-", so[256] = "-";
    for (char *l = buf; n > 0 && l && *l; l = strchr(l, '\n'), l = l ? l + 1 : NULL) {
        char point[256], opts[256], tail[512];
        if (sscanf(l, "%*d %*d %*u:%*u %*s %255s %255s", point, opts) == 2 && !strcmp(point, "/proc")) {
            strcpy(mo, opts);
            char *dash = strstr(l, " - ");
            if (dash && sscanf(dash, " - %*s %*s %511s", tail) == 1)
                snprintf(so, sizeof so, "%s", tail);
        }
    }
    printf("mountinfo /proc mount options = %s\n", mo);
    printf("mountinfo /proc super options = %s\n", so);
}

/* AB-7c: the numbers of status, statm, smaps and smaps_rollup and how they move. */
static long status_kb(const char *k) {
    slurp("/proc/self/status");
    return atol(key(k));
}

/* The smaps entry starting at `at`: its "Key:" value in kB, or -1. */
static long smaps_kb(void *at, const char *k) {
    slurp("/proc/self/smaps");
    char head[32];
    snprintf(head, sizeof head, "%lx-", (unsigned long)at);
    char *e = strstr(buf, head);
    if (!e || (e != buf && e[-1] != '\n'))
        return -1;
    size_t kl = strlen(k);
    for (char *l = strchr(e, '\n'); l && l[1]; l = strchr(l + 1, '\n')) {
        char *x = l + 1;
        if (strchr(x, ' ') && strchr(x, ' ') < strchr(x, ':'))
            return -1; /* the next entry */
        if (!strncmp(x, k, kl) && x[kl] == ':')
            return atol(x + kl + 1);
    }
    return -1;
}

/* The VmFlags of the entry at `at`, without "sd" (soft-dirty, a CachyOS kernel option V2 does not have). */
static const char *smaps_flags(void *at) {
    static char out[128];
    out[0] = 0;
    slurp("/proc/self/smaps");
    char head[32];
    snprintf(head, sizeof head, "%lx-", (unsigned long)at);
    char *e = strstr(buf, head);
    char *f = e ? strstr(e, "VmFlags:") : NULL;
    if (!f)
        return "-";
    char *end = strchr(f, '\n');
    for (char *w = f + 8; w < end;) {
        while (*w == ' ')
            w++;
        size_t n = strcspn(w, " \n");
        if (n && !(n == 2 && !strncmp(w, "sd", 2)))
            snprintf(out + strlen(out), sizeof out - strlen(out), "%s%.*s", out[0] ? " " : "", (int)n, w);
        w += n;
    }
    return out;
}

static void part_memory(void) {
    slurp("/proc/self/status");
    long hwm = atol(key("VmHWM")), rss = atol(key("VmRSS")), anon = atol(key("RssAnon")), file = atol(key("RssFile")),
         shm = atol(key("RssShmem")), pte = atol(key("VmPTE")), peak = atol(key("VmPeak")), size = atol(key("VmSize"));
    yes("VmHWM >= VmRSS > 0", hwm >= rss && rss > 0);
    yes("RssAnon + RssFile + RssShmem = VmRSS", anon + file + shm == rss);
    yes("VmPTE > 0", pte > 0);
    yes("VmPeak >= VmSize", peak >= size);
    slurp("/proc/self/statm");
    long sz, res, shared;
    yes("statm resident = VmRSS, shared = RssFile + RssShmem",
        sscanf(buf, "%ld %ld %ld", &sz, &res, &shared) == 3 && res * 4 == status_kb("VmRSS") && shared * 4 == status_kb("RssFile") + status_kb("RssShmem"));
    /* 64 pages of our own, cut out of a larger mapping so no neighbour merges with it. */
    char *big = mmap(NULL, 66 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    munmap(big, 4096);
    munmap(big + 65 * 4096, 4096);
    char *m = big + 4096;
    printf("untouched smaps Rss = %ld\n", smaps_kb(m, "Rss"));
    printf("untouched smaps Size = %ld\n", smaps_kb(m, "Size"));
    memset(m, 1, 64 * 4096);
    const char *ks[] = {"Rss", "Pss", "Pss_Dirty", "Shared_Clean", "Shared_Dirty", "Private_Clean", "Private_Dirty", "Referenced", "Anonymous", "Swap", "Locked", "KernelPageSize", "THPeligible"};
    for (unsigned i = 0; i < sizeof ks / sizeof *ks; i++)
        printf("touched smaps %s = %ld\n", ks[i], smaps_kb(m, ks[i]));
    printf("touched smaps VmFlags = %s\n", smaps_flags(m));
    /* After a fork the pages are shared copy-on-write: Pss halves, they count as shared and dirty. */
    int go[2];
    pipe(go);
    pid_t c = fork();
    if (c == 0) {
        char x;
        read(go[0], &x, 1);
        _exit(0);
    }
    printf("forked smaps Rss = %ld\n", smaps_kb(m, "Rss"));
    printf("forked smaps Pss = %ld\n", smaps_kb(m, "Pss"));
    printf("forked smaps Shared_Dirty = %ld\n", smaps_kb(m, "Shared_Dirty"));
    printf("forked smaps Private_Dirty = %ld\n", smaps_kb(m, "Private_Dirty"));
    write(go[1], "x", 1);
    int st;
    waitpid(c, &st, 0);
    close(go[0]);
    close(go[1]);
    printf("after the child smaps Pss = %ld\n", smaps_kb(m, "Pss"));
    /* status' counters are approximate on Linux (per-CPU batches): only relations are checked there. */
    munmap(m, 64 * 4096);
    printf("unmapped smaps entry = %ld\n", smaps_kb(m, "Rss"));
    yes("after munmap VmHWM >= VmRSS", status_kb("VmHWM") >= status_kb("VmRSS"));
    /* smaps sums to smaps_rollup (nothing maps or touches memory between the two reads). */
    long n = slurp("/proc/self/smaps");
    long sum = 0, ents = 0, flags = 0;
    for (char *l = buf; n > 0 && l && *l; l = strchr(l, '\n'), l = l ? l + 1 : NULL) {
        if (!strncmp(l, "Rss:", 4))
            sum += atol(l + 4), ents++;
        flags += !strncmp(l, "VmFlags:", 8);
    }
    slurp("/proc/self/smaps_rollup");
    long rr = atol(key("Rss")), pss = atol(key("Pss")), parts = atol(key("Pss_Anon")) + atol(key("Pss_File")) + atol(key("Pss_Shmem"));
    yes("smaps entries each have VmFlags", ents > 0 && ents == flags);
    yes("smaps Rss sums to smaps_rollup Rss", sum == rr);
    yes("rollup 0 < Pss <= Rss", pss > 0 && pss <= rr);
    yes("rollup Pss_Anon + Pss_File + Pss_Shmem = Pss (within 2 kB)", parts <= pss && pss - parts <= 2);
    /* ru_maxrss: our peak (at least VmHWM), and the children's: one that touched 4 MiB. */
    struct rusage ru;
    getrusage(RUSAGE_SELF, &ru);
    yes("ru_maxrss >= VmHWM", ru.ru_maxrss >= status_kb("VmHWM"));
    getrusage(RUSAGE_THREAD, &ru);
    yes("RUSAGE_THREAD ru_maxrss >= VmHWM", ru.ru_maxrss >= status_kb("VmHWM"));
    c = fork();
    if (c == 0) {
        char *k = mmap(NULL, 1024 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        memset(k, 2, 1024 * 4096);
        _exit(0);
    }
    getrusage(RUSAGE_CHILDREN, &ru);
    long earlier = ru.ru_maxrss;
    waitpid(c, &st, 0);
    getrusage(RUSAGE_CHILDREN, &ru);
    yes("RUSAGE_CHILDREN ru_maxrss covers the 4 MiB child", ru.ru_maxrss >= 4096 && ru.ru_maxrss >= earlier);
    /* wait4's rusage is the collected child's (RUSAGE_BOTH), and a bigger child raises CHILDREN. */
    earlier = ru.ru_maxrss;
    c = fork();
    if (c == 0) {
        char *k = mmap(NULL, 2048 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        memset(k, 3, 2048 * 4096);
        _exit(0);
    }
    memset(&ru, 0, sizeof ru);
    wait4(c, &st, 0, &ru);
    yes("wait4 ru_maxrss covers the 8 MiB child", ru.ru_maxrss >= 8192);
    getrusage(RUSAGE_CHILDREN, &ru);
    yes("RUSAGE_CHILDREN rises to the 8 MiB child", ru.ru_maxrss >= 8192 && ru.ru_maxrss >= earlier);
    /* The peak of an image left by execve stays the process's (signal->maxrss), for SELF and THREAD. */
    c = fork();
    if (c == 0) {
        char *k = mmap(NULL, 4096 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        memset(k, 4, 4096 * 4096);
        char *a[] = {"abi-procfs", "--maxrss-after-exec", NULL};
        execv("/proc/self/exe", a);
        _exit(9);
    }
    waitpid(c, &st, 0);
    printf("ru_maxrss after execve keeps the 16 MiB peak (SELF, THREAD) = %s\n",
           WIFEXITED(st) ? (WEXITSTATUS(st) == 0 ? "yes" : WEXITSTATUS(st) == 1 ? "SELF lost" : WEXITSTATUS(st) == 2 ? "THREAD lost" : "exec failed") : "killed");
    getrusage(RUSAGE_CHILDREN, &ru);
    yes("RUSAGE_CHILDREN rises to the 16 MiB child across its execve", ru.ru_maxrss >= 16384);
}

/* The image execve'd by part_memory: exit 0 when both SELF and THREAD keep the earlier image's peak. */
static int maxrss_after_exec(void) {
    struct rusage ru;
    getrusage(RUSAGE_SELF, &ru);
    if (ru.ru_maxrss < 16384)
        return 1;
    getrusage(RUSAGE_THREAD, &ru);
    return ru.ru_maxrss < 16384 ? 2 : 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "--maxrss-after-exec"))
        return maxrss_after_exec();
    part_stat();
    part_status();
    part_comm();
    part_cmdline(argc, argv);
    part_environ();
    part_maps();
    part_limits();
    part_links();
    part_fds();
    part_threads();
    part_oom();
    part_others();
    part_foreign();
    part_edges(argv);
    part_mounts();
    part_memory();
    return 0;
}
