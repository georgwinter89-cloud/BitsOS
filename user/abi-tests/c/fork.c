/* AB-4: fork, vfork, clone, clone3, execve/execveat (with #! scripts), wait4/waitid, SIGCHLD and the
 * auto-reap rules, process groups and sessions, as CachyOS answers them. argv[1] picks a helper mode for the
 * programs the tests exec (they exec this same file through /proc/self/exe). Only results, errno names and
 * yes/no relations are printed: never pids or paths. Every fork flushes stdout first (a buffered line would be
 * printed twice) and children end with _exit. */
#include "abi.h"
#include <fcntl.h>
#include <limits.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/wait.h>

#ifndef __WCLONE
#define __WCLONE 0x80000000
#endif
#ifndef __WALL
#define __WALL 0x40000000
#endif

extern char **environ;
static char self[PATH_MAX];
static char tmp[64];
static char *env1[] = {"ABI_FORK=yes", NULL};

static void flush(void) {
    fflush(stdout);
    fflush(stderr);
}

static pid_t pfork(void) {
    flush();
    return fork();
}

static void status(const char *label, int st) {
    if (WIFEXITED(st))
        printf("%s: exit %d\n", label, WEXITSTATUS(st));
    else if (WIFSIGNALED(st))
        printf("%s: signal %d\n", label, WTERMSIG(st));
    else
        printf("%s: other %#x\n", label, st);
}

static void reap(const char *label, pid_t pid) {
    int st = 0;
    if (pid < 0) {
        printf("%s: fork -%s\n", label, ename(errno));
        return;
    }
    pid_t r = waitpid(pid, &st, 0);
    if (r != pid)
        printf("%s: waitpid -%s\n", label, ename(errno));
    else
        status(label, st);
}

static void msleep(long ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000};
    nanosleep(&t, 0);
}

/* Waits up to 5 s for a file to appear. */
static int await_file(const char *f) {
    for (int i = 0; i < 5000; i++) {
        if (access(f, F_OK) == 0)
            return 1;
        msleep(1);
    }
    return 0;
}

static void put_file(const char *f, const char *text, mode_t mode) {
    int fd = open(f, O_WRONLY | O_CREAT | O_TRUNC, mode);
    if (fd >= 0) {
        if (write(fd, text, strlen(text)) < 0)
            perror("write");
        close(fd);
    }
    chmod(f, mode);
}

static void path_of(char *out, size_t n, const char *suffix) {
    snprintf(out, n, "%s%s", tmp, suffix);
}

/* ---- helper modes (run by exec) --------------------------------------------------------------------- */

static int helper(int argc, char **argv) {
    const char *m = argv[1];
    if (!strcmp(m, "exec-child")) {
        const char *e = getenv("ABI_FORK");
        printf("exec-child %s: argc=%d env=%s\n", argc > 2 ? argv[2] : "?", argc, e ? e : "none");
        flush();
        return 5;
    }
    if (!strcmp(m, "pidcheck")) {
        yes("pidcheck.same_pid", getpid() == atoi(argv[2]));
        yes("pidcheck.tid_is_pid", syscall(SYS_gettid) == getpid());
        flush();
        return 0;
    }
    if (!strcmp(m, "after-exec")) {
        show("after_exec.cloexec_fd", fcntl(atoi(argv[2]), F_GETFD));
        show("after_exec.kept_fd", fcntl(atoi(argv[3]), F_GETFD));
        struct sigaction a;
        sigaction(SIGUSR1, 0, &a);
        yes("after_exec.caught_signal_now_default", a.sa_handler == SIG_DFL);
        sigaction(SIGUSR2, 0, &a);
        yes("after_exec.ignored_signal_still_ignored", a.sa_handler == SIG_IGN);
        sigset_t cur;
        sigprocmask(SIG_BLOCK, 0, &cur);
        yes("after_exec.blocked_mask_kept", sigismember(&cur, SIGTERM));
        stack_t ss;
        sigaltstack(0, &ss);
        yes("after_exec.altstack_gone", (ss.ss_flags & SS_DISABLE) != 0);
        flush();
        return 0;
    }
    if (!strncmp(m, "script-mode", 11)) {
        char exe[PATH_MAX] = {0};
        if (readlink("/proc/self/exe", exe, sizeof exe - 1) < 0)
            exe[0] = 0;
        printf("script: argc=%d arg1=\"%s\"\n", argc, m);
        yes("script.argv0_is_interpreter", !strcmp(argv[0], exe));
        for (int i = 2; i < argc; i++) {
            size_t l = strlen(argv[i]);
            if (l > 3 && !strcmp(argv[i] + l - 3, ".sh"))
                printf("script.argv%d: a script path ending in %s\n", i, argv[i] + l - 5);
            else
                printf("script.argv%d: \"%s\"\n", i, argv[i]);
        }
        flush();
        return 6;
    }
    if (!strcmp(m, "hold")) {
        char b;
        return read(0, &b, 1) == 1 ? 0 : 1;
    }
    if (!strcmp(m, "mark")) {
        put_file(argv[2], "x", 0644);
        msleep(atoi(argv[3]));
        return 0;
    }
    printf("unknown mode %s\n", m);
    return 99;
}

/* ---- the tests --------------------------------------------------------------------------------------- */

static void t_fork_basics(void) {
    pid_t parent = getpid();
    pid_t p = pfork();
    if (p == 0)
        _exit(getppid() == parent ? 7 : 8);
    yes("fork.returns_child_pid", p > 0 && p != parent);
    reap("fork.child_sees_parent", p);
    if ((p = pfork()) == 0) {
        kill(getpid(), SIGKILL);
        _exit(1);
    }
    reap("fork.killed", p);
    if ((p = pfork()) == 0) {
        *(volatile int *)0 = 1;
        _exit(1);
    }
    reap("fork.segv", p);
    if ((p = pfork()) == 0)
        _exit(300);
    reap("fork.exit_low_byte", p);
}

static void t_wait_rules(void) {
    int st = 0;
    SYS("wait4.no_children", SYS_wait4, -1, 0, 0, 0);
    SYS("wait4.bad_option", SYS_wait4, -1, 0, 0x100, 0);
    SYS("wait4.waitid_only_option", SYS_wait4, -1, 0, WEXITED, 0);
    SYS("wait4.int_min", SYS_wait4, INT_MIN, 0, 0, 0);
    SYS("wait4.not_my_child", SYS_wait4, getppid(), 0, 0, 0);
    pid_t p = pfork();
    if (p == 0) {
        msleep(100);
        _exit(3);
    }
    SYS("wait4.wnohang_running", SYS_wait4, p, &st, WNOHANG, 0);
    struct rusage ru;
    yes("wait4.collects_the_child", syscall(SYS_wait4, p, &st, 0, &ru) == p);
    status("wait4.status", st);
    if ((p = pfork()) == 0)
        _exit(1);
    SYS("wait4.bad_status_pointer", SYS_wait4, p, KERNEL_ADDR, 0, 0);
    SYS("wait4.collected_despite_efault", SYS_wait4, p, 0, 0, 0);
    if ((p = pfork()) == 0)
        _exit(2);
    yes("wait4.own_group", syscall(SYS_wait4, 0, &st, 0, 0) == p);
    status("wait4.own_group_status", st);
    if ((p = pfork()) == 0)
        _exit(4);
    yes("wait4.named_group", syscall(SYS_wait4, -getpgrp(), &st, 0, 0) == p);
    status("wait4.named_group_status", st);
}

static void t_waitid(void) {
    siginfo_t si;
    SYS("waitid.no_kind", SYS_waitid, P_ALL, 0, &si, 0, 0);
    SYS("waitid.bad_idtype", SYS_waitid, 7, 0, &si, WEXITED, 0);
    SYS("waitid.pid_zero", SYS_waitid, P_PID, 0, &si, WEXITED, 0);
    SYS("waitid.pidfd_on_a_pipe", SYS_waitid, 3, 0, &si, WEXITED, 0);
    SYS("waitid.no_children", SYS_waitid, P_ALL, 0, &si, WEXITED | WNOHANG, 0);
    pid_t p = pfork();
    if (p == 0) {
        msleep(100);
        _exit(9);
    }
    memset(&si, 0xff, sizeof si);
    SYS("waitid.wnohang_running", SYS_waitid, P_PID, p, &si, WEXITED | WNOHANG, 0);
    yes("waitid.wnohang_zeroes_pid_and_signo", si.si_pid == 0 && si.si_signo == 0);
    SYS("waitid.stopped_only", SYS_waitid, P_PID, p, &si, WSTOPPED | WNOHANG, 0);
    SYS("waitid.nowait", SYS_waitid, P_PID, p, &si, WEXITED | WNOWAIT, 0);
    printf("waitid.si_code=%d si_status=%d\n", si.si_code, si.si_status);
    yes("waitid.si_signo_is_sigchld", si.si_signo == SIGCHLD);
    yes("waitid.si_pid_is_child", si.si_pid == p);
    yes("waitid.si_uid_is_mine", si.si_uid == getuid());
    SYS("waitid.again_collects", SYS_waitid, P_PID, p, &si, WEXITED, 0);
    SYS("waitid.gone", SYS_waitid, P_PID, p, &si, WEXITED | WNOHANG, 0);
    if ((p = pfork()) == 0) {
        kill(getpid(), SIGKILL);
        _exit(1);
    }
    SYS("waitid.killed", SYS_waitid, P_PGID, 0, &si, WEXITED, 0);
    printf("waitid.killed si_code=%d si_status=%d\n", si.si_code, si.si_status);
}

static int g_value = 1;

static void t_memory(void) {
    char f[96];
    path_of(f, sizeof f, ".map");
    char page[4096];
    memset(page, 'a', sizeof page);
    int fd = open(f, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (write(fd, page, sizeof page) != sizeof page)
        puts("memory: short write");
    char *sh = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    char *pr = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    int *anon = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    int *none = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *heap = malloc(64);
    strcpy(heap, "parent");
    *anon = 11;
    *none = 42;
    mprotect(none, 4096, PROT_NONE);
    pr[0] = 'p';
    int local = 5;
    pid_t p = pfork();
    if (p == 0) {
        yes("memory.child_sees_parent_heap", !strcmp(heap, "parent"));
        yes("memory.child_sees_parent_private_write", pr[0] == 'p');
        yes("memory.child_sees_anonymous", *anon == 11);
        mprotect(none, 4096, PROT_READ);
        yes("memory.child_keeps_prot_none_content", *none == 42);
        g_value = 2;
        local = 6;
        strcpy(heap, "child");
        *anon = 22;
        sh[0] = 's';
        pr[1] = 'q';
        char *big = malloc(1 << 20);
        memset(big, 1, 1 << 20);
        long cur = syscall(SYS_brk, 0);
        yes("memory.child_heap_grows", big != 0 && syscall(SYS_brk, cur + 4096) == cur + 4096);
        flush();
        _exit(local);
    }
    reap("memory.child", p);
    yes("memory.global_unchanged", g_value == 1);
    yes("memory.local_unchanged", local == 5);
    yes("memory.heap_unchanged", !strcmp(heap, "parent"));
    yes("memory.anonymous_unchanged", *anon == 11);
    yes("memory.shared_file_sees_child_write", sh[0] == 's');
    yes("memory.private_file_unchanged", pr[1] == 'a' && pr[0] == 'p');
    char back[2] = {0};
    yes("memory.file_has_shared_write", pread(fd, back, 1, 0) == 1 && back[0] == 's');
    close(fd);
    unlink(f);
}

static void t_exec(void) {
    pid_t p;
    if ((p = pfork()) == 0) {
        char *av[] = {"x", "exec-child", "plain", NULL};
        execve(self, av, env1);
        _exit(127);
    }
    reap("exec.plain", p);
    if ((p = pfork()) == 0) {
        char pid[16];
        snprintf(pid, sizeof pid, "%d", getpid());
        char *av[] = {"x", "pidcheck", pid, NULL};
        execve(self, av, env1);
        _exit(127);
    }
    reap("exec.pidcheck", p);
    if ((p = pfork()) == 0) {
        execve(self, NULL, env1);
        _exit(127);
    }
    reap("exec.null_argv", p);
    char s1[96], s2[96], s3[96], line[PATH_MAX + 64];
    path_of(s1, sizeof s1, "-1.sh");
    path_of(s2, sizeof s2, "-2.sh");
    path_of(s3, sizeof s3, "-3.sh");
    snprintf(line, sizeof line, "#!%s script-mode\n", self);
    put_file(s1, line, 0755);
    snprintf(line, sizeof line, "#! %s  script-mode two  words \t\nignored\n", self);
    put_file(s2, line, 0755);
    snprintf(line, sizeof line, "#!%s\n", s1);
    put_file(s3, line, 0755);
    char *av[] = {"s", "x", NULL};
    if ((p = pfork()) == 0) {
        execve(s1, av, env1);
        _exit(127);
    }
    reap("exec.script", p);
    if ((p = pfork()) == 0) {
        execve(s2, av, env1);
        _exit(127);
    }
    reap("exec.script_with_argument", p);
    if ((p = pfork()) == 0) {
        execve(s3, av, env1);
        _exit(127);
    }
    reap("exec.script_of_a_script", p);
    unlink(s1);
    unlink(s2);
    unlink(s3);
}

static void t_exec_errors(void) {
    char *av[] = {"x", NULL};
    char *ev[] = {NULL};
    char f[96], g[96], h[96], line[PATH_MAX + 64];
    path_of(f, sizeof f, ".noexec");
    put_file(f, "\x7f" "ELF not really", 0644);
    path_of(g, sizeof g, ".garbage");
    put_file(g, "garbage\n", 0755);
    show("execve.missing", execve("/tmp/abi-fork-does-not-exist", av, ev));
    show("execve.directory", execve("/tmp", av, ev));
    show("execve.no_x_bit", execve(f, av, ev));
    show("execve.not_a_program", execve(g, av, ev));
    snprintf(line, sizeof line, "%s/x", self);
    show("execve.through_a_file", execve(line, av, ev));
    show("execve.empty_path", execve("", av, ev));
    show("execve.bad_path_pointer", execve(KERNEL_ADDR, av, ev));
    show("execve.bad_argv_pointer", execve(self, KERNEL_ADDR, ev));
    char *bad_entry[] = {"x", KERNEL_ADDR, NULL};
    show("execve.bad_argv_entry", execve(self, bad_entry, ev));
    char *big = malloc(131073);
    memset(big, 'a', 131072);
    big[131072] = 0;
    char *av_big[] = {"x", big, NULL};
    show("execve.string_too_long", execve(self, av_big, ev));
    big[131071] = 0;
    char *long_path = malloc(5000);
    memset(long_path, 'a', 4999);
    long_path[4999] = 0;
    show("execve.path_too_long", execve(long_path, av, ev));
    /* Script errors: a missing interpreter, none at all, one without x, a loop. */
    path_of(h, sizeof h, "-e.sh");
    put_file(h, "#!/tmp/abi-fork-no-interpreter\n", 0755);
    show("execve.script_missing_interpreter", execve(h, av, ev));
    put_file(h, "#!   \n", 0755);
    show("execve.script_without_interpreter", execve(h, av, ev));
    snprintf(line, sizeof line, "#!%s\n", f);
    put_file(h, line, 0755);
    show("execve.script_interpreter_no_x", execve(h, av, ev));
    snprintf(line, sizeof line, "#!%s\n", h);
    put_file(h, line, 0755);
    show("execve.script_loop", execve(h, av, ev));
    unlink(f);
    unlink(g);
    unlink(h);
    free(big);
    free(long_path);
}

static void t_execveat(void) {
    char *ev[] = {NULL};
    char *av[] = {"x", NULL};
    int fd = open(self, O_RDONLY);
    int root = open("/", O_RDONLY | O_DIRECTORY);
    pid_t p;
    if ((p = pfork()) == 0) {
        char *a[] = {"x", "exec-child", "at-empty-path", NULL};
        syscall(SYS_execveat, fd, "", a, env1, AT_EMPTY_PATH);
        _exit(127);
    }
    reap("execveat.empty_path_fd", p);
    if ((p = pfork()) == 0) {
        char *a[] = {"x", "exec-child", "dirfd-relative", NULL};
        syscall(SYS_execveat, root, "proc/self/exe", a, env1, 0);
        _exit(127);
    }
    reap("execveat.dirfd_relative", p);
    SYS("execveat.bad_flags", SYS_execveat, AT_FDCWD, self, av, ev, 1);
    SYS("execveat.file_as_dirfd", SYS_execveat, fd, "x", av, ev, 0);
    SYS("execveat.bad_dirfd", SYS_execveat, 999, "x", av, ev, 0);
    SYS("execveat.nofollow_symlink", SYS_execveat, AT_FDCWD, "/proc/self/exe", av, ev, AT_SYMLINK_NOFOLLOW);
    SYS("execveat.empty_without_flag", SYS_execveat, fd, "", av, ev, 0);
    close(fd);
    close(root);
    /* A #! script reached through a close-on-exec fd: its name /dev/fd/N would be gone after the exec, so
     * Linux refuses it with ENOENT (BINPRM_FLAGS_PATH_INACCESSIBLE); without O_CLOEXEC it runs (fexecve). */
    char s[96], line[PATH_MAX + 64];
    path_of(s, sizeof s, "-fd.sh");
    snprintf(line, sizeof line, "#!%s script-mode\n", self);
    put_file(s, line, 0755);
    int sfd = open(s, O_RDONLY | O_CLOEXEC);
    SYS("execveat.script_cloexec_fd", SYS_execveat, sfd, "", av, ev, AT_EMPTY_PATH);
    char *slash = strrchr(s, '/');
    *slash = 0;
    int dfd = open(s, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    SYS("execveat.script_cloexec_dirfd", SYS_execveat, dfd, slash + 1, av, ev, 0);
    *slash = '/';
    close(dfd);
    close(sfd);
    sfd = open(s, O_RDONLY);
    flush();
    if ((p = pfork()) == 0) {
        /* The script's output names /dev/fd/N: kept out of the transcript, the status says it ran. */
        int nul = open("/dev/null", O_WRONLY);
        dup2(nul, 1);
        syscall(SYS_execveat, sfd, "", av, ev, AT_EMPTY_PATH);
        _exit(127);
    }
    reap("execveat.script_fd_runs", p);
    close(sfd);
    unlink(s);
}

static void handler(int s) { (void)s; }

static void t_exec_resets(void) {
    pid_t p = pfork();
    if (p == 0) {
        int a = open("/dev/null", O_RDONLY | O_CLOEXEC);
        int b = open("/dev/null", O_RDONLY);
        struct sigaction sa = {0};
        sa.sa_handler = handler;
        sigaction(SIGUSR1, &sa, 0);
        signal(SIGUSR2, SIG_IGN);
        sigset_t s;
        sigemptyset(&s);
        sigaddset(&s, SIGTERM);
        sigprocmask(SIG_BLOCK, &s, 0);
        stack_t ss = {.ss_sp = malloc(65536), .ss_size = 65536, .ss_flags = 0};
        sigaltstack(&ss, 0);
        char as[16], bs[16];
        snprintf(as, sizeof as, "%d", a);
        snprintf(bs, sizeof bs, "%d", b);
        char *av[] = {"after", "after-exec", as, bs, NULL};
        execve(self, av, env1);
        _exit(127);
    }
    reap("exec.resets", p);
}

static void t_vfork(void) {
    volatile int shared = 0;
    flush();
    pid_t p = vfork();
    if (p == 0) {
        shared = 42;
        _exit(3);
    }
    yes("vfork.child_wrote_parent_memory", shared == 42);
    reap("vfork.child", p);
    flush();
    if ((p = vfork()) == 0) {
        char *av[] = {"vf", "exec-child", "vfork", NULL};
        execve(self, av, env1);
        _exit(127);
    }
    reap("vfork.exec", p);
    volatile int err = 0;
    flush();
    if ((p = vfork()) == 0) {
        char *av[] = {"x", NULL};
        execve("/tmp/abi-fork-does-not-exist", av, env1);
        err = errno;
        _exit(21);
    }
    reap("vfork.exec_fails", p);
    printf("vfork.exec_error = %s\n", ename(err));
}

/* posix_spawn needs pipe2 (musl reports exec errors through a pipe). A kernel without pipe2 (V2 before FS-3)
 * gets the same spawn done with vfork + execve, so the lines stay the same. */
static int spawn(pid_t *pid, const char *path, char **av) {
    int r = posix_spawn(pid, path, 0, 0, av, env1);
    if (r != ENOSYS)
        return r;
    volatile int err = 0;
    flush();
    pid_t p = vfork();
    if (p == 0) {
        execve(path, av, env1);
        err = errno;
        _exit(127);
    }
    if (err) {
        waitpid(p, 0, 0);
        return err;
    }
    *pid = p;
    return 0;
}

static void t_spawn(void) {
    pid_t p = 0;
    char *av[] = {"sp", "exec-child", "spawn", NULL};
    flush();
    int r = spawn(&p, self, av);
    printf("posix_spawn = %s\n", r ? ename(r) : "0");
    if (!r)
        reap("posix_spawn.child", p);
    r = spawn(&p, "/tmp/abi-fork-does-not-exist", av);
    printf("posix_spawn.missing = %s\n", r ? ename(r) : "0");
}

static void jobs_child(void) {
    pid_t me = getpid();
    yes("jobs.child_in_parent_session", getsid(0) == getsid(getppid()));
    yes("jobs.child_in_parent_group", getpgrp() == getpgid(getppid()));
    yes("jobs.setsid_returns_pid", setsid() == me);
    yes("jobs.sid_is_pid", getsid(0) == me);
    yes("jobs.pgrp_is_pid", getpgrp() == me);
    show("jobs.setsid_again", setsid());
    show("jobs.setpgid_as_session_leader", setpgid(0, 0));
    show("jobs.setpgid_negative", setpgid(0, -1));
    show("jobs.getpgid_none", getpgid(0x7ffffff0));
    show("jobs.getsid_none", getsid(0x7ffffff0));
    show("jobs.setpgid_none", setpgid(0x7ffffff0, 0));
    show("jobs.setpgid_parent", setpgid(getppid(), getppid()));
    show("jobs.kill_own_group", kill(-me, 0));
    show("jobs.kill_no_group", kill(-0x7ffffff0, 0));
    pid_t k = pfork();
    if (k == 0) {
        yes("jobs.k_new_group", setpgid(0, 0) == 0 && getpgrp() == getpid());
        show("jobs.k_join_parent_group", setpgid(0, getppid()));
        yes("jobs.k_in_parent_group", getpgrp() == getppid());
        show("jobs.k_no_such_group", setpgid(0, 0x7ffffff0));
        flush();
        _exit(0);
    }
    reap("jobs.k", k);
    char go[96];
    path_of(go, sizeof go, ".go");
    unlink(go);
    k = pfork();
    if (k == 0) {
        await_file(go);
        _exit(0);
    }
    show("jobs.setpgid_child", setpgid(k, k));
    yes("jobs.child_group_set", getpgid(k) == k);
    put_file(go, "x", 0644);
    reap("jobs.k2", k);
    char mark[96];
    path_of(mark, sizeof mark, ".mark");
    unlink(mark);
    if ((k = pfork()) == 0) {
        char *av[] = {"m", "mark", mark, "300", NULL};
        execve(self, av, env1);
        _exit(127);
    }
    yes("jobs.child_execed", await_file(mark));
    show("jobs.setpgid_after_exec", setpgid(k, k));
    reap("jobs.k3", k);
    unlink(go);
    unlink(mark);
}

static void t_jobs(void) {
    pid_t j = pfork();
    if (j == 0) {
        jobs_child();
        flush();
        _exit(0);
    }
    reap("jobs", j);
}

static void t_sigchld(void) {
    sigset_t s, old, pend;
    sigemptyset(&s);
    sigaddset(&s, SIGCHLD);
    sigprocmask(SIG_BLOCK, &s, &old);
    pid_t p = pfork();
    if (p == 0)
        _exit(0);
    reap("sigchld.child", p);
    sigpending(&pend);
    yes("sigchld.pending_after_child_end", sigismember(&pend, SIGCHLD));
    signal(SIGCHLD, SIG_IGN);
    sigpending(&pend);
    yes("sigchld.ignoring_discards_it", !sigismember(&pend, SIGCHLD));
    if ((p = pfork()) == 0) {
        msleep(20);
        _exit(0);
    }
    SYS("sigchld.ignored_child_is_reaped", SYS_wait4, -1, 0, 0, 0);
    signal(SIGCHLD, SIG_DFL);
    struct sigaction sa = {0};
    sa.sa_handler = SIG_DFL;
    sa.sa_flags = SA_NOCLDWAIT;
    sigaction(SIGCHLD, &sa, 0);
    if ((p = pfork()) == 0) {
        msleep(20);
        _exit(0);
    }
    SYS("sigchld.nocldwait_child_is_reaped", SYS_wait4, -1, 0, 0, 0);
    signal(SIGCHLD, SIG_DFL);
    sigprocmask(SIG_SETMASK, &old, 0);
}

static int vm_value;

static int vm_child(void *arg) {
    vm_value = *(int *)arg;
    return 7;
}

static void t_clone(void) {
    int st = 0;
    SYS("clone.sighand_without_vm", SYS_clone, CLONE_SIGHAND | SIGCHLD, 0, 0, 0, 0);
    SYS("clone.thread_without_sighand", SYS_clone, CLONE_THREAD | CLONE_VM | SIGCHLD, 0, 0, 0, 0);
    SYS("clone.settls_kernel_address", SYS_clone, CLONE_SETTLS | SIGCHLD, 0, 0, 0, KERNEL_ADDR);
    int ptid = 0, ctid = 0;
    flush();
    long r = syscall(SYS_clone, CLONE_PARENT_SETTID | CLONE_CHILD_SETTID | SIGCHLD, 0, &ptid, &ctid, 0);
    if (r == 0)
        _exit(ctid == syscall(SYS_gettid) ? 0 : 1);
    yes("clone.parent_settid", ptid == r);
    reap("clone.child_settid", r);
    sigset_t s, pend;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    sigprocmask(SIG_BLOCK, &s, 0);
    flush();
    r = syscall(SYS_clone, SIGUSR1, 0, 0, 0, 0);
    if (r == 0) {
        msleep(20);
        _exit(4);
    }
    SYS("clone.wait_without_wclone", SYS_wait4, r, &st, 0, 0);
    yes("clone.wait_with_wclone", syscall(SYS_wait4, r, &st, __WCLONE, 0) == r);
    status("clone.wclone_status", st);
    sigpending(&pend);
    yes("clone.exit_signal_sent", sigismember(&pend, SIGUSR1));
    signal(SIGUSR1, SIG_IGN);
    signal(SIGUSR1, SIG_DFL);
    sigprocmask(SIG_UNBLOCK, &s, 0);
    flush();
    r = syscall(SYS_clone, 0, 0, 0, 0, 0);
    if (r == 0)
        _exit(5);
    yes("clone.no_exit_signal_wall", syscall(SYS_wait4, r, &st, __WALL, 0) == r);
    status("clone.wall_status", st);
    int arg = 33;
    char *stack = malloc(1 << 16);
    flush();
    r = clone(vm_child, stack + (1 << 16), CLONE_VM | SIGCHLD, &arg);
    reap("clone.vm", r);
    yes("clone.vm_child_wrote_shared_memory", vm_value == 33);
    free(stack);
}

struct clone_args_v2 {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup;
};

static void t_clone3(void) {
    struct clone_args_v2 a;
    unsigned char big[128];
    memset(&a, 0, sizeof a);
    SYS("clone3.too_small", SYS_clone3, &a, 63);
    SYS("clone3.too_big", SYS_clone3, &a, 4097);
    memset(big, 0, sizeof big);
    big[100] = 1;
    SYS("clone3.nonzero_tail", SYS_clone3, big, 104);
    a.exit_signal = 65;
    SYS("clone3.bad_exit_signal", SYS_clone3, &a, sizeof a);
    a.exit_signal = SIGCHLD;
    a.flags = 0x400000; /* CLONE_DETACHED */
    SYS("clone3.detached", SYS_clone3, &a, sizeof a);
    a.flags = 0;
    a.stack = (uint64_t)big;
    SYS("clone3.stack_without_size", SYS_clone3, &a, sizeof a);
    a.stack = 0;
    a.flags = CLONE_THREAD | CLONE_SIGHAND | CLONE_VM;
    SYS("clone3.thread_with_exit_signal", SYS_clone3, &a, sizeof a);
    a.flags = 0;
    pid_t want = 300;
    a.set_tid = (uint64_t)&want;
    a.set_tid_size = 1;
    SYS("clone3.set_tid_unprivileged", SYS_clone3, &a, sizeof a);
    a.set_tid = 0;
    a.set_tid_size = 0;
    flush();
    long r = syscall(SYS_clone3, &a, sizeof a);
    if (r == 0)
        _exit(9);
    reap("clone3.fork", r);
    flush();
    r = syscall(SYS_clone3, &a, 64);
    if (r == 0)
        _exit(8);
    reap("clone3.fork_ver0_size", r);
}

static void t_orphan(void) {
    char done[96];
    path_of(done, sizeof done, ".orphan");
    unlink(done);
    pid_t c = pfork();
    if (c == 0) {
        pid_t me = getpid();
        pid_t g = pfork();
        if (g == 0) {
            int moved = 0;
            for (int i = 0; i < 5000 && !moved; i++) {
                moved = getppid() != me;
                if (!moved)
                    msleep(1);
            }
            yes("orphan.reparented", moved);
            flush();
            put_file(done, "x", 0644);
            _exit(0);
        }
        _exit(0);
    }
    /* The grandchild prints once its parent is gone: wait for its mark before printing anything here. */
    int st = 0;
    waitpid(c, &st, 0);
    yes("orphan.grandchild_finished", await_file(done));
    status("orphan.parent", st);
    unlink(done);
}

static void t_nproc(void) {
    pid_t p = pfork();
    if (p == 0) {
        struct rlimit r = {1, 1};
        setrlimit(RLIMIT_NPROC, &r);
        flush();
        pid_t q = fork();
        if (q == 0)
            _exit(0);
        if (q > 0)
            waitpid(q, 0, 0);
        printf("nproc.fork_over_limit = %s\n", q < 0 ? ename(errno) : "a child");
        flush();
        _exit(0);
    }
    reap("nproc", p);
}

/* ETXTBSY (deny_write_access): a file open for writing cannot be executed; a running program's file cannot be
 * opened for writing or truncated until the program ends. */
static void t_etxtbsy(void) {
    char c[96], buf[65536];
    path_of(c, sizeof c, ".txt");
    int in = open(self, O_RDONLY), out = open(c, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    long n;
    while (in >= 0 && out >= 0 && (n = read(in, buf, sizeof buf)) > 0)
        if (write(out, buf, n) != n)
            break;
    close(in);
    char *hold[] = {"x", "hold", NULL};
    char *ev[] = {NULL};
    pid_t k = fork();
    if (k == 0) {
        execve(c, hold, ev);
        _exit(errno == ETXTBSY ? 26 : 1);
    }
    int st = 0;
    waitpid(k, &st, 0);
    printf("etxtbsy.exec_while_open_for_writing: %s\n", WIFEXITED(st) && WEXITSTATUS(st) == 26 ? "ETXTBSY" : "other");
    close(out);
    int hp[2], ep[2];
    if (pipe(hp) != 0 || pipe2(ep, O_CLOEXEC) != 0)
        return;
    k = fork();
    if (k == 0) {
        dup2(hp[0], 0);
        close(hp[1]);
        close(ep[0]);
        execve(c, hold, ev);
        _exit(1);
    }
    close(hp[0]);
    close(ep[1]);
    char b;
    yes("etxtbsy.copy_runs", read(ep[0], &b, 1) == 0);
    close(ep[0]);
    show("etxtbsy.open_running_for_writing", open(c, O_WRONLY));
    show("etxtbsy.truncate_running", truncate(c, 0));
    int r = open(c, O_RDONLY);
    yes("etxtbsy.open_running_for_reading", r >= 0);
    close(r);
    write(hp[1], "x", 1);
    close(hp[1]);
    waitpid(k, &st, 0);
    yes("etxtbsy.copy_ended", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    r = open(c, O_WRONLY);
    yes("etxtbsy.open_for_writing_after_the_end", r >= 0);
    close(r);
    unlink(c);
}

int main(int argc, char **argv) {
    if (argc == 1 && argv[0] && !argv[0][0]) {
        puts("null-argv: argc=1, argv[0] is empty");
        flush();
        return 12;
    }
    if (argc > 1)
        return helper(argc, argv);
    if (readlink("/proc/self/exe", self, sizeof self - 1) < 0) {
        puts("no /proc/self/exe");
        return 1;
    }
    snprintf(tmp, sizeof tmp, "/tmp/abi-fork-%d", getpid());
    t_fork_basics();
    t_wait_rules();
    t_waitid();
    t_memory();
    t_exec();
    t_exec_errors();
    t_etxtbsy();
    t_execveat();
    t_exec_resets();
    t_vfork();
    t_spawn();
    t_jobs();
    t_sigchld();
    t_clone();
    t_clone3();
    t_orphan();
    t_nproc();
    puts("fork: done");
    return 0;
}
