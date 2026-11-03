/* SX-2: pids across pid namespaces as the initial /proc mount shows them (linux-6.6/fs/proc/array.c NSpid,
 * NStgid, NSpgid, NSsid; kernel/pid.c pid_vnr): getpid/getppid inside, the NS* lines with one number per
 * level (global first, then each namespace down to the caller's), /proc/self naming the global pid, kill and
 * waitid with namespace numbers. Pids appear only as levels and relations. */
#include "ns.h"
#include <sys/syscall.h>

static long outer_pid;

static void levels(const char *who, long expect_first) {
    long v[8];
    const char *keys[] = { "NSpid:", "NStgid:", "NSpgid:", "NSsid:" };
    for (int k = 0; k < 4; k++) {
        int n = status_fields(keys[k], v, 8);
        printf("%s: %s %d levels", who, keys[k], n);
        if (k < 2 && n > 0)
            printf(", last is getpid() = %s, first is the global pid = %s", yn(v[n - 1] == getpid()), yn(expect_first == 0 || v[0] == expect_first));
        printf("\n");
    }
}

static long first_nspid(void) {
    long v[8];
    return status_fields("NSpid:", v, 8) > 0 ? v[0] : -1;
}

static int nested(void) {
    printf("nested: getpid %d getppid %d\n", (int)getpid(), (int)getppid());
    levels("nested", 0);
    return 0;
}

static int zygote(int w) {
    printf("zygote: getpid %d getppid %d\n", (int)getpid(), (int)getppid());
    long g = first_nspid();
    if (write(w, &g, sizeof g) != sizeof g)
        return 1;
    levels("zygote", 0);
    long t[8];
    status_fields("Tgid:", t, 1);
    printf("zygote: Tgid is the global pid = %s\n", yn(t[0] == g));
    status_fields("PPid:", t, 1);
    printf("zygote: PPid is the parent's global pid = %s\n", yn(t[0] == outer_pid));
    char b[32];
    long n = readlink("/proc/self", b, sizeof b - 1);
    b[n < 0 ? 0 : n] = 0;
    printf("zygote: /proc/self names the global pid = %s\n", yn(n > 0 && atol(b) == g));
    R("zygote: kill(the outer pid, 0)", kill(outer_pid, 0));
    long c = rclone(CLONE_NEWPID | SIGCHLD);
    if (c == 0)
        _exit(nested());
    siginfo_t si;
    memset(&si, 0, sizeof si);
    R("zygote: waitid(P_PID, 2)", waitid(P_PID, 2, &si, WEXITED));
    printf("zygote: the nested child's pid here = %ld\n", c);
    printf("zygote: si_pid %d si_status %d\n", (int)si.si_pid, si.si_status);
    pid_t s = fork();
    if (s == 0) {
        for (;;)
            pause();
    }
    printf("zygote: a sleeper is pid %d\n", (int)s);
    R("zygote: kill(3, SIGKILL)", kill(3, SIGKILL));
    reap("zygote: sleeper", s);
    R("zygote: kill(-1, 0) with nothing else here", kill(-1, 0));
    return 0;
}

int main(void) {
    setvbuf(stdout, 0, _IOLBF, 0);
    outer_pid = getpid();
    levels("outside", outer_pid);
    int p[2];
    if (pipe(p))
        return 1;
    long z = rclone(CLONE_NEWUSER | CLONE_NEWPID | SIGCHLD);
    if (z == 0) {
        close(p[0]);
        _exit(zygote(p[1]));
    }
    close(p[1]);
    long g = 0;
    if (read(p[0], &g, sizeof g) != sizeof g)
        g = -1;
    reap("zygote", z);
    printf("the zygote's global pid is what clone returned = %s\n", yn(g == z));
    return 0;
}
