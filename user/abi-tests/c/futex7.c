/* abi-futex7 (L7-SYNC): what Linux 7.3.1 added to the futex calls, for the ABI diff (CachyOS is the oracle).
 * Modes (argv[1]):
 *   robust   the op flags FUTEX_ROBUST_UNLOCK and FUTEX_ROBUST_LIST32 on FUTEX_WAKE, WAKE_BITSET and UNLOCK_PI
 *            (the word set to 0, list_op_pending cleared, error order, the robust list dropped when its own
 *            pending pointer cannot be written), and the vDSO's __vdso_futex_robust_list64_try_unlock
 *   futex2   futex_wake, futex_wait, futex_requeue (454-456) and futex_waitv with FUTEX2_NUMA / FUTEX2_MPOL
 *   exit     a thread that dies with list_op_pending naming a futex another thread owns without
 *            FUTEX_WAITERS: its exit walk wakes the waiter (7.3.1's handle_futex_death)
 *   fixup    the vDSO try-unlock single-stepped (TF): a signal taken between its cmpxchg and its store finds
 *            list_op_pending already cleared by the kernel (7.3.1's futex_fixup_robust_unlock)
 * Raw syscalls only; results, errno names and yes/no relations. */
#include "abi.h"
#include <pthread.h>
#include <signal.h>
#include <linux/futex.h>
#include <stdatomic.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include "vdsosym.h"

#define ROBUST_UNLOCK 512
#define ROBUST_LIST32 1024
#define F2_U8 0x00
#define F2_U32 0x02
#define F2_U64 0x03
#define F2_NUMA 0x04
#define F2_MPOL 0x08
#define F2_PRIV 128
#define NR_WAKE2 454
#define NR_WAIT2 455
#define NR_REQUEUE2 456
/* Every bit of a 32-bit futex: a mask must fit the futex size (an unsigned long argument). */
#define ALL 0xfffffffful

struct waitv {
    uint64_t val;
    uint64_t uaddr;
    uint32_t flags;
    uint32_t reserved;
};

struct rhead {
    void *next;
    long offset;
    void *pending;
};

static uint32_t tid(void) {
    return (uint32_t)syscall(SYS_gettid);
}

static void robust_ops(void) {
    static _Alignas(8) uint32_t w[4];
    static uint64_t pop;
    puts("== robust unlock");
    pop = 0x1234;
    SYS("WAKE|LIST32 alone is a plain wake", SYS_futex, w, FUTEX_WAKE_PRIVATE | ROBUST_LIST32, 1, 0, 0, 0);
    SYS("WAIT|ROBUST_UNLOCK", SYS_futex, w, FUTEX_WAIT_PRIVATE | ROBUST_UNLOCK, 0, 0, 0, 0);
    SYS("WAIT|ROBUST_UNLOCK, bad timeout (read first)", SYS_futex, w, FUTEX_WAIT_PRIVATE | ROBUST_UNLOCK, 0, KERNEL_ADDR, 0, 0);
    SYS("WAKE_OP|ROBUST_UNLOCK", SYS_futex, w, FUTEX_WAKE_OP_PRIVATE | ROBUST_UNLOCK, 1, 0, w + 1, 0);
    SYS("LOCK_PI|ROBUST_UNLOCK", SYS_futex, w, FUTEX_LOCK_PI_PRIVATE | ROBUST_UNLOCK, 0, 0, 0, 0);
    SYS("WAKE|CLOCK_REALTIME|ROBUST_UNLOCK", SYS_futex, w, FUTEX_WAKE_PRIVATE | FUTEX_CLOCK_REALTIME | ROBUST_UNLOCK, 1, 0, 0, 0);
    SYS("CMP_REQUEUE|LIST32", SYS_futex, w, FUTEX_CMP_REQUEUE_PRIVATE | ROBUST_LIST32, 1, 0, w + 1, 0);

    w[0] = tid();
    SYS("UNLOCK_WAKE_LIST64", SYS_futex, w, FUTEX_WAKE_PRIVATE | ROBUST_UNLOCK, 1, 0, &pop, 0);
    printf("  word %u, pending %llu\n", w[0], (unsigned long long)pop);
    w[0] = 7;
    pop = 0xffffffffffffffffull;
    SYS("UNLOCK_WAKE_LIST32", SYS_futex, w, FUTEX_WAKE_PRIVATE | ROBUST_UNLOCK | ROBUST_LIST32, 1, 0, &pop, 0);
    printf("  word %u, pending %#llx\n", w[0], (unsigned long long)pop);
    w[0] = 7;
    pop = 5;
    SYS("UNLOCK_BITSET_LIST64, bitset 0", SYS_futex, w, FUTEX_WAKE_BITSET_PRIVATE | ROBUST_UNLOCK, 1, 0, &pop, 0);
    printf("  word %u, pending %llu\n", w[0], (unsigned long long)pop);
    SYS("UNLOCK_BITSET_LIST64, bitset 1", SYS_futex, w, FUTEX_WAKE_BITSET_PRIVATE | ROBUST_UNLOCK, 1, 0, &pop, 1);
    printf("  word %u, pending %llu\n", w[0], (unsigned long long)pop);
    w[0] = 7;
    SYS("UNLOCK_WAKE_LIST64, misaligned word", SYS_futex, (char *)w + 2, FUTEX_WAKE_PRIVATE | ROBUST_UNLOCK, 1, 0, &pop, 0);
    printf("  word %u\n", w[0]);
    SYS("UNLOCK_WAKE_LIST64, word in the kernel", SYS_futex, KERNEL_ADDR, FUTEX_WAKE_PRIVATE | ROBUST_UNLOCK, 1, 0, &pop, 0);
    pop = 5;
    SYS("UNLOCK_WAKE_LIST64, pending in the kernel", SYS_futex, w, FUTEX_WAKE_PRIVATE | ROBUST_UNLOCK, 1, 0, KERNEL_ADDR, 0);
    printf("  word %u (unlocked before the pending pointer)\n", w[0]);
    w[0] = 7;
    SYS("UNLOCK_WAKE_LIST64, shared, pending unmapped", SYS_futex, w, FUTEX_WAKE | ROBUST_UNLOCK, 1, 0, (void *)16, 0);
    printf("  word %u\n", w[0]);

    /* The thread's own list_op_pending that cannot be written: the robust list is dropped. */
    struct rhead *h = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    h->next = h;
    h->offset = 0;
    h->pending = 0;
    SYS("set_robust_list(page)", SYS_set_robust_list, h, sizeof *h);
    mprotect(h, 4096, PROT_READ);
    w[0] = 7;
    SYS("UNLOCK_WAKE_LIST64, own pending read-only", SYS_futex, w, FUTEX_WAKE_PRIVATE | ROBUST_UNLOCK, 1, 0, &h->pending, 0);
    void *got = (void *)1;
    size_t len = 0;
    syscall(SYS_get_robust_list, 0, &got, &len);
    yes("  robust list dropped", got == 0);
    SYS("set_robust_list(page) again", SYS_set_robust_list, h, sizeof *h);
    SYS("UNLOCK_WAKE_LIST64, other read-only pointer", SYS_futex, w, FUTEX_WAKE_PRIVATE | ROBUST_UNLOCK, 1, 0, (char *)&h->pending + 8, 0);
    syscall(SYS_get_robust_list, 0, &got, &len);
    yes("  robust list kept", got == h);
    SYS("set_robust_list(NULL)", SYS_set_robust_list, 0, sizeof *h);
    munmap(h, 4096);

    puts("== robust unlock pi");
    w[0] = tid();
    pop = 9;
    SYS("UNLOCK_PI_LIST64", SYS_futex, w, FUTEX_UNLOCK_PI_PRIVATE | ROBUST_UNLOCK, 0, 0, &pop, 0);
    printf("  word %u, pending %llu\n", w[0], (unsigned long long)pop);
    w[0] = tid() + 1;
    pop = 9;
    SYS("UNLOCK_PI_LIST64, not the owner", SYS_futex, w, FUTEX_UNLOCK_PI_PRIVATE | ROBUST_UNLOCK, 0, 0, &pop, 0);
    printf("  pending %llu\n", (unsigned long long)pop);
    w[0] = tid();
    SYS("UNLOCK_PI_LIST64, pending in the kernel", SYS_futex, w, FUTEX_UNLOCK_PI_PRIVATE | ROBUST_UNLOCK, 0, 0, KERNEL_ADDR, 0);
    printf("  word %u\n", w[0]);
    w[0] = tid();
    pop = 0xffffffffffffffffull;
    SYS("UNLOCK_PI_LIST32", SYS_futex, w, FUTEX_UNLOCK_PI_PRIVATE | ROBUST_UNLOCK | ROBUST_LIST32, 0, 0, &pop, 0);
    printf("  word %u, pending %#llx\n", w[0], (unsigned long long)pop);

    puts("== vdso try unlock");
    typedef uint32_t (*try_fn)(uint32_t *, uint32_t, uint64_t *);
    try_fn f = (try_fn)vdso_sym((void *)getauxval(AT_SYSINFO_EHDR), "__vdso_futex_robust_list64_try_unlock", "LINUX_2.6");
    yes("__vdso_futex_robust_list64_try_unlock", f != 0);
    if (!f)
        return;
    w[0] = tid();
    pop = 3;
    yes("  uncontended: returns the tid", f(w, tid(), &pop) == tid());
    printf("  word %u, pending %llu\n", w[0], (unsigned long long)pop);
    w[0] = tid() | FUTEX_WAITERS;
    pop = 3;
    yes("  with waiters: returns the word", f(w, tid(), &pop) == (tid() | FUTEX_WAITERS));
    yes("  word kept", w[0] == (tid() | FUTEX_WAITERS));
    printf("  pending %llu\n", (unsigned long long)pop);
}

static volatile uint32_t fx_word;
static volatile uint64_t fx_pop;
static volatile int fx_steps, fx_stale;

static void on_step(int s) {
    (void)s;
    fx_steps++;
    if (fx_word == 0 && fx_pop != 0)
        fx_stale = 1;
}

/* TF on around the call: every instruction traps, so a SIGTRAP is taken inside the critical section too. The
 * stack pointer steps over the red zone before pushfq. */
static void fixup(void) {
    puts("== try unlock single-stepped");
    typedef uint32_t (*try_fn)(volatile uint32_t *, uint32_t, volatile uint64_t *);
    try_fn f = (try_fn)vdso_sym((void *)getauxval(AT_SYSINFO_EHDR), "__vdso_futex_robust_list64_try_unlock", "LINUX_2.6");
    yes("__vdso_futex_robust_list64_try_unlock", f != 0);
    if (!f)
        return;
    signal(SIGTRAP, on_step);
    uint32_t me = tid();
    fx_word = me;
    fx_pop = 3;
    asm volatile("lea -128(%%rsp), %%rsp\n\tpushfq\n\torq $0x100, (%%rsp)\n\tpopfq\n\tlea 128(%%rsp), %%rsp" ::: "memory");
    uint32_t r = f(&fx_word, me, &fx_pop);
    asm volatile("lea -128(%%rsp), %%rsp\n\tpushfq\n\tandq $~0x100, (%%rsp)\n\tpopfq\n\tlea 128(%%rsp), %%rsp" ::: "memory");
    signal(SIGTRAP, SIG_DFL);
    yes("  unlocked, pending cleared", r == me && fx_word == 0 && fx_pop == 0);
    yes("  stepped through it (more than 4 traps)", fx_steps > 4);
    yes("  a signal saw the lock given away with list_op_pending still set", fx_stale);
}

static long wake2(void *u, unsigned long mask, int nr, unsigned flags) {
    return syscall(NR_WAKE2, u, mask, nr, flags);
}

static _Alignas(8) uint32_t w2[4];
static long w2res;

static void *w2_waiter(void *arg) {
    (void)arg;
    w2res = syscall(NR_WAIT2, w2, 0ul, ALL, F2_U32 | F2_PRIV, 0, 0);
    return 0;
}

static void futex2(void) {
    static _Alignas(8) uint32_t w[8];
    struct timespec past = {0, 1}, bad = {0, 1000000000};
    puts("== futex_wake");
    SYS("futex_wake(U32|PRIVATE)", NR_WAKE2, w, ALL, 1, F2_U32 | F2_PRIV);
    SYS("futex_wake(U32, shared)", NR_WAKE2, w, ALL, 1, F2_U32);
    SYS("futex_wake(nr 0)", NR_WAKE2, w, ALL, 0, F2_U32 | F2_PRIV);
    SYS("futex_wake(U8)", NR_WAKE2, w, ALL, 1, F2_U8 | F2_PRIV);
    SYS("futex_wake(U64)", NR_WAKE2, w, ALL, 1, F2_U64 | F2_PRIV);
    SYS("futex_wake(flag 0x10)", NR_WAKE2, w, ALL, 1, F2_U32 | 0x10);
    SYS("futex_wake(mask 1 << 32)", NR_WAKE2, w, 1ul << 32, 1, F2_U32 | F2_PRIV);
    SYS("futex_wake(mask ~0ul)", NR_WAKE2, w, ~0ul, 1, F2_U32 | F2_PRIV);
    SYS("futex_wake(mask 0)", NR_WAKE2, w, 0ul, 1, F2_U32 | F2_PRIV);
    SYS("futex_wake(misaligned)", NR_WAKE2, (char *)w + 2, ALL, 1, F2_U32 | F2_PRIV);
    SYS("futex_wake(kernel)", NR_WAKE2, KERNEL_ADDR, ALL, 1, F2_U32 | F2_PRIV);

    puts("== futex_wait");
    w[0] = 0;
    SYS("futex_wait(val 1)", NR_WAIT2, w, 1ul, ALL, F2_U32 | F2_PRIV, 0, 0);
    SYS("futex_wait(val 1 << 32)", NR_WAIT2, w, 1ul << 32, ALL, F2_U32 | F2_PRIV, 0, 0);
    SYS("futex_wait(mask 1 << 32)", NR_WAIT2, w, 0ul, 1ul << 32, F2_U32 | F2_PRIV, 0, 0);
    SYS("futex_wait(mask 0)", NR_WAIT2, w, 0ul, 0, F2_U32 | F2_PRIV, 0, 0);
    SYS("futex_wait(mask 0, bad timeout first)", NR_WAIT2, w, 0ul, 0, F2_U32 | F2_PRIV, KERNEL_ADDR, CLOCK_MONOTONIC);
    SYS("futex_wait(U16)", NR_WAIT2, w, 0ul, ALL, 0x01 | F2_PRIV, 0, 0);
    SYS("futex_wait(past, MONOTONIC)", NR_WAIT2, w, 0ul, ALL, F2_U32 | F2_PRIV, &past, CLOCK_MONOTONIC);
    SYS("futex_wait(past, REALTIME)", NR_WAIT2, w, 0ul, ALL, F2_U32 | F2_PRIV, &past, CLOCK_REALTIME);
    SYS("futex_wait(past, BOOTTIME)", NR_WAIT2, w, 0ul, ALL, F2_U32 | F2_PRIV, &past, CLOCK_BOOTTIME);
    SYS("futex_wait(nsec 1e9)", NR_WAIT2, w, 0ul, ALL, F2_U32 | F2_PRIV, &bad, CLOCK_MONOTONIC);
    SYS("futex_wait(val 1, clock 99 without timeout)", NR_WAIT2, w, 1ul, ALL, F2_U32 | F2_PRIV, 0, 99);

    pthread_t t;
    w2[0] = 0;
    pthread_create(&t, 0, w2_waiter, 0);
    long n = 0;
    for (int i = 0; i < 2000 && n == 0; i++) {
        n = wake2(w2, ALL, 1, F2_U32 | F2_PRIV);
        if (n < 0)
            break;
        if (!n)
            usleep(1000);
    }
    pthread_join(t, 0);
    printf("futex_wake woke the futex_wait thread: %ld, it returned %ld\n", n, w2res);

    puts("== futex_requeue");
    struct waitv rq[2] = {{0, (uintptr_t)&w[0], F2_U32 | F2_PRIV, 0}, {0, (uintptr_t)&w[2], F2_U32 | F2_PRIV, 0}};
    SYS("futex_requeue(NULL)", NR_REQUEUE2, 0, 0, 1, 1);
    SYS("futex_requeue(flags 1)", NR_REQUEUE2, rq, 1, 1, 1);
    SYS("futex_requeue", NR_REQUEUE2, rq, 0, 1, 1);
    SYS("futex_requeue(nr_wake -1)", NR_REQUEUE2, rq, 0, -1, 1);
    rq[0].val = 1;
    SYS("futex_requeue(value differs)", NR_REQUEUE2, rq, 0, 1, 1);
    rq[0].val = 0;
    rq[1].flags = F2_U32;
    SYS("futex_requeue(flags differ)", NR_REQUEUE2, rq, 0, 1, 1);
    rq[1].flags = F2_U32 | F2_PRIV | 0x40;
    SYS("futex_requeue(bad flag in entry 2)", NR_REQUEUE2, rq, 0, 1, 1);
    SYS("futex_requeue(kernel address)", NR_REQUEUE2, KERNEL_ADDR, 0, 1, 1);

    puts("== NUMA words");
    w[0] = 0;
    w[1] = 0xffffffff;
    SYS("futex_wake(NUMA), node -1", NR_WAKE2, w, ALL, 1, F2_U32 | F2_NUMA | F2_PRIV);
    printf("  node word %d\n", (int)w[1]);
    w[1] = 0xffffffff;
    SYS("futex_wake(NUMA|MPOL), node -1", NR_WAKE2, w, ALL, 1, F2_U32 | F2_NUMA | F2_MPOL | F2_PRIV);
    printf("  node word %d\n", (int)w[1]);
    w[1] = 5;
    SYS("futex_wake(NUMA), node 5", NR_WAKE2, w, ALL, 1, F2_U32 | F2_NUMA | F2_PRIV);
    w[1] = 4096;
    SYS("futex_wake(NUMA), node 4096", NR_WAKE2, w, ALL, 1, F2_U32 | F2_NUMA | F2_PRIV);
    w[2] = 0;
    w[3] = 0xffffffff;
    SYS("futex_wake(NUMA), 4-aligned only", NR_WAKE2, w + 1, ALL, 1, F2_U32 | F2_NUMA | F2_PRIV);
    SYS("futex_wake(MPOL alone)", NR_WAKE2, w + 2, ALL, 1, F2_U32 | F2_MPOL | F2_PRIV);
    printf("  word after it %d\n", (int)w[3]);
    w[1] = 0xffffffff;
    SYS("futex_wait(NUMA), value differs", NR_WAIT2, w, 1ul, ALL, F2_U32 | F2_NUMA | F2_PRIV, 0, 0);
    printf("  node word %d\n", (int)w[1]);
    w[1] = 0xffffffff;
    struct waitv wv[2] = {{0, (uintptr_t)&w[2], F2_U32 | F2_PRIV, 0}, {1, (uintptr_t)&w[0], F2_U32 | F2_NUMA | F2_PRIV, 0}};
    SYS("futex_waitv(NUMA entry differs)", 449, wv, 2, 0, 0, 0);
    printf("  node word %d\n", (int)w[1]);
    wv[1].val = 0;
    SYS("futex_waitv(NUMA, past)", 449, wv, 2, 0, &past, CLOCK_MONOTONIC);
    wv[1].flags = F2_U32 | F2_NUMA | F2_MPOL;
    SYS("futex_waitv(NUMA|MPOL shared, past)", 449, wv, 2, 0, &past, CLOCK_MONOTONIC);
    wv[1].flags = F2_U32 | 0x20;
    SYS("futex_waitv(flag 0x20)", 449, wv, 2, 0, &past, CLOCK_MONOTONIC);
    wv[1].flags = F2_U64 | F2_PRIV;
    SYS("futex_waitv(U64)", 449, wv, 2, 0, &past, CLOCK_MONOTONIC);
    w[1] = 3;
    wv[1].flags = F2_U32 | F2_NUMA | F2_PRIV;
    SYS("futex_waitv(NUMA, node 3)", 449, wv, 2, 0, &past, CLOCK_MONOTONIC);
    w[1] = 0xffffffff;
    rq[0].flags = rq[1].flags = F2_U32 | F2_NUMA | F2_PRIV;
    rq[1].uaddr = (uintptr_t)&w[4];
    w[5] = 0xffffffff;
    SYS("futex_requeue(NUMA)", NR_REQUEUE2, rq, 0, 1, 1);
    printf("  node words %d %d\n", (int)w[1], (int)w[5]);
}

static _Alignas(8) _Atomic uint32_t lockword;
static long exit_res;

static void *exit_waiter(void *arg) {
    (void)arg;
    struct timespec t = {3, 0};
    exit_res = syscall(SYS_futex, &lockword, FUTEX_WAIT, 0x1234, &t, 0, 0);
    return 0;
}

static void *exit_dier(void *arg) {
    (void)arg;
    static struct rhead h;
    h.next = &h;
    h.offset = 0;
    h.pending = (void *)&lockword;
    syscall(SYS_set_robust_list, &h, sizeof h);
    syscall(SYS_exit, 0);
    return 0;
}

static void exit_walk(void) {
    puts("== pending op at exit");
    lockword = 0x1234;
    pthread_t w, d;
    pthread_create(&w, 0, exit_waiter, 0);
    usleep(300000);
    pthread_create(&d, 0, exit_dier, 0);
    pthread_join(w, 0);
    printf("waiter of an owned word without FUTEX_WAITERS: %s\n", exit_res == 0 ? "woken" : ename(errno));
    printf("word %#x\n", lockword);
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    const char *m = argc > 1 ? argv[1] : "";
    if (!strcmp(m, "robust"))
        robust_ops();
    else if (!strcmp(m, "futex2"))
        futex2();
    else if (!strcmp(m, "exit"))
        exit_walk();
    else if (!strcmp(m, "fixup"))
        fixup();
    else {
        puts("mode: robust, futex2, exit or fixup");
        return 2;
    }
    puts("== end");
    return 0;
}
