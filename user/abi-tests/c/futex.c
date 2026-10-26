/* abi-futex (SC-5): the futex and futex_waitv system calls as one thread sees them, for the ABI diff
 * (CachyOS is the oracle) and the ktest sc.futex.abi (compared with expected/futex.txt). Prints results, errno
 * names and yes/no relations only. Modes (argv[1]):
 *   (none)   argument rules and error order of every operation, timeouts, FUTEX_WAKE_OP's arithmetic,
 *            requeue checks, shared keys on anonymous, read-only and file memory, futex_waitv
 *   pi       the PI operations: argument and word rules on one thread, then (SC-7b) with helper threads: the
 *            hand-over by FUTEX_UNLOCK_PI, a contended trylock, plain operations meeting a PI waiter, the
 *            owner's exit without and with a robust list (FUTEX_OWNER_DIED), timeouts, ESRCH, EDEADLK
 *   worker PATH INDEX ITERS   one of many processes contending for a process-shared futex mutex in the
 *            MAP_SHARED file PATH (layout below); used by ktest sc.futex.procs64, prints one result line
 *   check PATH N ITERS        after N workers: the counter and every worker's progress, then removes PATH
 *   multi    several waiter threads (musl pthreads; V2 needs AB-4's clone): the counts FUTEX_WAKE, REQUEUE,
 *            CMP_REQUEUE, WAKE_OP and WAKE_BITSET report with real waiters (expected/futex-multi.txt)
 *   robust-die PATH    takes a robust lock word in the MAP_SHARED file PATH (owner = its TID, FUTEX_WAITERS set)
 *            with its robust list registered, and exits holding it
 *   robust-check PATH  afterwards: whether the kernel's robust-list walk at the exit marked the word
 *            FUTEX_OWNER_DIED, kept FUTEX_WAITERS and cleared the TID; then removes PATH
 * Raw futex syscalls throughout, so musl adds nothing of its own. */
#include "abi.h"
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <linux/futex.h>
#include <stdatomic.h>
#include <sys/mman.h>
#include <sys/stat.h>

#ifndef FUTEX_32
#define FUTEX_32 2
#endif

struct waitv {
    uint64_t val;
    uint64_t uaddr;
    uint32_t flags;
    uint32_t reserved;
};

static long fx(void *u, long op, long val, const void *t, void *u2, long val3) {
    return syscall(SYS_futex, u, op, val, t, u2, val3);
}
/* The "timeout" argument as an integer (val2 of REQUEUE/CMP_REQUEUE/WAKE_OP). */
static long fx2(void *u, long op, long val, unsigned long val2, void *u2, long val3) {
    return syscall(SYS_futex, u, op, val, val2, u2, val3);
}
static long waitv(struct waitv *w, long n, long flags, const struct timespec *t, long clk) {
    return syscall(449, w, n, flags, t, clk);
}

static struct timespec at_ns(clockid_t c, uint64_t from_now) {
    uint64_t n = ns_of(c) + from_now;
    struct timespec t = {(time_t)(n / 1000000000u), (long)(n % 1000000000u)};
    return t;
}

static void errors(void) {
    static _Alignas(8) uint32_t w[16];
    struct timespec ms10 = {0, 10000000}, bad_ns = {0, 1000000000}, neg = {-1, 0}, zero = {0, 0};
    char *mis = (char *)w + 2;
    w[0] = 5;
    puts("== wait/wake");
    SYS("wake.nobody.private", SYS_futex, w, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0);
    SYS("wake.nobody.shared", SYS_futex, w, FUTEX_WAKE, 1, 0, 0, 0);
    SYS("wake.zero_count", SYS_futex, w, FUTEX_WAKE_PRIVATE, 0, 0, 0, 0);
    SYS("wait.mismatch.private", SYS_futex, w, FUTEX_WAIT_PRIVATE, 6, 0, 0, 0);
    SYS("wait.mismatch.shared", SYS_futex, w, FUTEX_WAIT, 6, 0, 0, 0);
    uint64_t s0 = ns_of(CLOCK_MONOTONIC);
    show("wait.timeout_10ms", fx(w, FUTEX_WAIT_PRIVATE, 5, &ms10, 0, 0));
    yes("wait.waited_10ms", ns_of(CLOCK_MONOTONIC) - s0 >= 10000000u);
    show("wait.timeout_zero", fx(w, FUTEX_WAIT_PRIVATE, 5, &zero, 0, 0));
    show("wait.timeout_bad_ns", fx(w, FUTEX_WAIT_PRIVATE, 5, &bad_ns, 0, 0));
    show("wait.timeout_negative", fx(w, FUTEX_WAIT_PRIVATE, 5, &neg, 0, 0));
    show("wait.timeout_fault", fx(w, FUTEX_WAIT_PRIVATE, 6, KERNEL_ADDR, 0, 0));
    show("wait.bad_ns_and_mismatch", fx(w, FUTEX_WAIT_PRIVATE, 6, &bad_ns, 0, 0));
    show("wait.misaligned", fx(mis, FUTEX_WAIT_PRIVATE, 5, 0, 0, 0));
    show("wake.misaligned", fx(mis, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0));
    show("wait.null.private", fx(0, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0));
    show("wake.null.private", fx(0, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0));
    show("wake.null.shared", fx(0, FUTEX_WAKE, 1, 0, 0, 0));
    show("wake.kernel.private", fx(KERNEL_ADDR, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0));
    show("wake.noncanonical.private", fx((void *)0x0000900000000000ul, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0));
    show("wait.noncanonical.private", fx((void *)0x0000900000000000ul, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0));
    show("wake.noncanonical.shared", fx((void *)0x0000900000000000ul, FUTEX_WAKE, 1, 0, 0, 0));
    /* access_ok's inclusive bound (USER_PTR_MAX): the first address past user space still passes. */
    show("wake.user_top.private", fx((void *)0x00007ffffffff000ul, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0));
    show("wake.user_top_plus4.private", fx((void *)0x00007ffffffff004ul, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0));
    show("wake.user_top.shared", fx((void *)0x00007ffffffff000ul, FUTEX_WAKE, 1, 0, 0, 0));
    show("wait.user_top.private", fx((void *)0x00007ffffffff000ul, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0));
    show("unknown_op", fx(w, 99, 0, 0, 0, 0));
    show("futex_fd", fx(w, 2, 0, 0, 0, 0));

    puts("== bitset and clocks");
    show("wait_bitset.zero", fx(w, FUTEX_WAIT_BITSET_PRIVATE, 5, 0, 0, 0));
    show("wake_bitset.zero", fx(w, FUTEX_WAKE_BITSET_PRIVATE, 1, 0, 0, 0));
    show("wake_bitset.nobody", fx(w, FUTEX_WAKE_BITSET_PRIVATE, 1, 0, 0, 1));
    struct timespec past = {1, 0};
    show("wait_bitset.abs_past_mono", fx(w, FUTEX_WAIT_BITSET_PRIVATE, 5, &past, 0, ~0));
    show("wait_bitset.abs_past_real", fx(w, FUTEX_WAIT_BITSET_PRIVATE | FUTEX_CLOCK_REALTIME, 5, &past, 0, ~0));
    struct timespec soon = at_ns(CLOCK_MONOTONIC, 10000000);
    s0 = ns_of(CLOCK_MONOTONIC);
    show("wait_bitset.abs_10ms_mono", fx(w, FUTEX_WAIT_BITSET_PRIVATE, 5, &soon, 0, ~0));
    yes("wait_bitset.waited_until", ns_of(CLOCK_MONOTONIC) >= (uint64_t)soon.tv_sec * 1000000000u + (uint64_t)soon.tv_nsec);
    soon = at_ns(CLOCK_REALTIME, 10000000);
    show("wait_bitset.abs_10ms_real", fx(w, FUTEX_WAIT_BITSET_PRIVATE | FUTEX_CLOCK_REALTIME, 5, &soon, 0, ~0));
    show("wait_bitset.mismatch_past", fx(w, FUTEX_WAIT_BITSET_PRIVATE, 6, &past, 0, ~0));
    show("wait.clock_realtime", fx(w, FUTEX_WAIT_PRIVATE | FUTEX_CLOCK_REALTIME, 5, &ms10, 0, 0));
    show("wait.clock_realtime_bad_timeout", fx(w, FUTEX_WAIT_PRIVATE | FUTEX_CLOCK_REALTIME, 5, KERNEL_ADDR, 0, 0));
    show("wake.clock_realtime", fx(w, FUTEX_WAKE_PRIVATE | FUTEX_CLOCK_REALTIME, 1, 0, 0, 0));
    show("wake_bitset.clock_realtime", fx(w, FUTEX_WAKE_BITSET_PRIVATE | FUTEX_CLOCK_REALTIME, 1, 0, 0, ~0));
    show("wake.timeout_ignored", fx(w, FUTEX_WAKE_PRIVATE, 1, KERNEL_ADDR, 0, 0));

    puts("== wake_op");
    uint32_t *a = &w[2], *b = &w[3];
    *b = 10;
    show("wake_op.set", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, b, FUTEX_OP(FUTEX_OP_SET, 7, FUTEX_OP_CMP_EQ, 10)));
    printf("wake_op.set.value = %u\n", *b);
    show("wake_op.add", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, b, FUTEX_OP(FUTEX_OP_ADD, 0xfff, FUTEX_OP_CMP_LT, 0)));
    printf("wake_op.add.value = %u\n", *b);
    show("wake_op.or_shift", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, b, FUTEX_OP(FUTEX_OP_OR | FUTEX_OP_OPARG_SHIFT, 4, FUTEX_OP_CMP_GE, 6)));
    printf("wake_op.or_shift.value = %u\n", *b);
    show("wake_op.andn", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, b, FUTEX_OP(FUTEX_OP_ANDN, 6, FUTEX_OP_CMP_NE, 0)));
    printf("wake_op.andn.value = %u\n", *b);
    show("wake_op.xor", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, b, FUTEX_OP(FUTEX_OP_XOR, 0x7ff, FUTEX_OP_CMP_GT, -5)));
    printf("wake_op.xor.value = %u\n", *b);
    show("wake_op.shift_out_of_range", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, b, FUTEX_OP(FUTEX_OP_SET | FUTEX_OP_OPARG_SHIFT, 33, FUTEX_OP_CMP_LE, 0)));
    printf("wake_op.shift_out_of_range.value = %u\n", *b);
    *b = 1;
    show("wake_op.bad_op", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, b, (5u << 28) | 3));
    printf("wake_op.bad_op.value = %u\n", *b);
    show("wake_op.bad_cmp", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, b, FUTEX_OP(FUTEX_OP_SET, 9, 7, 0)));
    printf("wake_op.bad_cmp.value = %u\n", *b);
    show("wake_op.misaligned2", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, mis, FUTEX_OP(FUTEX_OP_SET, 9, 0, 0)));
    show("wake_op.null2", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, 0, FUTEX_OP(FUTEX_OP_SET, 9, 0, 0)));
    show("wake_op.null2.bad_op", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, 0, (6u << 28)));
    show("wake_op.kernel2", fx2(a, FUTEX_WAKE_OP_PRIVATE, 1, 1, KERNEL_ADDR, FUTEX_OP(FUTEX_OP_SET, 9, 0, 0)));

    puts("== requeue");
    w[4] = 3;
    show("requeue.nobody", fx2(&w[4], FUTEX_REQUEUE_PRIVATE, 1, 1, &w[5], 0));
    show("requeue.negative_wake", fx2(&w[4], FUTEX_REQUEUE_PRIVATE, -1, 1, &w[5], 0));
    show("requeue.negative_requeue", fx2(&w[4], FUTEX_REQUEUE_PRIVATE, 1, 0xfffffffful, &w[5], 0));
    show("cmp_requeue.mismatch", fx2(&w[4], FUTEX_CMP_REQUEUE_PRIVATE, 1, 1, &w[5], 4));
    show("cmp_requeue.match", fx2(&w[4], FUTEX_CMP_REQUEUE_PRIVATE, 1, 1, &w[5], 3));
    show("cmp_requeue.misaligned1", fx2(mis, FUTEX_CMP_REQUEUE_PRIVATE, 1, 1, &w[5], 3));
    show("cmp_requeue.misaligned2", fx2(&w[4], FUTEX_CMP_REQUEUE_PRIVATE, 1, 1, mis, 3));
    show("cmp_requeue.null1", fx2(0, FUTEX_CMP_REQUEUE_PRIVATE, 1, 1, &w[5], 3));
    show("requeue.null1", fx2(0, FUTEX_REQUEUE_PRIVATE, 1, 1, &w[5], 3));
    show("requeue.null.shared", fx2(0, FUTEX_REQUEUE, 1, 1, &w[5], 3));
}

static void shared_keys(void) {
    puts("== shared keys");
    long pg = 4096;
    uint32_t *ro = mmap(0, pg, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    uint32_t *none = mmap(0, pg, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    show("anon_ro.wait.shared", fx(ro, FUTEX_WAIT, 1, 0, 0, 0));
    show("anon_ro.wait.private", fx(ro, FUTEX_WAIT_PRIVATE, 1, 0, 0, 0));
    show("anon_ro.wake.shared", fx(ro, FUTEX_WAKE, 1, 0, 0, 0));
    show("anon_ro.wake.private", fx(ro, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0));
    show("anon_none.wake.shared", fx(none, FUTEX_WAKE, 1, 0, 0, 0));
    show("anon_none.wake.private", fx(none, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0));
    show("anon_none.wait.private", fx(none, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0));
    uint32_t rw = 0;
    show("wake_op.anon_ro2", fx2(&rw, FUTEX_WAKE_OP_PRIVATE, 1, 1, ro, FUTEX_OP(FUTEX_OP_SET, 1, 0, 0)));
    show("wake_op.anon_ro2.shared", fx2(&rw, FUTEX_WAKE_OP, 1, 1, ro, FUTEX_OP(FUTEX_OP_SET, 1, 0, 0)));
    munmap(ro, pg);
    munmap(none, pg);

    char path[64];
    snprintf(path, sizeof path, "/dev/shm/abi-futex-%d", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
    unlink(path);
    yes("shm.open", fd >= 0);
    yes("shm.truncate", ftruncate(fd, pg) == 0);
    uint32_t *sh = mmap(0, pg, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    uint32_t *shro = mmap(0, pg, PROT_READ, MAP_SHARED, fd, 0);
    uint32_t *prro = mmap(0, pg, PROT_READ, MAP_PRIVATE, fd, 0);
    uint32_t *prrw = mmap(0, pg, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    sh[1] = 7;
    show("file_shared.wait.mismatch", fx(&sh[1], FUTEX_WAIT, 8, 0, 0, 0));
    show("file_shared.wake", fx(&sh[1], FUTEX_WAKE, 1, 0, 0, 0));
    show("file_shared_ro.wait.mismatch", fx(&shro[1], FUTEX_WAIT, 8, 0, 0, 0));
    show("file_shared_ro.wake", fx(&shro[1], FUTEX_WAKE, 1, 0, 0, 0));
    show("file_shared_ro.wake_op2", fx2(&sh[2], FUTEX_WAKE_OP, 1, 1, &shro[1], FUTEX_OP(FUTEX_OP_ADD, 1, 0, 0)));
    printf("file_shared_ro.value = %u\n", sh[1]);
    show("file_shared.wake_op", fx2(&shro[2], FUTEX_WAKE_OP, 1, 1, &sh[1], FUTEX_OP(FUTEX_OP_ADD, 1, 0, 0)));
    printf("file_shared.value = %u\n", shro[1]);
    show("file_private_ro.wait.mismatch", fx(&prro[1], FUTEX_WAIT, 9, 0, 0, 0));
    show("file_private_ro.wake_op2", fx2(&sh[2], FUTEX_WAKE_OP, 1, 1, &prro[1], FUTEX_OP(FUTEX_OP_ADD, 1, 0, 0)));
    show("file_private_rw.wait.mismatch", fx(&prrw[1], FUTEX_WAIT, 1, 0, 0, 0));
    show("file_private_rw.wake_op", fx2(&sh[2], FUTEX_WAKE_OP, 1, 1, &prrw[1], FUTEX_OP(FUTEX_OP_ADD, 1, 0, 0)));
    printf("file_private_rw.value = %u, file = %u\n", prrw[1], sh[1]);
    struct timespec ms5 = {0, 5000000};
    show("file_shared.wait.timeout", fx(&sh[1], FUTEX_WAIT, sh[1], &ms5, 0, 0));
    munmap(sh, pg);
    munmap(shro, pg);
    munmap(prro, pg);
    munmap(prrw, pg);
    close(fd);
}

static void waitv_checks(void) {
    puts("== futex_waitv");
    static _Alignas(8) uint32_t w[4] = {1, 2, 3, 4};
    struct waitv v[3] = {{1, (uintptr_t)&w[0], FUTEX_32 | FUTEX_PRIVATE_FLAG, 0},
                         {2, (uintptr_t)&w[1], FUTEX_32, 0},
                         {3, (uintptr_t)&w[2], FUTEX_32 | FUTEX_PRIVATE_FLAG, 0}};
    struct timespec past = {1, 0};
    show("waitv.flags", waitv(v, 3, 1, 0, CLOCK_MONOTONIC));
    show("waitv.zero", waitv(v, 0, 0, 0, CLOCK_MONOTONIC));
    show("waitv.129", waitv(v, 129, 0, 0, CLOCK_MONOTONIC));
    show("waitv.null", waitv(0, 1, 0, 0, CLOCK_MONOTONIC));
    show("waitv.bad_clock", waitv(v, 3, 0, &past, CLOCK_BOOTTIME));
    show("waitv.bad_timeout", waitv(v, 3, 0, KERNEL_ADDR, CLOCK_MONOTONIC));
    show("waitv.past_mono", waitv(v, 3, 0, &past, CLOCK_MONOTONIC));
    show("waitv.past_real", waitv(v, 3, 0, &past, CLOCK_REALTIME));
    struct timespec soon = at_ns(CLOCK_MONOTONIC, 10000000);
    show("waitv.10ms", waitv(v, 3, 0, &soon, CLOCK_MONOTONIC));
    v[1].val = 9;
    show("waitv.mismatch", waitv(v, 3, 0, 0, CLOCK_BOOTTIME));
    v[1].val = 0x500000002ull;
    show("waitv.val_low_32_bits", waitv(v, 3, 0, &past, CLOCK_MONOTONIC));
    v[1].val = 2;
    v[2].flags = FUTEX_PRIVATE_FLAG;
    show("waitv.no_futex32", waitv(v, 3, 0, &past, CLOCK_MONOTONIC));
    v[2].flags = FUTEX_32 | 4;
    show("waitv.bad_flag", waitv(v, 3, 0, &past, CLOCK_MONOTONIC));
    v[2].flags = FUTEX_32;
    v[2].reserved = 1;
    show("waitv.reserved", waitv(v, 3, 0, &past, CLOCK_MONOTONIC));
    v[2].reserved = 0;
    v[2].uaddr = (uintptr_t)&w[2] + 2;
    show("waitv.misaligned", waitv(v, 3, 0, &past, CLOCK_MONOTONIC));
    v[2].uaddr = 0;
    show("waitv.null_word", waitv(v, 3, 0, &past, CLOCK_MONOTONIC));
    show("waitv.bad_array", waitv(KERNEL_ADDR, 1, 0, 0, CLOCK_MONOTONIC));
}

static void robust_lists(void) {
    puts("== robust list");
    static _Alignas(8) uint64_t head[3];
    uint64_t *hp = 0;
    size_t len = 0;
    SYS("set_robust_list.len23", SYS_set_robust_list, head, 23);
    SYS("set_robust_list", SYS_set_robust_list, head, 24);
    SYS("get_robust_list", SYS_get_robust_list, 0, &hp, &len);
    yes("get_robust_list.same", hp == head && len == 24);
}

static void pi_ops(void) {
    puts("== pi");
    static _Alignas(8) uint32_t w[2];
    SYS("trylock_pi", SYS_futex, &w[0], FUTEX_TRYLOCK_PI_PRIVATE, 0, 0, 0, 0);
    yes("trylock_pi.owner_is_me", w[0] == (uint32_t)syscall(SYS_gettid));
    SYS("unlock_pi", SYS_futex, &w[0], FUTEX_UNLOCK_PI_PRIVATE, 0, 0, 0, 0);
    printf("unlock_pi.value = %u\n", w[0]);
    SYS("lock_pi", SYS_futex, &w[0], FUTEX_LOCK_PI_PRIVATE, 0, 0, 0, 0);
    SYS("unlock_pi.again", SYS_futex, &w[0], FUTEX_UNLOCK_PI_PRIVATE, 0, 0, 0, 0);
    SYS("unlock_pi.not_owner", SYS_futex, &w[0], FUTEX_UNLOCK_PI_PRIVATE, 0, 0, 0, 0);
    SYS("cmp_requeue_pi.same", SYS_futex, &w[0], FUTEX_CMP_REQUEUE_PI_PRIVATE, 1, 0, &w[0], 0);
}

/* SC-7b: PI futexes with helper threads. A helper ends with a raw SYS_exit (no libc cleanup, never joined) when
 * it is meant to die holding a lock. "Blocked" is seen as FUTEX_WAITERS in the word (set under the bucket lock together with
 * the queueing), plus a short sleep; the answers do not depend on the sleep. */
static _Alignas(64) _Atomic uint32_t pw[8];
static long pres[4];
static uint64_t phead[3];

static uint32_t me_tid(void) {
    return (uint32_t)syscall(SYS_gettid);
}
static long pi_call(void *w, long op, const struct timespec *t) {
    long r = syscall(SYS_futex, w, op, 0, t, 0, 0);
    return r == -1 ? -errno : r;
}
static void until_waiters(_Atomic uint32_t *w) {
    struct timespec ms = {0, 1000000};
    for (int i = 0; i < 10000 && !(atomic_load(w) & FUTEX_WAITERS); i++)
        nanosleep(&ms, 0);
    struct timespec settle = {0, 20000000};
    nanosleep(&settle, 0);
}
static void until_set(_Atomic uint32_t *w) {
    struct timespec ms = {0, 1000000};
    for (int i = 0; i < 10000 && !atomic_load(w); i++)
        nanosleep(&ms, 0);
}
static void say(const char *label, long r) {
    if (r < 0)
        printf("%s = -%s\n", label, ename((int)-r));
    else
        printf("%s = %ld\n", label, r);
}

/* Holds pw[0] (trylock), waits until someone waits for it, then unlocks (the hand-over). */
static void *pi_holder(void *arg) {
    long mode = (long)arg;
    pres[0] = pi_call(&pw[0], FUTEX_TRYLOCK_PI_PRIVATE, 0);
    atomic_store(&pw[4], 1);
    if (mode == 1) {
        until_waiters(&pw[0]);
    } else {
        until_set(&pw[5]);
    }
    pres[1] = pi_call(&pw[0], FUTEX_UNLOCK_PI_PRIVATE, 0);
    return 0;
}
/* Takes pw[0] with LOCK_PI, waits for a waiter, and ends holding it (mode 1: listed on its robust list). */
static void *pi_dier(void *arg) {
    long robust = (long)arg;
    if (robust) {
        static uint64_t entry;
        /* The lock word is `pw` + 0; the entry sits ROBUST_OFF bytes below it in memory we own: use the
         * futex_offset to point from `entry` to the word. */
        entry = (uint64_t)phead;
        phead[0] = (uint64_t)&entry | 1;
        phead[1] = (uint64_t)((char *)&pw[0] - (char *)&entry);
        phead[2] = 0;
        syscall(SYS_set_robust_list, phead, 24);
    }
    pres[0] = pi_call(&pw[0], FUTEX_LOCK_PI_PRIVATE, 0);
    atomic_store(&pw[4], 1);
    until_waiters(&pw[0]);
    syscall(SYS_exit, 0);
    return 0;
}
/* Waits for pw[0] with LOCK_PI and unlocks it again. */
static void *pi_waiter(void *arg) {
    (void)arg;
    pres[2] = pi_call(&pw[0], FUTEX_LOCK_PI_PRIVATE, 0);
    pres[3] = (atomic_load(&pw[0]) & FUTEX_TID_MASK) == me_tid();
    pi_call(&pw[0], FUTEX_UNLOCK_PI_PRIVATE, 0);
    return 0;
}
/* Deadlock: holds pw[1], then waits for pw[2] (held by main). */
static void *pi_cross(void *arg) {
    (void)arg;
    pres[0] = pi_call(&pw[1], FUTEX_LOCK_PI_PRIVATE, 0);
    atomic_store(&pw[4], 1);
    pres[1] = pi_call(&pw[2], FUTEX_LOCK_PI_PRIVATE, 0);
    pi_call(&pw[2], FUTEX_UNLOCK_PI_PRIVATE, 0);
    pi_call(&pw[1], FUTEX_UNLOCK_PI_PRIVATE, 0);
    return 0;
}

static void reset(void) {
    for (int i = 0; i < 8; i++)
        atomic_store(&pw[i], 0);
    for (int i = 0; i < 4; i++)
        pres[i] = 1;
}

static void pi_threads(void) {
    puts("== pi threads");
    pthread_t t, t2;
    uint32_t tid = me_tid();

    reset();
    pthread_create(&t, 0, pi_holder, (void *)1);
    until_set(&pw[4]);
    say("handover.lock_pi", pi_call(&pw[0], FUTEX_LOCK_PI_PRIVATE, 0));
    uint32_t v = atomic_load(&pw[0]);
    yes("handover.owner_is_me", (v & FUTEX_TID_MASK) == tid);
    yes("handover.waiters_bit", v & FUTEX_WAITERS);
    pthread_join(t, 0);
    say("handover.holder_trylock", pres[0]);
    say("handover.holder_unlock", pres[1]);
    say("handover.unlock_last", pi_call(&pw[0], FUTEX_UNLOCK_PI_PRIVATE, 0));
    printf("handover.value = %u\n", atomic_load(&pw[0]));

    reset();
    pthread_create(&t, 0, pi_holder, (void *)2);
    until_set(&pw[4]);
    say("trylock.contended", pi_call(&pw[0], FUTEX_TRYLOCK_PI_PRIVATE, 0));
    yes("trylock.contended.waiters_bit", atomic_load(&pw[0]) & FUTEX_WAITERS);
    say("trylock.contended.again", pi_call(&pw[0], FUTEX_TRYLOCK_PI_PRIVATE, 0));
    atomic_store(&pw[5], 1);
    pthread_join(t, 0);
    say("trylock.holder_unlock", pres[1]);
    printf("trylock.value = %u\n", atomic_load(&pw[0]));

    reset();
    pthread_create(&t, 0, pi_holder, (void *)2);
    until_set(&pw[4]);
    pthread_create(&t2, 0, pi_waiter, 0);
    until_waiters(&pw[0]);
    say("mixed.wake", syscall(SYS_futex, &pw[0], FUTEX_WAKE_PRIVATE, 1, 0, 0, 0) == -1 ? -errno : 0);
    say("mixed.cmp_requeue", syscall(SYS_futex, &pw[0], FUTEX_CMP_REQUEUE_PRIVATE, 0, 1, &pw[3], atomic_load(&pw[0])) == -1 ? -errno : 0);
    say("mixed.wake_op", syscall(SYS_futex, &pw[0], FUTEX_WAKE_OP_PRIVATE, 1, 1, &pw[3], FUTEX_OP(FUTEX_OP_SET, 0, FUTEX_OP_CMP_EQ, 0)) == -1 ? -errno : 0);
    say("mixed.wait_on_pi_word", syscall(SYS_futex, &pw[0], FUTEX_WAIT_PRIVATE, 0, 0, 0, 0) == -1 ? -errno : 0);
    say("mixed.unlock_not_owner", pi_call(&pw[0], FUTEX_UNLOCK_PI_PRIVATE, 0));
    atomic_store(&pw[5], 1);
    pthread_join(t, 0);
    pthread_join(t2, 0);
    say("mixed.holder_unlock", pres[1]);
    say("mixed.waiter_lock", pres[2]);
    yes("mixed.waiter_was_owner", pres[3] == 1);
    printf("mixed.value = %u\n", atomic_load(&pw[0]));

    for (long robust = 0; robust < 2; robust++) {
        const char *n = robust ? "died_robust" : "died";
        char l[64];
        reset();
        pthread_create(&t, 0, pi_dier, (void *)robust);
        until_set(&pw[4]);
        snprintf(l, sizeof l, "%s.lock_pi", n);
        say(l, pi_call(&pw[0], FUTEX_LOCK_PI_PRIVATE, 0));
        v = atomic_load(&pw[0]);
        snprintf(l, sizeof l, "%s.owner_is_me", n);
        yes(l, (v & FUTEX_TID_MASK) == tid);
        snprintf(l, sizeof l, "%s.owner_died_bit", n);
        yes(l, v & FUTEX_OWNER_DIED);
        snprintf(l, sizeof l, "%s.waiters_bit", n);
        yes(l, v & FUTEX_WAITERS);
        /* Ended by a raw SYS_exit: not joinable through musl (its exit path never ran). */
        pthread_detach(t);
        snprintf(l, sizeof l, "%s.dier_lock", n);
        say(l, pres[0]);
        snprintf(l, sizeof l, "%s.unlock", n);
        say(l, pi_call(&pw[0], FUTEX_UNLOCK_PI_PRIVATE, 0));
        printf("%s.value = %u\n", n, atomic_load(&pw[0]));
    }

    reset();
    pthread_create(&t, 0, pi_holder, (void *)2);
    until_set(&pw[4]);
    struct timespec rt = at_ns(CLOCK_REALTIME, 30000000), mono = at_ns(CLOCK_MONOTONIC, 30000000);
    say("timeout.lock_pi_realtime", pi_call(&pw[0], FUTEX_LOCK_PI_PRIVATE, &rt));
    yes("timeout.waiters_bit_kept", atomic_load(&pw[0]) & FUTEX_WAITERS);
    say("timeout.lock_pi2_monotonic", pi_call(&pw[0], FUTEX_LOCK_PI2_PRIVATE, &mono));
    rt = at_ns(CLOCK_REALTIME, 30000000);
    say("timeout.lock_pi2_realtime", pi_call(&pw[0], FUTEX_LOCK_PI2_PRIVATE | FUTEX_CLOCK_REALTIME, &rt));
    struct timespec past = {0, 0};
    say("timeout.lock_pi_past", pi_call(&pw[0], FUTEX_LOCK_PI_PRIVATE, &past));
    struct timespec bad = {0, 1000000000};
    say("timeout.lock_pi_bad_ns", pi_call(&pw[0], FUTEX_LOCK_PI_PRIVATE, &bad));
    say("timeout.lock_pi_clock_realtime", pi_call(&pw[0], FUTEX_LOCK_PI_PRIVATE | FUTEX_CLOCK_REALTIME, &rt));
    atomic_store(&pw[5], 1);
    pthread_join(t, 0);
    say("timeout.holder_unlock", pres[1]);
    printf("timeout.value = %u\n", atomic_load(&pw[0]));

    reset();
    atomic_store(&pw[2], tid);
    pthread_create(&t, 0, pi_cross, 0);
    until_set(&pw[4]);
    until_waiters(&pw[2]);
    say("deadlock.lock_pi", pi_call(&pw[1], FUTEX_LOCK_PI_PRIVATE, 0));
    say("deadlock.unlock", pi_call(&pw[2], FUTEX_UNLOCK_PI_PRIVATE, 0));
    pthread_join(t, 0);
    say("deadlock.other_first", pres[0]);
    say("deadlock.other_second", pres[1]);
    printf("deadlock.values = %u %u\n", atomic_load(&pw[1]), atomic_load(&pw[2]));

    reset();
    atomic_store(&pw[0], 0x3ffffff0u);
    say("esrch.lock_pi", pi_call(&pw[0], FUTEX_LOCK_PI_PRIVATE, 0));
    yes("esrch.waiters_set", atomic_load(&pw[0]) == (0x3ffffff0u | FUTEX_WAITERS));
    say("esrch.trylock_pi", pi_call(&pw[0], FUTEX_TRYLOCK_PI_PRIVATE, 0));
    atomic_store(&pw[0], tid);
    say("self.lock_pi", pi_call(&pw[0], FUTEX_LOCK_PI_PRIVATE, 0));
    say("self.trylock_pi", pi_call(&pw[0], FUTEX_TRYLOCK_PI_PRIVATE, 0));
    atomic_store(&pw[0], tid | FUTEX_OWNER_DIED);
    say("owner_died.unlock", pi_call(&pw[0], FUTEX_UNLOCK_PI_PRIVATE, 0));
    printf("owner_died.unlock.value = %u\n", atomic_load(&pw[0]));
    atomic_store(&pw[0], FUTEX_OWNER_DIED);
    say("owner_died.lock_pi", pi_call(&pw[0], FUTEX_LOCK_PI_PRIVATE, 0));
    yes("owner_died.lock_pi.kept", atomic_load(&pw[0]) == (FUTEX_OWNER_DIED | tid));
    atomic_store(&pw[0], FUTEX_WAITERS | FUTEX_OWNER_DIED);
    say("waiters_only.trylock_pi", pi_call(&pw[0], FUTEX_TRYLOCK_PI_PRIVATE, 0));
    yes("waiters_only.value", atomic_load(&pw[0]) == (FUTEX_OWNER_DIED | tid));
    atomic_store(&pw[0], 0);
    say("unlock.free", pi_call(&pw[0], FUTEX_UNLOCK_PI_PRIVATE, 0));
    say("lock_pi.misaligned", pi_call((char *)&pw[0] + 1, FUTEX_LOCK_PI_PRIVATE, 0));
    say("lock_pi.null", pi_call(0, FUTEX_LOCK_PI_PRIVATE, 0));
    say("lock_pi.kernel", pi_call(KERNEL_ADDR, FUTEX_LOCK_PI_PRIVATE, 0));
    say("unlock_pi.null", pi_call(0, FUTEX_UNLOCK_PI_PRIVATE, 0));
    say("trylock_pi.kernel", pi_call(KERNEL_ADDR, FUTEX_TRYLOCK_PI_PRIVATE, 0));
    say("lock_pi.shared", pi_call(&pw[6], FUTEX_LOCK_PI, 0));
    say("unlock_pi.shared", pi_call(&pw[6], FUTEX_UNLOCK_PI, 0));
    say("wait_requeue_pi.same", syscall(SYS_futex, &pw[0], FUTEX_WAIT_REQUEUE_PI_PRIVATE, 0, 0, &pw[0], 0) == -1 ? -errno : 0);
    say("cmp_requeue_pi.nr_wake2", syscall(SYS_futex, &pw[0], FUTEX_CMP_REQUEUE_PI_PRIVATE, 2, 0, &pw[1], 0) == -1 ? -errno : 0);
    say("cmp_requeue_pi.negative", syscall(SYS_futex, &pw[0], FUTEX_CMP_REQUEUE_PI_PRIVATE, 1, -1L, &pw[1], 0) == -1 ? -errno : 0);
}

/* worker: layout of the shared file (64 KiB, created by the first worker). A Drepper "mutex2" (0 free, 1 locked,
 * 2 locked with waiters) with process-shared futex operations (no FUTEX_PRIVATE_FLAG: keyed by the file page),
 * a plain counter changed only under the mutex (a lost update would show a broken mutex) and one progress
 * counter per worker. */
struct shared {
    _Atomic uint32_t mutex;
    uint32_t pad[15];
    volatile uint64_t counter;
    uint64_t pad2[7];
    _Atomic uint64_t progress[];
};
#define SHARED_LEN (1 << 16)

static void lock(_Atomic uint32_t *m) {
    uint32_t c = 0;
    if (atomic_compare_exchange_strong(m, &c, 1))
        return;
    if (c != 2)
        c = atomic_exchange(m, 2);
    while (c != 0) {
        fx(m, FUTEX_WAIT, 2, 0, 0, 0);
        c = atomic_exchange(m, 2);
    }
}

static void unlock(_Atomic uint32_t *m) {
    if (atomic_fetch_sub(m, 1) != 1) {
        atomic_store(m, 0);
        fx(m, FUTEX_WAKE, 1, 0, 0, 0);
    }
}

static struct shared *map_shared(const char *path) {
    int fd = open(path, O_RDWR | O_CREAT, 0600);
    if (fd < 0) {
        printf("open %s\n", ename(errno));
        return 0;
    }
    struct stat st;
    if (fstat(fd, &st) == 0 && st.st_size < SHARED_LEN && ftruncate(fd, SHARED_LEN) != 0) {
        printf("ftruncate %s\n", ename(errno));
        return 0;
    }
    struct shared *s = mmap(0, SHARED_LEN, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (s == MAP_FAILED) {
        printf("mmap %s\n", ename(errno));
        return 0;
    }
    return s;
}

static int worker(const char *path, int idx, long iters) {
    struct shared *s = map_shared(path);
    if (!s || idx < 0 || idx >= 4096)
        return 1;
    volatile uint64_t spin = 0;
    for (long i = 0; i < iters; i++) {
        lock(&s->mutex);
        uint64_t c = s->counter;
        for (int k = 0; k < 50; k++)
            spin++;
        s->counter = c + 1;
        unlock(&s->mutex);
        atomic_fetch_add(&s->progress[idx], 1);
        for (int k = 0; k < 2000; k++)
            spin++;
    }
    printf("worker %d done\n", idx);
    return 0;
}

/* check: the totals after the workers ended, then the file goes. */
static int check(const char *path, int n, long iters) {
    struct shared *s = map_shared(path);
    if (!s || n < 0 || n > 4096)
        return 1;
    int full = 0;
    for (int i = 0; i < n; i++)
        full += s->progress[i] == (uint64_t)iters;
    printf("counter %llu of %llu, workers complete %d of %d, mutex word %u\n", (unsigned long long)s->counter,
           (unsigned long long)n * (unsigned long long)iters, full, n, (unsigned)atomic_load(&s->mutex));
    unlink(path);
    return s->counter == (uint64_t)n * (uint64_t)iters && full == n ? 0 : 1;
}

/* multi: waiters are known to be queued once a FUTEX_REQUEUE (nr_wake 0) has moved all of them from the word
 * they wait on to a gathering word, so no timing decides an answer. */
static _Alignas(64) uint32_t mw[16];
static long mres[8];
struct mwarg {
    int i;
    uint32_t *w;
    uint32_t bitset;
};

static void *mwaiter(void *p) {
    struct mwarg *a = p;
    long r = syscall(SYS_futex, a->w, FUTEX_WAIT_BITSET_PRIVATE, 0, 0, 0, a->bitset);
    mres[a->i] = r == -1 ? -errno : r;
    return 0;
}

static long gather(uint32_t *from, uint32_t *to, int n) {
    long total = 0;
    for (int tries = 0; total < n && tries < 10000; tries++) {
        long r = fx2(from, FUTEX_REQUEUE_PRIVATE, 0, INT_MAX, to, 0);
        if (r > 0)
            total += r;
        if (total < n) {
            struct timespec ms = {0, 1000000};
            nanosleep(&ms, 0);
        }
    }
    return total;
}

/* Starts n waiters (index base..) on w with the given bitsets; 0 when a thread could not be made. */
static int start(pthread_t *t, struct mwarg *a, int base, int n, uint32_t *w, const uint32_t *bitsets) {
    for (int i = 0; i < n; i++) {
        a[base + i] = (struct mwarg){base + i, w, bitsets ? bitsets[i] : FUTEX_BITSET_MATCH_ANY};
        mres[base + i] = 1;
        int e = pthread_create(&t[base + i], 0, mwaiter, &a[base + i]);
        if (e) {
            printf("create %s\n", ename(e));
            return 0;
        }
    }
    return 1;
}

static int joined(pthread_t *t, int base, int n) {
    int ok = 1;
    for (int i = base; i < base + n; i++) {
        pthread_join(t[i], 0);
        ok &= mres[i] == 0;
    }
    return ok;
}

static int multi(void) {
    pthread_t t[8];
    struct mwarg a[8];
    puts("== multi: wake, cmp_requeue, wake_op");
    if (!start(t, a, 0, 6, &mw[0], 0))
        return 0;
    printf("multi.gathered = %ld\n", gather(&mw[0], &mw[1], 6));
    show("multi.wake_2_of_6", fx(&mw[1], FUTEX_WAKE_PRIVATE, 2, 0, 0, 0));
    show("multi.cmp_requeue.mismatch", fx2(&mw[1], FUTEX_CMP_REQUEUE_PRIVATE, 1, 1, &mw[2], 1));
    show("multi.cmp_requeue.wake1_move1", fx2(&mw[1], FUTEX_CMP_REQUEUE_PRIVATE, 1, 1, &mw[2], 0));
    /* Now 2 wait on mw[1], 1 on mw[2]. */
    show("multi.wake_op.cmp_false", fx2(&mw[2], FUTEX_WAKE_OP_PRIVATE, 1, 1, &mw[1], FUTEX_OP(FUTEX_OP_SET, 2, FUTEX_OP_CMP_EQ, 1)));
    printf("multi.wake_op.word = %u\n", mw[1]);
    show("multi.wake_op.cmp_true", fx2(&mw[2], FUTEX_WAKE_OP_PRIVATE, 1, 1, &mw[1], FUTEX_OP(FUTEX_OP_SET, 3, FUTEX_OP_CMP_EQ, 2)));
    printf("multi.wake_op.word = %u\n", mw[1]);
    show("multi.wake.rest", fx(&mw[1], FUTEX_WAKE_PRIVATE, INT_MAX, 0, 0, 0));
    show("multi.wake.none_left", fx(&mw[2], FUTEX_WAKE_PRIVATE, INT_MAX, 0, 0, 0));
    yes("multi.all_returned_0", joined(t, 0, 6));

    puts("== multi: counts of zero");
    if (!start(t, a, 0, 2, &mw[3], 0))
        return 0;
    printf("multi.gathered = %ld\n", gather(&mw[3], &mw[4], 2));
    show("multi.requeue_0_0", fx2(&mw[4], FUTEX_REQUEUE_PRIVATE, 0, 0, &mw[3], 0));
    show("multi.wake_0_wakes_one", fx(&mw[4], FUTEX_WAKE_PRIVATE, 0, 0, 0, 0));
    show("multi.wake.rest", fx(&mw[4], FUTEX_WAKE_PRIVATE, INT_MAX, 0, 0, 0));
    yes("multi.all_returned_0", joined(t, 0, 2));

    puts("== multi: bitsets");
    static const uint32_t bs[3] = {1, 2, 3};
    if (!start(t, a, 0, 3, &mw[5], bs))
        return 0;
    printf("multi.gathered = %ld\n", gather(&mw[5], &mw[6], 3));
    show("multi.wake_bitset.4", fx(&mw[6], FUTEX_WAKE_BITSET_PRIVATE, INT_MAX, 0, 0, 4));
    show("multi.wake_bitset.2", fx(&mw[6], FUTEX_WAKE_BITSET_PRIVATE, INT_MAX, 0, 0, 2));
    show("multi.wake_bitset.1", fx(&mw[6], FUTEX_WAKE_BITSET_PRIVATE, INT_MAX, 0, 0, 1));
    yes("multi.all_returned_0", joined(t, 0, 3));
    puts("== end");
    return 0;
}

/* robust-die / robust-check: the lock word at offset ROBUST_WORD of the shared file, its robust_list entry
 * ROBUST_OFF bytes before it, the list head in the process's own memory. */
#define ROBUST_WORD 4096
#define ROBUST_OFF 64

static int robust_die(const char *path) {
    struct shared *s = map_shared(path);
    if (!s)
        return 1;
    char *base = (char *)s;
    static _Alignas(8) uint64_t head[3];
    uint64_t *entry = (uint64_t *)(base + ROBUST_WORD - ROBUST_OFF);
    _Atomic uint32_t *word = (_Atomic uint32_t *)(base + ROBUST_WORD);
    uint32_t tid = (uint32_t)syscall(SYS_gettid);
    *entry = (uint64_t)head;
    head[0] = (uint64_t)entry;
    head[1] = ROBUST_OFF;
    head[2] = 0;
    atomic_store(word, tid | FUTEX_WAITERS);
    SYS("robust-die.set_robust_list", SYS_set_robust_list, head, 24);
    syscall(SYS_exit_group, 0);
    return 1;
}

static int robust_check(const char *path) {
    struct shared *s = map_shared(path);
    if (!s)
        return 1;
    uint32_t v = atomic_load((_Atomic uint32_t *)((char *)s + ROBUST_WORD));
    printf("robust-check: owner died: %s, waiters kept: %s, tid cleared: %s\n", v & FUTEX_OWNER_DIED ? "yes" : "no",
           v & FUTEX_WAITERS ? "yes" : "no", v & FUTEX_TID_MASK ? "no" : "yes");
    unlink(path);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    if (argc >= 5 && !strcmp(argv[1], "worker"))
        return worker(argv[2], atoi(argv[3]), atol(argv[4]));
    if (argc >= 5 && !strcmp(argv[1], "check"))
        return check(argv[2], atoi(argv[3]), atol(argv[4]));
    if (argc >= 2 && !strcmp(argv[1], "multi"))
        return multi();
    if (argc >= 3 && !strcmp(argv[1], "robust-die"))
        return robust_die(argv[2]);
    if (argc >= 3 && !strcmp(argv[1], "robust-check"))
        return robust_check(argv[2]);
    if (argc >= 2 && !strcmp(argv[1], "pi")) {
        pi_ops();
        pi_threads();
        return 0;
    }
    errors();
    shared_keys();
    waitv_checks();
    robust_lists();
    puts("== end");
    return 0;
}
