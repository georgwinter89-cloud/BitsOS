/* MM-tlb2: Cobalt's memory pattern during playback (YT-4a CR 7, the TLB shootdown IPI storm) through the real
 * syscall and page-fault path, checked for content: four decoder threads map a 2-8 MB buffer per frame at
 * 60 Hz for one second, fill every page, give the first half back with MADV_DONTNEED (it must read as zero,
 * the second half keeps its bytes) and unmap it; then copy-on-write reuse with threads that hold the old
 * read-only translation (after a fork whose child ended, the parent's write makes the page writable without a
 * shootdown; the other threads' stores still land, and a read() into the page from a thread that still holds
 * the read-only entry, a kernel copy through it, succeeds: no EFAULT, fixer of MM-tlb2). Prints counts of
 * wrong bytes and failed reads only (0 on both kernels).
 * The kernel side counts the rounds and IPIs (ktest mm.tlb.cobalt). Modes: none. */
#include "abi.h"
#include <pthread.h>
#include <sched.h>
#include <sys/mman.h>
#include <sys/wait.h>

#define THREADS 4
#define FRAMES 60
#define P 4096ul

static int bad[THREADS];

static void tick(struct timespec *t) {
    t->tv_nsec += 1000000000l / 60;
    if (t->tv_nsec >= 1000000000l) {
        t->tv_sec++;
        t->tv_nsec -= 1000000000l;
    }
    clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, t, 0);
}

static void *decoder(void *arg) {
    int k = (int)(intptr_t)arg;
    uint64_t seed = 0x9E3779B97F4A7C15ull ^ (uint64_t)k;
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    for (int f = 0; f < FRAMES; f++) {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        size_t len = (size_t)(2 + seed % 7) << 20;
        unsigned char v = (unsigned char)(1 + k * FRAMES + f);
        unsigned char *p = mmap(0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) {
            bad[k]++;
            continue;
        }
        for (size_t o = 0; o < len; o += P) p[o + (o / P) % P] = v;
        if (madvise(p, len / 2, MADV_DONTNEED)) bad[k]++;
        for (size_t o = 0; o < len / 2; o += P) bad[k] += p[o + (o / P) % P] != 0;
        for (size_t o = len / 2; o < len; o += P) bad[k] += p[o + (o / P) % P] != v;
        if (munmap(p, len)) bad[k]++;
        tick(&t);
    }
    return 0;
}

#define ROUNDS 64

static volatile unsigned char *page;
static volatile int go, seen;
static int pipefd[2];
static int efaults;
static int ncpu = 1;

/* Holder k on its own CPU (1, 2, ...), the writer on CPU 0: the read-only entries stay on other CPUs. */
static void pin(int cpu) {
    if (ncpu < 2) return;
    cpu_set_t s;
    CPU_ZERO(&s);
    CPU_SET(cpu % ncpu, &s);
    sched_setaffinity(0, sizeof s, &s);
}

static void *holder(void *arg) {
    int k = (int)(intptr_t)arg;
    pin(1 + k % (ncpu > 1 ? ncpu - 1 : 1));
    unsigned sum = 0;
    for (int i = 0; i < 1000; i++) sum += page[i % 64];
    __atomic_add_fetch(&seen, 1, __ATOMIC_SEQ_CST);
    while (!__atomic_load_n(&go, __ATOMIC_SEQ_CST)) sum += page[0];
    /* The kernel writes first, through this thread's read-only entry if it is still there. */
    if (read(pipefd[0], (void *)(page + 128 + 8 * k), 8) != 8) __atomic_add_fetch(&efaults, 1, __ATOMIC_SEQ_CST);
    page[64 + k] = (unsigned char)(0x40 + k);
    return (void *)(uintptr_t)(sum & 0);
}

static int cow_reuse(void) {
    page = mmap(0, P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) return -1;
    memset((void *)page, 7, P);
    pid_t c = fork();
    if (c == 0) _exit(0);
    int st = 0;
    if (c < 0 || waitpid(c, &st, 0) != c) return -1;
    /* The child's memory may still be on its way back: then the write below copies instead of reusing. */
    struct timespec rest = {0, 20 * 1000000l};
    nanosleep(&rest, 0);
    static const unsigned char fill[8 * THREADS] = {[0 ... 8 * THREADS - 1] = 0x5a};
    if (write(pipefd[1], fill, sizeof fill) != (ssize_t)sizeof fill) return -1;
    go = seen = 0;
    pthread_t th[THREADS];
    for (int k = 0; k < THREADS; k++) pthread_create(&th[k], 0, holder, (void *)(intptr_t)k);
    while (__atomic_load_n(&seen, __ATOMIC_SEQ_CST) < THREADS) {
    }
    page[1] = 9; /* the parent's frame is its own again: made writable in place */
    __atomic_store_n(&go, 1, __ATOMIC_SEQ_CST);
    for (int k = 0; k < THREADS; k++) pthread_join(th[k], 0);
    int wrong = page[0] != 7 || page[1] != 9;
    for (int k = 0; k < THREADS; k++) wrong += page[64 + k] != 0x40 + k;
    for (int i = 0; i < 8 * THREADS; i++) wrong += page[128 + i] != 0x5a;
    munmap((void *)page, P);
    return wrong;
}

int main(void) {
    pthread_t th[THREADS];
    for (int k = 0; k < THREADS; k++) pthread_create(&th[k], 0, decoder, (void *)(intptr_t)k);
    int wrong = 0;
    for (int k = 0; k < THREADS; k++) {
        pthread_join(th[k], 0);
        wrong += bad[k];
    }
    printf("decoders: %d threads x %d frames, wrong %d\n", THREADS, FRAMES, wrong);
    if (pipe(pipefd)) return 1;
    cpu_set_t all;
    if (sched_getaffinity(0, sizeof all, &all) == 0) ncpu = CPU_COUNT(&all);
    pin(0);
    int cow = 0;
    for (int r = 0; r < ROUNDS; r++) {
        int w = cow_reuse();
        cow += w < 0 ? 1000 : w;
    }
    printf("cow reuse: %d rounds, wrong %d, failed reads %d\n", ROUNDS, cow, efaults);
    printf("tlbstress: done\n");
    return 0;
}
