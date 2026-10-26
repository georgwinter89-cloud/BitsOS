/* AB-4: threads through musl's pthreads (clone with CLONE_VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|SETTLS|
 * PARENT_SETTID|CHILD_CLEARTID): counters under a mutex, a condvar ping-pong, return values, thread ids,
 * thread-local storage, detached threads, and how a thread group ends (exit from a thread ends all, the main
 * thread may leave first, execve from a thread, fork from a thread, a kill). Only the main thread prints,
 * after joins; the end cases run in forked children so their status is compared. */
#include "abi.h"
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <sys/wait.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static long counter;
static int turn;
static __thread int tls = 1;
static char self[4096];

static void flush(void) {
    fflush(stdout);
    fflush(stderr);
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
    if (pid < 0 || waitpid(pid, &st, 0) != pid) {
        printf("%s: -%s\n", label, ename(errno));
        return;
    }
    status(label, st);
}

static void msleep(long ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000};
    nanosleep(&t, 0);
}

static void *add(void *arg) {
    long n = (long)arg;
    for (long i = 0; i < n; i++) {
        pthread_mutex_lock(&lock);
        counter++;
        pthread_mutex_unlock(&lock);
    }
    tls = 7;
    return (void *)(long)tls;
}

static void *pong(void *arg) {
    long rounds = (long)arg;
    for (long i = 0; i < rounds; i++) {
        pthread_mutex_lock(&lock);
        while (turn != 1)
            pthread_cond_wait(&cond, &lock);
        turn = 0;
        pthread_cond_signal(&cond);
        pthread_mutex_unlock(&lock);
    }
    return 0;
}

struct ids {
    long tid;
    pid_t pid;
};

static void *who(void *arg) {
    struct ids *i = arg;
    i->tid = syscall(SYS_gettid);
    i->pid = getpid();
    return (void *)42;
}

static sem_t sem;

static void *detached(void *arg) {
    (void)arg;
    sem_post(&sem);
    return 0;
}

static void basics(void) {
    pthread_t t[4];
    for (int i = 0; i < 4; i++)
        pthread_create(&t[i], 0, add, (void *)10000L);
    long sum = 0;
    for (int i = 0; i < 4; i++) {
        void *r;
        pthread_join(t[i], &r);
        sum += (long)r;
    }
    printf("threads.counter = %ld\n", counter);
    printf("threads.tls_results = %ld\n", sum);
    yes("threads.main_tls_untouched", tls == 1);

    pthread_t p;
    pthread_create(&p, 0, pong, (void *)200L);
    for (int i = 0; i < 200; i++) {
        pthread_mutex_lock(&lock);
        turn = 1;
        pthread_cond_signal(&cond);
        while (turn != 0)
            pthread_cond_wait(&cond, &lock);
        pthread_mutex_unlock(&lock);
    }
    pthread_join(p, 0);
    puts("threads.condvar_pingpong: 200 rounds");

    struct ids i = {0, 0};
    void *r = 0;
    pthread_create(&p, 0, who, &i);
    pthread_join(p, &r);
    printf("threads.return_value = %ld\n", (long)r);
    yes("threads.own_tid", i.tid != getpid() && i.tid > 0);
    yes("threads.same_pid", i.pid == getpid());
    yes("threads.main_tid_is_pid", syscall(SYS_gettid) == getpid());

    sem_init(&sem, 0, 0);
    pthread_attr_t a;
    pthread_attr_init(&a);
    pthread_attr_setdetachstate(&a, PTHREAD_CREATE_DETACHED);
    for (int k = 0; k < 8; k++)
        pthread_create(&p, &a, detached, 0);
    for (int k = 0; k < 8; k++)
        sem_wait(&sem);
    puts("threads.detached: 8 posted");

    pthread_t many[32];
    counter = 0;
    for (int k = 0; k < 32; k++)
        pthread_create(&many[k], 0, add, (void *)100L);
    for (int k = 0; k < 32; k++)
        pthread_join(many[k], 0);
    printf("threads.thirty_two = %ld\n", counter);
}

/* sigaltstack is per thread: a new thread starts without one (sas_ss_reset), and what it installs is not the
 * main thread's. The thread reports what it saw at its start and installs its own stack. */
static char alt_main[65536], alt_thread[65536];
static void *alt_worker(void *arg) {
    stack_t st;
    sigaltstack(0, &st);
    ((int *)arg)[0] = (st.ss_flags & SS_DISABLE) != 0;
    stack_t mine = {.ss_sp = alt_thread, .ss_size = sizeof alt_thread, .ss_flags = 0};
    ((int *)arg)[1] = sigaltstack(&mine, 0) == 0;
    sigaltstack(0, &st);
    ((int *)arg)[2] = st.ss_sp == alt_thread;
    return 0;
}

static void alt_stacks(void) {
    stack_t mine = {.ss_sp = alt_main, .ss_size = sizeof alt_main, .ss_flags = 0}, old, now;
    sigaltstack(&mine, &old);
    int seen[3] = {0, 0, 0};
    pthread_t t;
    pthread_create(&t, 0, alt_worker, seen);
    pthread_join(t, 0);
    sigaltstack(0, &now);
    yes("altstack.new_thread_starts_without", seen[0]);
    yes("altstack.thread_installs_its_own", seen[1] && seen[2]);
    yes("altstack.main_keeps_its_own", now.ss_sp == alt_main && now.ss_size == sizeof alt_main);
    sigaltstack(&old, 0);
}

static void *exit3(void *arg) {
    (void)arg;
    msleep(20);
    exit(3);
}

static void *last_one(void *arg) {
    (void)arg;
    msleep(50);
    puts("ends.main_left_first: the other thread still runs");
    flush();
    return 0;
}

static void *spin(void *arg) {
    (void)arg;
    for (;;)
        __asm__ volatile("" ::: "memory");
    return 0;
}

static void *exec_from_thread(void *arg) {
    char pid[16];
    snprintf(pid, sizeof pid, "%d", getpid());
    char *av[] = {"x", "pidcheck", pid, NULL};
    char *ev[] = {NULL};
    (void)arg;
    execve(self, av, ev);
    return 0;
}

static void *forker(void *arg) {
    (void)arg;
    flush();
    pid_t c = fork();
    if (c == 0) {
        pthread_t t;
        struct ids i;
        int ok = pthread_create(&t, 0, who, &i) == 0 && pthread_join(t, 0) == 0;
        _exit(ok ? 0 : 1);
    }
    int st = 0;
    waitpid(c, &st, 0);
    return (void *)(long)st;
}

static void ends(void) {
    pid_t c;
    flush();
    if ((c = fork()) == 0) {
        pthread_t t;
        pthread_create(&t, 0, exit3, 0);
        pthread_join(t, 0);
        _exit(9);
    }
    reap("ends.exit_from_a_thread", c);
    flush();
    if ((c = fork()) == 0) {
        pthread_t t;
        pthread_create(&t, 0, last_one, 0);
        pthread_exit(0);
    }
    reap("ends.main_thread_exits_first", c);
    flush();
    if ((c = fork()) == 0) {
        pthread_t t;
        for (int i = 0; i < 3; i++)
            pthread_create(&t, 0, spin, 0);
        msleep(20);
        kill(getpid(), SIGKILL);
        _exit(1);
    }
    reap("ends.sigkill_with_spinning_threads", c);
    flush();
    if ((c = fork()) == 0) {
        pthread_t t;
        pthread_create(&t, 0, spin, 0);
        pthread_create(&t, 0, exec_from_thread, 0);
        pthread_join(t, 0);
        _exit(1);
    }
    reap("ends.execve_from_a_thread", c);
    pthread_t t;
    void *st;
    pthread_create(&t, 0, forker, 0);
    pthread_join(t, &st);
    status("ends.fork_from_a_thread", (int)(long)st);
}

int main(int argc, char **argv) {
    if (argc > 2 && !strcmp(argv[1], "pidcheck")) {
        yes("pidcheck.same_pid_after_exec_from_thread", getpid() == atoi(argv[2]));
        yes("pidcheck.tid_is_pid", syscall(SYS_gettid) == getpid());
        flush();
        return 4;
    }
    if (readlink("/proc/self/exe", self, sizeof self - 1) < 0) {
        puts("no /proc/self/exe");
        return 1;
    }
    basics();
    alt_stacks();
    ends();
    puts("threads: done");
    return 0;
}
