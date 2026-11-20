/* DESK-LEFT 5 (WY): a Chromium-shaped process tree ends, and every process of it must be reaped. On the Mini-PC
 * four brave processes stayed zombies for good after the browser was closed. The tree as Brave builds it
 * (plan/spec/programs.md §1.4): the browser; its sandboxed zygote in a new user + pid namespace, whose pid 1
 * is an init that reaps (Chromium's CreateInitProcessReaper: waitid(P_ALL) until its zygote ended); the zygote's
 * renderers, each pid 1 of a nested pid namespace; a plain zygote without namespaces; and in both zygotes a child
 * that ends at once while its zygote lives on (a zombie of the zygote until the zygote ends). Every process
 * blocks on a pipe the browser holds, so the browser's end closes them all, as Chromium's IPC does.
 *
 * Modes: `close` (the browser ends by itself), `term` (SIGTERM to every process of the tree in pid order, as
 * the desk's round does), `kill` (SIGKILL to the browser only, as bitscomp's ending does after 2 s). With
 * `sub-` the test makes itself a child subreaper and reaps the orphans itself; without it the orphans go to
 * init. Either way: after the end no process of the tree may be left, and the test says which process the
 * subreaper reaped (by role, never pids). */
#include "ns.h"
#include <sys/prctl.h>
#include <time.h>

enum { BROWSER, INIT, ZYGOTE, ZKID, RENDER1, RENDER2, PLAIN, PKID, ROLES };
static const char *role_name[ROLES] = { "browser", "init", "zygote", "zygote-child", "renderer-1", "renderer-2", "plain-zygote", "plain-child" };
static int report_w = -1, hold_r = -1;

static long global_pid(void) {
    long v[8];
    return status_fields("NSpid:", v, 8) > 0 ? v[0] : -1;
}

static void tell(int role) {
    long m[2] = { role, global_pid() };
    if (write(report_w, m, sizeof m) != sizeof m)
        _exit(90);
}

/* Until the browser's end (EOF on the pipe only the browser can write). */
static void hold(void) {
    char c;
    while (read(hold_r, &c, 1) != 0 && errno == EINTR)
        ;
}

static void on_term(int s) {
    (void)s;
    _exit(0);
}

static void nothing(int s) {
    (void)s;
}

static void renderer(int role) {
    signal(SIGTERM, on_term);
    tell(role);
    hold();
    _exit(0);
}

static void zygote(void) {
    tell(ZYGOTE);
    if (fork() == 0) {
        tell(ZKID);
        _exit(0);
    }
    for (int r = RENDER1; r <= RENDER2; r++)
        if (rclone(CLONE_NEWPID | SIGCHLD) == 0)
            renderer(r);
    hold();
    _exit(0);
}

/* Chromium's init process reaper: reaps everything, ends when its zygote did. */
static void init_reaper(void) {
    struct sigaction a;
    memset(&a, 0, sizeof a);
    a.sa_handler = nothing;
    sigaction(SIGCHLD, &a, 0);
    tell(INIT);
    pid_t z = fork();
    if (z == 0)
        zygote();
    for (;;) {
        siginfo_t si;
        memset(&si, 0, sizeof si);
        if (waitid(P_ALL, 0, &si, WEXITED) == 0) {
            if (si.si_pid == z)
                _exit(0);
        } else if (errno != EINTR)
            _exit(1);
    }
}

static void browser(int go_r) {
    int h[2];
    if (pipe(h))
        _exit(91);
    hold_r = h[0];
    long i = rclone(CLONE_NEWUSER | CLONE_NEWPID | SIGCHLD);
    if (i == 0) {
        close(h[1]);
        close(go_r);
        init_reaper();
    }
    if (fork() == 0) {
        close(h[1]);
        close(go_r);
        tell(PLAIN);
        if (fork() == 0) {
            tell(PKID);
            _exit(0);
        }
        hold();
        _exit(0);
    }
    tell(BROWSER);
    char c;
    while (read(go_r, &c, 1) < 0 && errno == EINTR)
        ;
    _exit(0);
}

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* The state letter of /proc/PID/stat ('-' when there is none) and its parent's pid. */
static char state_of(long pid, long *ppid) {
    char path[64], b[512];
    snprintf(path, sizeof path, "/proc/%ld/stat", pid);
    if (rd(path, b, sizeof b) <= 0)
        return '-';
    char *e = strrchr(b, ')');
    char st = '?';
    if (!e || sscanf(e + 1, " %c %ld", &st, ppid) != 2)
        return '?';
    return st;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IOLBF, 0);
    const char *mode = argc > 1 ? argv[1] : "close";
    int sub = strncmp(mode, "sub-", 4) == 0;
    const char *how = sub ? mode + 4 : mode;
    if (sub)
        R("prctl(PR_SET_CHILD_SUBREAPER, 1)", prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0));
    int rp[2], go[2];
    if (pipe(rp) || pipe(go))
        return 1;
    report_w = rp[1];
    pid_t b = fork();
    if (b == 0) {
        close(rp[0]);
        close(go[1]);
        browser(go[0]);
    }
    close(rp[1]);
    close(go[0]);
    long pid[ROLES];
    int seen = 0;
    for (int k = 0; k < ROLES; k++)
        pid[k] = 0;
    for (int k = 0; k < ROLES; k++) {
        long m[2];
        if (read(rp[0], m, sizeof m) != sizeof m || m[0] < 0 || m[0] >= ROLES)
            break;
        pid[m[0]] = m[1];
        seen++;
    }
    printf("%s: %d of %d processes started\n", mode, seen, ROLES);
    /* The two at-once children are zombies of their zygotes before anything ends. */
    for (double end = now() + 5; now() < end;) {
        long pp;
        if (state_of(pid[ZKID], &pp) == 'Z' && state_of(pid[PKID], &pp) == 'Z')
            break;
        usleep(10000);
    }
    long pp = 0;
    printf("zygote-child is a zombie of its living zygote = %s\n", yn(state_of(pid[ZKID], &pp) == 'Z' && pp == pid[ZYGOTE]));
    printf("plain-child is a zombie of its living zygote = %s\n", yn(state_of(pid[PKID], &pp) == 'Z' && pp == pid[PLAIN]));
    if (strcmp(how, "term") == 0) {
        long order[ROLES];
        memcpy(order, pid, sizeof order);
        for (int x = 0; x < ROLES; x++)
            for (int y = x + 1; y < ROLES; y++)
                if (order[y] < order[x]) {
                    long t = order[x];
                    order[x] = order[y];
                    order[y] = t;
                }
        for (int x = 0; x < ROLES; x++)
            if (order[x] > 1)
                kill(order[x], SIGTERM);
    } else if (strcmp(how, "kill") == 0)
        kill(b, SIGKILL);
    else
        close(go[1]);
    reap("browser", b);
    int reaped[ROLES] = { 0 }, others = 0;
    double end = now() + 15;
    for (;;) {
        int left = 0;
        for (int k = 0; k < ROLES; k++)
            if (state_of(pid[k], &pp) != '-')
                left++;
        if (sub) {
            int st;
            pid_t r;
            while ((r = waitpid(-1, &st, WNOHANG)) > 0) {
                int k = 0;
                while (k < ROLES && pid[k] != r)
                    k++;
                if (k < ROLES)
                    reaped[k]++;
                else
                    others++;
                if (k == INIT)
                    printf("init: %s %d\n", WIFEXITED(st) ? "exit" : "signal", WIFEXITED(st) ? WEXITSTATUS(st) : WTERMSIG(st));
            }
        }
        if (left == 0 || now() > end)
            break;
        usleep(20000);
    }
    int left = 0;
    for (int k = 0; k < ROLES; k++) {
        char st = state_of(pid[k], &pp);
        if (st == '-')
            continue;
        left++;
        const char *parent = pp == 0 ? "none (the kernel)" : pp == 1 ? "init" : pp == getpid() ? "this test" : "another process";
        for (int j = 0; j < ROLES; j++)
            if (pid[j] == pp)
                parent = role_name[j];
        printf("left: %s state %c, parent %s\n", role_name[k], st, parent);
    }
    printf("processes of the tree left after the end: %d\n", left);
    if (sub) {
        printf("reaped by this subreaper:");
        for (int k = 0; k < ROLES; k++)
            if (reaped[k])
                printf(" %s", role_name[k]);
        printf("%s\n", others ? " (and others)" : "");
    }
    return 0;
}
