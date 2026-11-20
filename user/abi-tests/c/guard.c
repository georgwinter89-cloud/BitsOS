/* L7-VFS: guard regions (MADV_GUARD_INSTALL 102 / MADV_GUARD_REMOVE 103, linux-7.3.1/mm/madvise.c:1124, :1252): a
 * touch is SIGSEGV with SEGV_MAPERR, read(2)/write(2) through one EFAULT, MADV_POPULATE EFAULT, mlock over one
 * ENOMEM; the pages under an install lose their data; DONTNEED keeps the guards, REMOVE, munmap and MAP_FIXED take
 * them; fork copies them, mremap moves them; a locked VMA refuses an install (EINVAL), not a remove. */
#include "abi.h"
#include <setjmp.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096L
#define GUARD_INSTALL 102
#define GUARD_REMOVE 103
#ifndef MADV_POPULATE_READ
#define MADV_POPULATE_READ 22
#define MADV_POPULATE_WRITE 23
#endif

static sigjmp_buf back;
static volatile int code;

static void on_segv(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)uc;
    code = si->si_code;
    siglongjmp(back, 1);
}

/* "label = ok <byte>" or "label = SIGSEGV <code>". */
static void touch(const char *label, volatile char *p, int write) {
    code = 0;
    if (sigsetjmp(back, 1) == 0) {
        char c = write ? (*p = 'w') : *p;
        printf("%s = ok %d\n", label, c);
    } else {
        printf("%s = SIGSEGV %s\n", label, code == SEGV_MAPERR ? "SEGV_MAPERR" : code == SEGV_ACCERR ? "SEGV_ACCERR" : "other");
    }
}

static long adv(char *p, long len, int a) {
    return madvise(p, len, a) ? -1 : 0;
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    struct sigaction sa = {0};
    sa.sa_sigaction = on_segv;
    sa.sa_flags = SA_SIGINFO | SA_NODEFER;
    sigaction(SIGSEGV, &sa, 0);
    char *m = mmap(0, 8 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    for (int i = 0; i < 8; i++)
        m[i * PG] = 'a' + i;
    show("advice 104", adv(m, PG, 104));
    show("install 2..5", adv(m + 2 * PG, 3 * PG, GUARD_INSTALL));
    touch("read a guard", m + 2 * PG, 0);
    touch("write a guard", m + 4 * PG, 1);
    touch("read beside it", m + PG, 0);
    touch("read past it", m + 5 * PG, 0);
    show("install again", adv(m + 3 * PG, PG, GUARD_INSTALL));
    int p[2];
    if (pipe(p))
        return 1;
    show("write(2) from a guard", write(p[1], m + 2 * PG, 16));
    show("write(2) across into it", write(p[1], m + 2 * PG - 8, 16));
    show("write 1 byte", write(p[1], "x", 1));
    show("read(2) into a guard", read(p[0], m + 3 * PG, 1));
    show("POPULATE_WRITE over it", adv(m, 8 * PG, MADV_POPULATE_WRITE));
    show("POPULATE_READ before it", adv(m, 2 * PG, MADV_POPULATE_READ));
    show("DONTNEED all", adv(m, 8 * PG, MADV_DONTNEED));
    touch("a guard after DONTNEED", m + 3 * PG, 0);
    touch("a page after DONTNEED", m + 6 * PG, 0);
    show("remove 3", adv(m + 3 * PG, PG, GUARD_REMOVE));
    touch("the removed page", m + 3 * PG, 0);
    touch("2 stays", m + 2 * PG, 0);
    pid_t c = fork();
    if (c == 0) {
        touch("child: 4", m + 4 * PG, 0);
        touch("child: 3", m + 3 * PG, 0);
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    char *to = mmap(0, 8 * PG, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *n = mremap(m, 8 * PG, 8 * PG, MREMAP_MAYMOVE | MREMAP_FIXED, to);
    printf("mremap moved = %s\n", n == to ? "yes" : "no");
    touch("moved guard", to + 2 * PG, 0);
    touch("moved page", to + PG, 0);
    show("munmap 2..3", munmap(to + 2 * PG, PG));
    show("map it again", mmap(to + 2 * PG, PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == to + 2 * PG ? 0 : -1);
    touch("after munmap and mmap", to + 2 * PG, 0);
    show("MAP_FIXED over 4", mmap(to + 4 * PG, PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == to + 4 * PG ? 0 : -1);
    touch("after MAP_FIXED", to + 4 * PG, 0);
    /* A write fault to a guard of a read-only mapping is the mapping's SEGV_ACCERR. */
    char *r = mmap(0, 2 * PG, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    show("install read-only", adv(r, 2 * PG, GUARD_INSTALL));
    touch("write a read-only guard", r, 1);
    touch("read a read-only guard", r, 0);
    show("remove across a hole", (munmap(r + PG, PG), adv(r, 2 * PG, GUARD_REMOVE)));
    /* Locked: no install; mlock over a guard is ENOMEM and locks anyway; a remove is fine. */
    char *l = mmap(0, 2 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    show("install on l", adv(l, PG, GUARD_INSTALL));
    show("mlock over it", mlock(l, 2 * PG));
    show("install on a locked page", adv(l + PG, PG, GUARD_INSTALL));
    show("remove on a locked page", adv(l, PG, GUARD_REMOVE));
    touch("after the remove", l, 0);
    return 0;
}
