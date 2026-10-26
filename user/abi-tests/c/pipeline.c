/* AB-4: what a shell does for `gen 5 | upper | count` and `upper < file > file2`: fork per stage, its own
 * process group for the job (the shell's setpgid on both sides), dup2 onto 0/1, close the rest, execve the
 * stage, wait for all, the last stage's status is the pipeline's. The stages are this program again
 * (argv[1]). With pipe2 the stages run at once over pipes; a kernel without pipe2 (V2 before FS-3) gets the
 * same job with temporary files between the stages, one stage after the other, so the lines are the same. */
#include "abi.h"
#include <ctype.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/wait.h>

static char self[4096];
static char *env0[] = {NULL};

static void flush(void) {
    fflush(stdout);
    fflush(stderr);
}

static int stage(int argc, char **argv) {
    const char *m = argv[1];
    char buf[4096];
    if (!strcmp(m, "gen")) {
        int n = argc > 2 ? atoi(argv[2]) : 3;
        for (int i = 1; i <= n; i++)
            printf("line %d of the generator\n", i);
        return 0;
    }
    if (!strcmp(m, "upper")) {
        ssize_t r;
        while ((r = read(0, buf, sizeof buf)) > 0) {
            for (ssize_t i = 0; i < r; i++)
                buf[i] = toupper((unsigned char)buf[i]);
            if (write(1, buf, r) != r)
                return 2;
        }
        return r < 0 ? 3 : 0;
    }
    if (!strcmp(m, "count")) {
        long lines = 0, bytes = 0;
        ssize_t r;
        while ((r = read(0, buf, sizeof buf)) > 0) {
            bytes += r;
            for (ssize_t i = 0; i < r; i++)
                lines += buf[i] == '\n';
        }
        printf("count: %ld lines, %ld bytes, pgrp is the job's: %s\n", lines, bytes, getpgrp() != getpgid(getppid()) ? "yes" : "no");
        return 0;
    }
    if (!strcmp(m, "fail")) {
        char c;
        while (read(0, &c, 1) > 0) {
        }
        return 1;
    }
    return 99;
}

/* One job: stages[i] is the stage's argv tail. Returns the last stage's wait status. */
static int job(const char *label, char ***stages, int n, int pipes) {
    pid_t pids[8];
    pid_t pgid = 0;
    int in = -1;
    char tmp[2][96];
    for (int i = 0; i < n; i++) {
        int fds[2] = {-1, -1};
        if (pipes && i + 1 < n && syscall(SYS_pipe2, fds, O_CLOEXEC) != 0)
            return -1;
        if (!pipes && i + 1 < n) {
            snprintf(tmp[i % 2], sizeof tmp[0], "/tmp/abi-pipeline-%d-%d", getpid(), i);
            fds[1] = open(tmp[i % 2], O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
        }
        flush();
        pid_t p = fork();
        if (p == 0) {
            setpgid(0, pgid);
            if (in >= 0)
                dup2(in, 0);
            if (fds[1] >= 0)
                dup2(fds[1], 1);
            execve(self, stages[i], env0);
            _exit(127);
        }
        /* The shell sets the group too, so it holds whichever side runs first. */
        setpgid(p, pgid ? pgid : p);
        if (!pgid)
            pgid = p;
        pids[i] = p;
        if (in >= 0)
            close(in);
        if (fds[1] >= 0)
            close(fds[1]);
        if (pipes) {
            in = fds[0];
        } else {
            /* Sequential: the next stage reads what this one wrote, once it is done. */
            if (i + 1 < n) {
                siginfo_t si;
                waitid(P_PID, p, &si, WEXITED | WNOWAIT);
                in = open(tmp[i % 2], O_RDONLY | O_CLOEXEC);
                unlink(tmp[i % 2]);
            }
        }
    }
    if (in >= 0)
        close(in);
    /* Its stages are alive or zombies until waited for: the group answers. Printed after the waits, so the
     * lines of the stages and of this process cannot change places. */
    long grp = kill(-pgid, 0) == 0 ? 0 : -errno;
    int last = 0;
    for (int i = 0; i < n; i++) {
        int st = 0;
        waitpid(pids[i], &st, 0);
        if (i + 1 == n)
            last = st;
        else if (!WIFEXITED(st) || WEXITSTATUS(st))
            printf("%s: stage %d ended %#x\n", label, i, st);
    }
    printf("%s: kill(-job, 0) = %s\n", label, grp ? ename((int)-grp) : "0");
    return last;
}

static void report(const char *label, int st) {
    if (WIFEXITED(st))
        printf("%s: exit %d\n", label, WEXITSTATUS(st));
    else
        printf("%s: status %#x\n", label, st);
}

int main(int argc, char **argv) {
    if (argc > 1)
        return stage(argc, argv);
    if (readlink("/proc/self/exe", self, sizeof self - 1) < 0)
        return 1;
    int fds[2];
    int pipes = syscall(SYS_pipe2, fds, O_CLOEXEC) == 0;
    if (pipes) {
        close(fds[0]);
        close(fds[1]);
    }
    char *gen[] = {"gen", "gen", "5", NULL};
    char *upper[] = {"upper", "upper", NULL};
    char *count[] = {"count", "count", NULL};
    char *fail[] = {"fail", "fail", NULL};
    char **three[] = {gen, upper, count};
    report("pipeline.gen_upper_count", job("pipeline.gen_upper_count", three, 3, pipes));
    char **failing[] = {gen, fail};
    report("pipeline.ends_with_fail", job("pipeline.ends_with_fail", failing, 2, pipes));
    char **two[] = {gen, count};
    report("pipeline.gen_count", job("pipeline.gen_count", two, 2, pipes));

    /* upper < in > out: redirections onto 0 and 1 of a single stage. */
    char in[96], out[96];
    snprintf(in, sizeof in, "/tmp/abi-pipeline-%d-in", getpid());
    snprintf(out, sizeof out, "/tmp/abi-pipeline-%d-out", getpid());
    int f = open(in, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (write(f, "redirected text\n", 16) != 16)
        puts("short write");
    close(f);
    flush();
    pid_t p = fork();
    if (p == 0) {
        int a = open(in, O_RDONLY);
        int b = open(out, O_WRONLY | O_CREAT | O_TRUNC, 0600);
        dup2(a, 0);
        dup2(b, 1);
        close(a);
        close(b);
        execve(self, upper, env0);
        _exit(127);
    }
    int st = 0;
    waitpid(p, &st, 0);
    report("redirect.upper", st);
    char back[64] = {0};
    f = open(out, O_RDONLY);
    ssize_t r = read(f, back, sizeof back - 1);
    close(f);
    printf("redirect.output: %zd bytes \"%.*s\"\n", r, (int)(r > 0 ? r - 1 : 0), back);
    unlink(in);
    unlink(out);
    puts("pipeline: done");
    return 0;
}
