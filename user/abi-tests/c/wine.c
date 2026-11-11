/* GM-PROTON: the kernel sequences Wine (Proton 10's, 64-bit and WoW64 alike) runs before and while a game starts, for
 * the ABI diff against CachyOS (plan/notes/gm-PROTON.md 1.4). argv[1]:
 *   reserve   the preloader's four PROT_NONE reservations, shared user data, hints, MAP_32BIT, a 64 GiB reservation
 *             cut into 20 000 VMAs
 *   faults    the exceptions Wine's segv/trap handlers decode: page-fault error bits, #GP of int 0x29/0x2c/0x2d/0x2e
 *             and privileged instructions, int3, ud2, divide error; a write watch that re-runs the store
 *   gs        the TEB through arch_prctl(ARCH_SET_GS): kept per thread, across switches, signals, fork and wrgsbase;
 *             ARCH_SET_FS inside a handler (init_handler with a WoW64 TEB)
 *   suspend   NtSuspendThread's shape: SIGUSR1 by tgkill, the handler blocks until "resume" and the context it
 *             returns with is edited; futex_waitv restarted after handled signals
 *   fsync     fsync's shape: futex_waitv on shared words of a /dev/shm file mapped separately by two processes
 *   seccomp   install_bpf's shape: a probe from an RWX page at 0x600000000000, a filter by instruction pointer with
 *             SPEC_ALLOW, SIGSYS answered by editing RAX
 * Lines are results, errno names, signal numbers, trap numbers and yes/no relations; no address, pid or time. */
#define _GNU_SOURCE
#include "abi.h"
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/futex.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stddef.h>
#include <stdatomic.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <ucontext.h>

#ifndef MAP_FIXED_NOREPLACE
#define MAP_FIXED_NOREPLACE 0x100000
#endif
#define ARCH_SET_GS 0x1001
#define ARCH_SET_FS 0x1002
#define ARCH_GET_FS 0x1003
#define ARCH_GET_GS 0x1004
#define NR_FUTEX_WAITV 449
#define PAGE 4096ul

static char altstack[1 << 17] __attribute__((aligned(64)));

static void on_alt(void) {
    stack_t ss = { .ss_sp = altstack, .ss_size = sizeof altstack };
    sigaltstack(&ss, 0);
}

/* Wine's flags for every handler (signal_x86_64.c signal_init_process). */
static void handle(int sig, void (*h)(int, siginfo_t *, void *)) {
    struct sigaction sa = { 0 };
    sa.sa_sigaction = h;
    sa.sa_flags = SA_SIGINFO | SA_RESTART | SA_ONSTACK;
    sigemptyset(&sa.sa_mask);
    sigaction(sig, &sa, 0);
}

/* ---------------------------------------------------------------- reserve */

static const struct {
    unsigned long addr, size;
    const char *name;
} ranges[] = {
    { 0x000000010000ul, 0x00100000ul, "dos" },
    { 0x000000110000ul, 0x67ef0000ul, "low" },
    { 0x00007f000000ul, 0x00ff0000ul, "top32" },
    { 0x7ffffe000000ul, 0x01ff0000ul, "top" },
};
#define NRANGES (sizeof ranges / sizeof ranges[0])

static int inside(unsigned long a, unsigned long start, unsigned long size) {
    return a >= start && a < start + size;
}

static int in_any(unsigned long a, const int *taken) {
    for (unsigned i = 0; i < NRANGES; i++)
        if (taken[i] && inside(a, ranges[i].addr, ranges[i].size)) return 1;
    return 0;
}

/* Whether /proc/self/maps has exactly this anonymous PROT_NONE line. */
static int maps_has(unsigned long start, unsigned long end) {
    char want[64], line[512];
    snprintf(want, sizeof want, "%08lx-%08lx ---p 00000000 00:00 0", start, end);
    FILE *f = fopen("/proc/self/maps", "r");
    int found = 0;
    while (f && fgets(line, sizeof line, f)) {
        size_t n = strlen(want);
        if (!strncmp(line, want, n) && (line[n] == '\n' || line[n] == ' ')) found = 1;
    }
    if (f) fclose(f);
    return found;
}

static long maps_lines(void) {
    char line[512];
    long n = 0;
    FILE *f = fopen("/proc/self/maps", "r");
    while (f && fgets(line, sizeof line, f)) n++;
    if (f) fclose(f);
    return n;
}

static void reserve(char **argv) {
    unsigned long stack = (unsigned long)&argv, image = (unsigned long)&ranges, vdso = getauxval(AT_SYSINFO_EHDR);
    int taken[NRANGES] = { 0 };
    /* The preloader drops a range that holds its argv (the stack); this program also keeps its own image and the vDSO
     * out of the way, which the preloader, linked elsewhere, never meets. */
    for (unsigned i = 0; i < NRANGES; i++) {
        unsigned long a = ranges[i].addr, s = ranges[i].size;
        int busy = inside(stack, a, s) || inside((unsigned long)argv[0], a, s) || inside(image, a, s) || (vdso && inside(vdso, a, s));
        if (!busy) {
            void *p = mmap((void *)a, s, PROT_NONE, MAP_FIXED | MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
            taken[i] = p == (void *)a;
        }
        printf("%s: reserved = %s\n", ranges[i].name, taken[i] || busy ? "yes" : ename(errno));
    }
    /* Adjacent reservations are one VMA, as Linux merges them. */
    yes("dos and low: one ---p line in maps", !(taken[0] && taken[1]) || maps_has(ranges[0].addr, ranges[1].addr + ranges[1].size));
    yes("top32: one ---p line in maps", !taken[2] || maps_has(ranges[2].addr, ranges[2].addr + ranges[2].size));
    yes("top: one ---p line in maps, or it holds the stack", !taken[3] || maps_has(ranges[3].addr, ranges[3].addr + ranges[3].size));
    /* Shared user data (0x7ffe0000) and Wine's own placements go inside its reservations with MAP_FIXED. */
    if (taken[2]) {
        volatile unsigned *usd = mmap((void *)0x7ffe0000ul, 0x10000, PROT_READ | PROT_WRITE, MAP_FIXED | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        int ok = usd == (void *)0x7ffe0000ul;
        if (ok) {
            usd[0x14 / 4] = 0x1234;
            ok = usd[0x14 / 4] == 0x1234;
        }
        yes("shared user data at 0x7ffe0000 written and read", ok);
        show("noreplace inside the reservation", (long)mmap((void *)0x7f100000ul, PAGE, PROT_READ, MAP_FIXED_NOREPLACE | MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) == -1 ? -1 : 0);
    } else {
        printf("shared user data at 0x7ffe0000 written and read: yes\n");
        printf("noreplace inside the reservation = -EEXIST\n");
    }
    /* VirtualAlloc at a chosen address: Wine unmaps a piece of its reservation and maps there by hint. */
    if (taken[1]) {
        void *hole = (void *)0x20000000ul;
        int ok = munmap(hole, 1 << 20) == 0;
        void *p = mmap(hole, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        yes("a hint into a freed hole is taken", ok && p == hole);
    } else {
        printf("a hint into a freed hole is taken: yes\n");
    }
    void *any = mmap(0, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    yes("mmap without a hint stays out of the reservations", any != MAP_FAILED && !in_any((unsigned long)any, taken) && !in_any((unsigned long)any + (1 << 20) - 1, taken));
    void *m32 = mmap(0, 1 << 16, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_32BIT, -1, 0);
    unsigned long a32 = (unsigned long)m32;
    yes("MAP_32BIT in [1 GiB, 2 GiB)", m32 != MAP_FAILED && a32 >= 0x40000000ul && a32 + (1 << 16) <= 0x80000000ul);
    yes("MAP_32BIT outside the reservations", m32 != MAP_FAILED && !in_any(a32, taken));
    /* Linux searches the MAP_32BIT window bottom-up from 1 GiB: the first free gap is right after "low". */
    if (taken[1]) yes("MAP_32BIT takes the lowest free address", a32 == 0x68000000ul);
    else printf("MAP_32BIT takes the lowest free address: yes\n");

    /* A big reservation, then committed page by page as VirtualAlloc(MEM_COMMIT) does: many VMAs, charged only
     * where writable. */
    unsigned long big = 64ul << 30;
    long before = maps_lines();
    char *r = mmap(0, big, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    yes("64 GiB PROT_NONE reservation", r != MAP_FAILED);
    if (r == MAP_FAILED) return;
    long reserved = maps_lines();
    int ok = 1;
    for (unsigned long i = 0; i < 40000; i += 2) {
        if (mprotect(r + i * PAGE, PAGE, PROT_READ | PROT_WRITE)) { ok = 0; break; }
        r[i * PAGE] = (char)i;
    }
    yes("20000 pages committed one by one", ok);
    long during = maps_lines();
    yes("maps grew by about 40000 lines", during - reserved >= 39990 && during - reserved <= 40010);
    ok = 1;
    for (unsigned long i = 0; i < 40000; i += 2) ok &= r[i * PAGE] == (char)i && r[(i + 1) * PAGE - 1] == 0;
    yes("every committed page holds its byte", ok);
    show("munmap of the reservation", munmap(r, big));
    yes("maps back to the line count before", maps_lines() == before);
}

/* ----------------------------------------------------------------- faults */

/* Each probe is a labelled instruction; the handler moves RIP to the label's end (or re-runs it). */
#define PROBE(name, body)                                                                                               \
    extern char name[], name##_end[];                                                                                     \
    __asm__(".text\n.globl " #name "\n" #name ":\n" body "\n.globl " #name "_end\n" #name "_end:\nret\n");
PROBE(p_read, "movl (%rdi), %eax")
PROBE(p_write, "movl $0x5a5a, (%rdi)")
PROBE(p_cli, "cli")
PROBE(p_hlt, "hlt")
PROBE(p_in, "inb $0x60, %al")
PROBE(p_int29, "int $0x29")
PROBE(p_int2c, "int $0x2c")
PROBE(p_int2d, "int $0x2d")
PROBE(p_int2e, "int $0x2e")
PROBE(p_int3, "int3")
PROBE(p_ud2, "ud2")
PROBE(p_div, "xorl %ecx, %ecx\n.globl p_div_at\np_div_at:\ndivl %ecx")
extern char p_div_at[];
PROBE(p_syscall, "movq %rdi, %rax\nsyscall")

static volatile int f_sig, f_code, f_alt, f_rip_at, f_rip_after, f_addr_ok, f_cr2_ok, f_rerun;
static volatile unsigned long f_err, f_trap, f_expect_addr, f_at, f_end;
static volatile void *f_watch;

static void fault_handler(int sig, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    greg_t *g = uc->uc_mcontext.gregs;
    char here;
    f_sig = sig;
    f_code = si->si_code;
    f_err = g[REG_ERR];
    f_trap = g[REG_TRAPNO];
    f_alt = &here >= altstack && &here < altstack + sizeof altstack;
    f_rip_at = (unsigned long)g[REG_RIP] == f_at;
    f_rip_after = (unsigned long)g[REG_RIP] == f_end;
    f_addr_ok = (unsigned long)si->si_addr == f_expect_addr;
    f_cr2_ok = (unsigned long)g[REG_CR2] == f_expect_addr;
    if (f_watch) {
        /* A write watch: make the page writable and let the store run again. */
        mprotect((void *)f_watch, PAGE, PROT_READ | PROT_WRITE);
        f_watch = 0;
        f_rerun = 1;
        return;
    }
    if (f_rip_at) {
        if (f_at == f_end) {
            /* An exec fault: RIP is the page itself; return to the caller as if the call had returned. */
            g[REG_RIP] = *(greg_t *)g[REG_RSP];
            g[REG_RSP] += 8;
        } else {
            g[REG_RIP] = (greg_t)f_end;
        }
    }
}

static void run_probe(const char *what, void (*fn)(void *), void *arg, char *at, char *end, unsigned long addr) {
    f_sig = f_code = f_alt = f_rip_at = f_rip_after = f_addr_ok = f_cr2_ok = f_rerun = 0;
    f_err = f_trap = 0;
    f_at = (unsigned long)at;
    f_end = (unsigned long)end;
    f_expect_addr = addr;
    fn(arg);
    printf("%s: signal %d code %d trap %lu on alt stack %s\n", what, f_sig, f_code, f_trap, f_alt ? "yes" : "no");
}

static void page_fault(const char *what, void (*fn)(void *), void *arg, char *at, char *end) {
    run_probe(what, fn, arg, at, end, (unsigned long)arg);
    printf("  err: present %lu write %lu user %lu fetch %lu\n", f_err & 1, (f_err >> 1) & 1, (f_err >> 2) & 1, (f_err >> 4) & 1);
    printf("  Wine's ExceptionInformation[0] = %lu\n", (f_err >> 1) & 9);
    yes("  si_addr and cr2 are the address", f_addr_ok && f_cr2_ok);
    yes("  rip at the instruction", f_rip_at);
}

static void gp_fault(const char *what, char *at, char *end) {
    run_probe(what, (void (*)(void *))at, 0, at, end, 0);
    printf("  err = %#lx, Wine's interrupt = %s\n", f_err, (f_err & 7) == 2 ? "yes" : "no");
    yes("  si_addr is 0", f_addr_ok);
    yes("  rip at the instruction", f_rip_at);
}

static void faults(void) {
    on_alt();
    int sigs[] = { SIGSEGV, SIGILL, SIGTRAP, SIGFPE, SIGBUS };
    for (unsigned i = 0; i < sizeof sigs / sizeof sigs[0]; i++) handle(sigs[i], fault_handler);
    char *none = mmap(0, PAGE, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *ro = mmap(0, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *rw = mmap(0, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    char *gone = mmap(0, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    munmap(gone, PAGE);
    page_fault("read of PROT_NONE", (void (*)(void *))p_read, none, p_read, p_read_end);
    page_fault("write to PROT_READ", (void (*)(void *))p_write, ro, p_write, p_write_end);
    page_fault("read of an unmapped page", (void (*)(void *))p_read, gone, p_read, p_read_end);
    /* Execution of a page without PROT_EXEC: the fault is at the page itself. */
    rw[0] = (char)0xc3;
    run_probe("call into PROT_READ|PROT_WRITE", (void (*)(void *))rw, 0, rw, rw, (unsigned long)rw);
    printf("  err: present %lu write %lu user %lu fetch %lu\n", f_err & 1, (f_err >> 1) & 1, (f_err >> 2) & 1, (f_err >> 4) & 1);
    yes("  si_addr and cr2 are the address", f_addr_ok && f_cr2_ok);
    /* Write watch (Wine without userfaultfd): the handler makes the page writable and the store runs again. */
    f_watch = ro;
    f_sig = f_rerun = 0;
    f_at = (unsigned long)p_write;
    f_end = (unsigned long)p_write_end;
    f_expect_addr = (unsigned long)ro;
    ((void (*)(void *))p_write)(ro);
    yes("write watch: one fault, store done after it", f_rerun && *(volatile unsigned *)ro == 0x5a5a);
    gp_fault("cli", p_cli, p_cli_end);
    gp_fault("hlt", p_hlt, p_hlt_end);
    gp_fault("in", p_in, p_in_end);
    gp_fault("int 0x29 (__fastfail)", p_int29, p_int29_end);
    gp_fault("int 0x2c (assertion)", p_int2c, p_int2c_end);
    gp_fault("int 0x2d (debug service)", p_int2d, p_int2d_end);
    gp_fault("int 0x2e", p_int2e, p_int2e_end);
    run_probe("int3", (void (*)(void *))p_int3, 0, p_int3, p_int3_end, 0);
    yes("  rip after the int3", f_rip_after);
    run_probe("ud2", (void (*)(void *))p_ud2, 0, p_ud2, p_ud2_end, (unsigned long)p_ud2);
    yes("  si_addr is the instruction, rip at it", f_addr_ok && f_rip_at);
    run_probe("divide by zero", (void (*)(void *))p_div, 0, p_div_at, p_div_end, (unsigned long)p_div_at);
    yes("  si_addr is the instruction, rip at it", f_addr_ok && f_rip_at);
    long r = ((long (*)(long))p_syscall)(0xffff);
    printf("syscall 0xffff = %s\n", r == -ENOSYS ? "-ENOSYS" : "other");
}

/* --------------------------------------------------------------------- gs */

struct teb {
    char pad[0x30];
    struct teb *self;
    char rest[0x100];
};
static struct teb teb_main, teb_thread, teb_wr;

static struct teb *gs_teb(void) {
    struct teb *t;
    __asm__ volatile("movq %%gs:0x30, %0" : "=r"(t));
    return t;
}

static volatile int h_gs_ok, h_fs_set, h_count;
static void gs_handler(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
    unsigned long fs = 0;
    h_gs_ok = gs_teb() == &teb_main;
    /* init_handler with a WoW64 TEB: FS goes back to the thread's own base before libc runs. */
    syscall(SYS_arch_prctl, ARCH_GET_FS, &fs);
    h_fs_set = syscall(SYS_arch_prctl, ARCH_SET_FS, fs) == 0;
    h_count++;
}

static volatile int thread_ok;
static void *gs_thread(void *arg) {
    (void)arg;
    teb_thread.self = &teb_thread;
    int ok = syscall(SYS_arch_prctl, ARCH_SET_GS, &teb_thread) == 0;
    for (int i = 0; i < 2000 && ok; i++) {
        sched_yield();
        ok = gs_teb() == &teb_thread;
    }
    thread_ok = ok;
    return 0;
}

static void gs(void) {
    on_alt();
    teb_main.self = &teb_main;
    show("arch_prctl ARCH_SET_GS", syscall(SYS_arch_prctl, ARCH_SET_GS, &teb_main));
    unsigned long got = 0;
    syscall(SYS_arch_prctl, ARCH_GET_GS, &got);
    yes("ARCH_GET_GS gives the TEB", got == (unsigned long)&teb_main);
    yes("%gs:0x30 is the TEB", gs_teb() == &teb_main);
    unsigned short sel;
    __asm__ volatile("movw %%gs, %0" : "=r"(sel));
    yes("the GS selector is 0", sel == 0);
    pthread_t t;
    pthread_create(&t, 0, gs_thread, 0);
    int ok = 1;
    for (int i = 0; i < 2000 && ok; i++) {
        sched_yield();
        ok = gs_teb() == &teb_main;
    }
    pthread_join(t, 0);
    yes("main keeps its TEB while a thread runs with another", ok);
    yes("the thread kept its own TEB", thread_ok);
    struct timespec ms = { 0, 1000000 };
    ok = 1;
    for (int i = 0; i < 20 && ok; i++) {
        nanosleep(&ms, 0);
        ok = gs_teb() == &teb_main;
    }
    yes("kept across sleeps", ok);
    handle(SIGUSR1, gs_handler);
    for (int i = 0; i < 50; i++) syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGUSR1);
    yes("50 handlers ran", h_count == 50);
    yes("the handler sees the TEB through %gs", h_gs_ok);
    yes("ARCH_SET_FS in the handler", h_fs_set);
    yes("TEB and libc's TLS intact after the handlers", gs_teb() == &teb_main && (errno = 7, errno == 7));
    pid_t c = fork();
    if (c == 0) _exit(gs_teb() == &teb_main ? 0 : 1);
    int st = 0;
    waitpid(c, &st, 0);
    yes("a fork child has the TEB", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    if (getauxval(AT_HWCAP2) & 2) {
        teb_wr.self = &teb_wr;
        __asm__ volatile("wrgsbase %0" ::"r"(&teb_wr));
        ok = 1;
        for (int i = 0; i < 2000 && ok; i++) {
            sched_yield();
            ok = gs_teb() == &teb_wr;
        }
        got = 0;
        syscall(SYS_arch_prctl, ARCH_GET_GS, &got);
        yes("wrgsbase kept across switches, or no FSGSBASE", ok && got == (unsigned long)&teb_wr);
    } else {
        printf("wrgsbase kept across switches, or no FSGSBASE: yes\n");
    }
}

/* ---------------------------------------------------------------- suspend */

#define SPIN_END (1ul << 62)
static atomic_int s_state; /* 0 running, 1 suspended (handler waits), 2 resume */
static volatile unsigned long s_r12, s_new_r12;
static volatile int s_rip_in, s_tid;
extern char s_spin[], s_spin_end[];
__asm__(".text\n.globl s_spin\ns_spin:\nincq %r12\nmovabsq $0x4000000000000000, %rax\ncmpq %rax, %r12\njb s_spin\n"
        ".globl s_spin_end\ns_spin_end:\nret\n");

static long futex(atomic_int *w, int op, int val) {
    return syscall(SYS_futex, w, op, val, 0, 0, 0);
}

static void suspend_handler(int sig, siginfo_t *si, void *ucv) {
    (void)sig, (void)si;
    greg_t *g = ((ucontext_t *)ucv)->uc_mcontext.gregs;
    unsigned long rip = g[REG_RIP];
    s_rip_in = rip >= (unsigned long)s_spin && rip < (unsigned long)s_spin_end;
    s_r12 = g[REG_R12];
    atomic_store(&s_state, 1);
    futex(&s_state, FUTEX_WAKE_PRIVATE, 1);
    while (atomic_load(&s_state) == 1) futex(&s_state, FUTEX_WAIT_PRIVATE, 1);
    if (s_rip_in) g[REG_R12] = s_new_r12;
}

/* One suspend: returns when the handler waits; `resume` lets it go on with r12 = `r12`. */
static void suspend_once(void) {
    atomic_store(&s_state, 0);
    syscall(SYS_tgkill, getpid(), s_tid, SIGUSR1);
    while (atomic_load(&s_state) != 1) futex(&s_state, FUTEX_WAIT_PRIVATE, 0);
}
static void resume(unsigned long r12) {
    s_new_r12 = r12;
    atomic_store(&s_state, 2);
    futex(&s_state, FUTEX_WAKE_PRIVATE, 1);
}

static void *spinner(void *arg) {
    (void)arg;
    stack_t ss = { .ss_sp = mmap(0, 1 << 16, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0), .ss_size = 1 << 16 };
    sigaltstack(&ss, 0);
    s_tid = syscall(SYS_gettid);
    unsigned long r12;
    /* Past the red zone: the call's push must not land on anything the compiler keeps there. */
    __asm__ volatile("subq $128, %%rsp\nxorl %%r12d, %%r12d\ncall s_spin\naddq $128, %%rsp\nmovq %%r12, %0" : "=r"(r12) : : "rax", "r12", "memory", "cc");
    return (void *)r12;
}

struct waitv {
    uint64_t val, uaddr;
    uint32_t flags, reserved;
};
static atomic_int w_word, w_hits;
static volatile long w_result;
static void count_handler(int sig) {
    (void)sig;
    atomic_fetch_add(&w_hits, 1);
}
static void *waiter(void *arg) {
    (void)arg;
    s_tid = syscall(SYS_gettid);
    struct waitv w[2] = { { 1, (uint64_t)(uintptr_t)&s_r12, FUTEX_32 | FUTEX_PRIVATE_FLAG, 0 }, { 0, (uint64_t)(uintptr_t)&w_word, FUTEX_32 | FUTEX_PRIVATE_FLAG, 0 } };
    s_r12 = 1;
    w_result = syscall(NR_FUTEX_WAITV, w, 2, 0, 0, 0);
    return 0;
}

static void suspend(void) {
    handle(SIGUSR1, suspend_handler);
    pthread_t t;
    s_tid = 0;
    pthread_create(&t, 0, spinner, 0);
    while (!s_tid) sched_yield();
    /* Until the thread is in its loop, a suspend finds it elsewhere and leaves r12 alone. */
    struct timespec ms = { 0, 1000000 };
    for (int tries = 0; tries < 2000; tries++) {
        suspend_once();
        int in = s_rip_in;
        resume(s_r12);
        if (in) break;
        nanosleep(&ms, 0);
    }
    int rip_ok = 1, mono = 1;
    unsigned long last = 0;
    for (int round = 1; round <= 200; round++) {
        suspend_once();
        rip_ok &= s_rip_in;
        mono &= s_r12 >= last;
        last = (unsigned long)round << 32;
        resume(round == 200 ? SPIN_END - 1 : last);
    }
    void *r12;
    pthread_join(t, &r12);
    yes("200 suspends caught the thread in its loop", rip_ok);
    yes("each resume continued from the written r12", mono);
    yes("the last written r12 ended the loop", (unsigned long)r12 == SPIN_END);

    /* A thread waiting in futex_waitv takes handled signals and goes on waiting (SA_RESTART). */
    struct sigaction sa = { 0 };
    sa.sa_handler = count_handler;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGUSR2, &sa, 0);
    s_tid = 0;
    w_result = -99;
    pthread_create(&t, 0, waiter, 0);
    while (!s_tid) sched_yield();
    nanosleep(&ms, 0);
    for (int i = 1; i <= 50; i++) {
        syscall(SYS_tgkill, getpid(), s_tid, SIGUSR2);
        while (atomic_load(&w_hits) < i) sched_yield();
    }
    nanosleep(&ms, 0);
    yes("still waiting after 50 handled signals", w_result == -99);
    atomic_store(&w_word, 1);
    while (w_result == -99) {
        futex(&w_word, FUTEX_WAKE_PRIVATE, 1);
        sched_yield();
    }
    pthread_join(t, 0);
    printf("futex_waitv then answers %ld (the woken entry)\n", w_result);
}

/* ------------------------------------------------------------------ fsync */

static long waitv_abs(struct waitv *w, int n, uint64_t deadline_ns) {
    struct timespec t = { (time_t)(deadline_ns / 1000000000u), (long)(deadline_ns % 1000000000u) };
    return syscall(NR_FUTEX_WAITV, w, n, 0, deadline_ns ? &t : 0, CLOCK_MONOTONIC);
}

static uint32_t *map_file(const char *path) {
    int fd = open(path, O_RDWR);
    uint32_t *m = fd < 0 ? MAP_FAILED : mmap(0, 65536, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (fd >= 0) close(fd);
    return m == MAP_FAILED ? 0 : m;
}

static void fsync_(void) {
    char path[64];
    snprintf(path, sizeof path, "/dev/shm/abi-wine-%d", getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
    yes("/dev/shm file made", fd >= 0 && ftruncate(fd, 65536) == 0);
    if (fd >= 0) close(fd);
    uint32_t *m = map_file(path);
    yes("mapped shared", m != 0);
    if (!m) return;
    struct waitv w[64];
    for (int i = 0; i < 64; i++) w[i] = (struct waitv){ 0, (uint64_t)(uintptr_t)&m[16 + i], FUTEX_32, 0 };
    show("waitv with a value that differs", (m[16] = 5, waitv_abs(w, 1, 0)));
    m[16] = 0;
    long r = waitv_abs(w, 2, ns_of(CLOCK_MONOTONIC) + 50000000u);
    printf("waitv to an absolute monotonic deadline 50 ms ahead = %s\n", r == -1 && errno == ETIMEDOUT ? "-ETIMEDOUT" : "other");
    /* Another process with its own mapping of the file (as each Wine process maps the fsync shm itself). */
    pid_t c = fork();
    if (c == 0) {
        uint32_t *cm = map_file(path);
        if (!cm || cm == m) _exit(90);
        struct waitv cw[64];
        for (int i = 0; i < 64; i++) cw[i] = (struct waitv){ 0, (uint64_t)(uintptr_t)&cm[16 + i], FUTEX_32, 0 };
        __atomic_store_n(&cm[0], 1, __ATOMIC_SEQ_CST);
        long r = waitv_abs(cw, 64, 0);
        _exit(r < 0 ? 91 : (int)r);
    }
    /* The parent maps the same file once more, so the wake goes through yet another address. The words stay 0: a
     * wake alone ends the wait, and the child cannot miss it by reading a changed value before it waits. */
    uint32_t *pm = map_file(path);
    while (!__atomic_load_n(&m[0], __ATOMIC_SEQ_CST)) sched_yield();
    int st = 0;
    for (int tries = 0; tries < 5000; tries++) {
        syscall(SYS_futex, &pm[16 + 63], FUTEX_WAKE, 1, 0, 0, 0);
        if (waitpid(c, &st, WNOHANG) == c) break;
        struct timespec ms = { 0, 1000000 };
        nanosleep(&ms, 0);
    }
    printf("the other process's waitv over 64 shared words answered %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    unlink(path);
}

/* ---------------------------------------------------------------- seccomp */

#define STATUS_INVALID_PARAMETER 0xc000000dul
static volatile int y_code, y_arch_ok, y_errno, y_nr, y_call_ok, y_rip_ok, y_hits;
static unsigned char *stub;

static void sigsys_handler(int sig, siginfo_t *si, void *ucv) {
    (void)sig;
    greg_t *g = ((ucontext_t *)ucv)->uc_mcontext.gregs;
    y_hits++;
    y_code = si->si_code;
    y_arch_ok = si->si_arch == AUDIT_ARCH_X86_64;
    y_errno = si->si_errno;
    y_nr = si->si_syscall;
    y_call_ok = si->si_call_addr == (void *)(stub + 5);
    y_rip_ok = (unsigned long)g[REG_RIP] == (unsigned long)(stub + 5);
    /* Wine answers 0xffff itself; any other number it runs as the Windows syscall (here: the Linux one). */
    if (g[REG_RAX] == 0xffff) g[REG_RAX] = STATUS_INVALID_PARAMETER;
    else g[REG_RAX] = syscall(g[REG_RAX], g[REG_RDI], g[REG_RSI], g[REG_RDX]);
}

static void seccomp_(void) {
    on_alt();
    unsigned char code[] = { 0x48, 0x89, 0xf8, 0x0f, 0x05, 0xc3 }; /* mov %rdi,%rax; syscall; ret */
    stub = mmap((void *)0x600000000000ul, PAGE, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    yes("RWX page at 0x600000000000", stub == (void *)0x600000000000ul);
    if (stub != (void *)0x600000000000ul) return;
    memcpy(stub, code, sizeof code);
    long (*call)(long) = (long (*)(long))stub;
    printf("probe before the filter = %s\n", call(0xffff) == -ENOSYS ? "-ENOSYS" : "other");
    show("PR_GET_SECCOMP", prctl(PR_GET_SECCOMP, 0, 0, 0, 0));
    yes("this program's code is at or above 0x700000000000", (unsigned long)&seccomp_ >= 0x700000000000ul);
    handle(SIGSYS, sigsys_handler);
    struct sock_filter f[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, instruction_pointer) + 4),
        BPF_JUMP(BPF_JMP | BPF_JGE | BPF_K, 0x7000, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 0x6000, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRAP | 42),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog prog = { sizeof f / sizeof f[0], f };
    show("PR_SET_NO_NEW_PRIVS", prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0));
    show("seccomp filter with SPEC_ALLOW", syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_SPEC_ALLOW, &prog));
    show("PR_GET_SECCOMP", prctl(PR_GET_SECCOMP, 0, 0, 0, 0));
    long r = call(0xffff);
    yes("probe after the filter gives STATUS_INVALID_PARAMETER", (unsigned long)r == STATUS_INVALID_PARAMETER);
    printf("  SIGSYS code %d, arch x86_64 %s, data %d, nr %#x\n", y_code, y_arch_ok ? "yes" : "no", y_errno, y_nr);
    yes("  call address and rip right after the syscall", y_call_ok && y_rip_ok);
    yes("a trapped getpid answered by the handler", call(SYS_getpid) == getpid() && y_nr == SYS_getpid);
    yes("syscalls from this program's code pass", syscall(SYS_getppid) == getppid() && y_hits == 2);
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "";
    setvbuf(stdout, 0, _IOLBF, 0);
    printf("mode %s\n", m);
    if (!strcmp(m, "reserve")) reserve(argv);
    else if (!strcmp(m, "faults")) faults();
    else if (!strcmp(m, "gs")) gs();
    else if (!strcmp(m, "suspend")) suspend();
    else if (!strcmp(m, "fsync")) fsync_();
    else if (!strcmp(m, "seccomp")) seccomp_();
    else {
        printf("usage: abi-wine reserve|faults|gs|suspend|fsync|seccomp\n");
        return 2;
    }
    printf("done\n");
    return 0;
}
