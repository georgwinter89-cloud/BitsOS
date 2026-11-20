/* FSR: an open file description's position (Linux 7 fdget_pos / file_needs_f_pos_lock). read, write and lseek
 * on a shared description see one position, each read takes its own bytes; pread/pwrite leave it alone; a
 * device (/dev/null) shared by many writers works without anyone waiting on its position. Cases (argv[1]):
 *   dup      dup'd fds share the position; a reopen does not
 *   fork     a child's reads move the parent's position (one description); after the child ends the parent goes on
 *   threads  eight threads read 4 KiB records of one fd: every record exactly once, the position at the end
 *   procs    four processes (fork) read records of the inherited fd: every record exactly once
 *   alone    one thread: read/lseek/pread/write keep their meaning; a thread that came and went changes nothing
 *   devnull  eight threads and four processes write to one /dev/null description; lseek answers 0 */
#include "abi.h"
#include <fcntl.h>
#include <pthread.h>
#include <sys/wait.h>

#define REC 4096
#define NREC 512

static char path[64];

static int make_file(void) {
    snprintf(path, sizeof path, "/tmp/abi-fpos-%d", getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    static char rec[REC];
    for (int i = 0; i < NREC; i++) {
        memset(rec, 0, REC);
        memcpy(rec, &i, sizeof i);
        if (write(fd, rec, REC) != REC) return -1;
    }
    lseek(fd, 0, SEEK_SET);
    return fd;
}

static int rfd;
static unsigned char seen[NREC];
static pthread_mutex_t seen_lock = PTHREAD_MUTEX_INITIALIZER;
static int bad;

static void *reader(void *arg) {
    (void)arg;
    char b[REC];
    for (;;) {
        ssize_t n = read(rfd, b, REC);
        if (n == 0) break;
        if (n != REC) { __atomic_fetch_add(&bad, 1, __ATOMIC_RELAXED); break; }
        int i;
        memcpy(&i, b, sizeof i);
        pthread_mutex_lock(&seen_lock);
        if (i >= 0 && i < NREC) seen[i]++; else bad++;
        pthread_mutex_unlock(&seen_lock);
    }
    return NULL;
}

static int once_each(void) {
    for (int i = 0; i < NREC; i++) if (seen[i] != 1) return 0;
    return 1;
}

static void case_dup(void) {
    int fd = make_file();
    char b[10];
    int d = dup(fd);
    read(fd, b, 10);
    printf("dup sees 10: %ld\n", (long)lseek(d, 0, SEEK_CUR));
    read(d, b, 6);
    printf("fd sees 16: %ld\n", (long)lseek(fd, 0, SEEK_CUR));
    int o = open(path, O_RDONLY);
    printf("reopen at 0: %ld\n", (long)lseek(o, 0, SEEK_CUR));
    close(d);
    read(fd, b, 4);
    printf("after close of dup 20: %ld\n", (long)lseek(fd, 0, SEEK_CUR));
    close(o);
    close(fd);
    unlink(path);
}

static void case_fork(void) {
    int fd = make_file();
    char b[REC];
    read(fd, b, 100);
    pid_t c = fork();
    if (c == 0) {
        read(fd, b, 1000);
        _exit(lseek(fd, 0, SEEK_CUR) == 1100 ? 0 : 1);
    }
    int st;
    waitpid(c, &st, 0);
    yes("child saw 1100", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    printf("parent after child: %ld\n", (long)lseek(fd, 0, SEEK_CUR));
    read(fd, b, 10);
    printf("parent goes on: %ld\n", (long)lseek(fd, 0, SEEK_CUR));
    close(fd);
    unlink(path);
}

static void case_threads(void) {
    rfd = make_file();
    pthread_t t[8];
    for (int i = 0; i < 8; i++) pthread_create(&t[i], NULL, reader, NULL);
    for (int i = 0; i < 8; i++) pthread_join(t[i], NULL);
    yes("every record once", once_each() && !bad);
    printf("end: %ld\n", (long)lseek(rfd, 0, SEEK_CUR));
    close(rfd);
    unlink(path);
}

static void case_procs(void) {
    rfd = make_file();
    int p[2];
    pipe(p);
    for (int k = 0; k < 4; k++) {
        if (fork() == 0) {
            close(p[0]);
            char b[REC];
            for (;;) {
                ssize_t n = read(rfd, b, REC);
                if (n != REC) _exit(n == 0 ? 0 : 1);
                write(p[1], b, sizeof(int));
            }
        }
    }
    close(p[1]);
    int i, ok = 1;
    while (read(p[0], &i, sizeof i) == sizeof i) {
        if (i >= 0 && i < NREC) seen[i]++; else ok = 0;
    }
    for (int k = 0; k < 4; k++) {
        int st;
        wait(&st);
        if (!WIFEXITED(st) || WEXITSTATUS(st)) ok = 0;
    }
    yes("every record once", once_each() && ok);
    printf("end: %ld\n", (long)lseek(rfd, 0, SEEK_CUR));
    close(rfd);
    unlink(path);
}

static void *nothing(void *a) { return a; }

static void case_alone(void) {
    int fd = make_file();
    char b[REC];
    read(fd, b, 3);
    printf("after read 3: %ld\n", (long)lseek(fd, 0, SEEK_CUR));
    pread(fd, b, 50, 1000);
    printf("pread leaves it: %ld\n", (long)lseek(fd, 0, SEEK_CUR));
    printf("seek end: %ld\n", (long)lseek(fd, 0, SEEK_END));
    printf("write 5 at end: %ld\n", (long)write(fd, "fivee", 5));
    printf("after write: %ld\n", (long)lseek(fd, 0, SEEK_CUR));
    pwrite(fd, "xx", 2, 0);
    printf("pwrite leaves it: %ld\n", (long)lseek(fd, 0, SEEK_CUR));
    pthread_t t;
    pthread_create(&t, NULL, nothing, NULL);
    pthread_join(t, NULL);
    lseek(fd, 8, SEEK_SET);
    read(fd, b, 8);
    printf("after a thread came and went: %ld\n", (long)lseek(fd, 0, SEEK_CUR));
    close(fd);
    unlink(path);
}

static int nfd;
static void *nwriter(void *a) {
    static char b[65536];
    for (int i = 0; i < 200; i++)
        if (write(nfd, b, sizeof b) != sizeof b) __atomic_fetch_add(&bad, 1, __ATOMIC_RELAXED);
    return a;
}

static void case_devnull(void) {
    nfd = open("/dev/null", O_WRONLY);
    pthread_t t[8];
    for (int i = 0; i < 8; i++) pthread_create(&t[i], NULL, nwriter, NULL);
    pid_t c[4];
    for (int k = 0; k < 4; k++)
        if ((c[k] = fork()) == 0) {
            bad = 0;
            nwriter(NULL);
            _exit(bad ? 1 : 0);
        }
    for (int i = 0; i < 8; i++) pthread_join(t[i], NULL);
    int ok = !bad;
    for (int k = 0; k < 4; k++) {
        int st;
        waitpid(c[k], &st, 0);
        if (!WIFEXITED(st) || WEXITSTATUS(st)) ok = 0;
    }
    yes("every write took everything", ok);
    printf("lseek: %ld\n", (long)lseek(nfd, 0, SEEK_CUR));
    close(nfd);
}

int main(int argc, char **argv) {
    const char *c = argc > 1 ? argv[1] : "";
    if (!strcmp(c, "dup")) case_dup();
    else if (!strcmp(c, "fork")) case_fork();
    else if (!strcmp(c, "threads")) case_threads();
    else if (!strcmp(c, "procs")) case_procs();
    else if (!strcmp(c, "alone")) case_alone();
    else if (!strcmp(c, "devnull")) case_devnull();
    else { printf("unknown case\n"); return 2; }
    return 0;
}
