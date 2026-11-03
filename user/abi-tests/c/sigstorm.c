/* AB-16: signal delivery under load. Nested SA_SIGINFO handlers on an alternate stack, rt_sigreturn with
 * hostile frames (bad CS/SS, non-canonical or kernel rip, bad rsp, a bad fpstate, no frame at all) and the
 * program's own iretq with bad selectors; every hostile step must end in a signal to the program, never in a
 * kernel fault. `storm` loops all of it with an interval timer firing (the ktest `ab.signal.nmi_storm` runs it
 * on every CPU while it takes NMI backtraces; SIGSTORM_CPU=<n> pins the program to CPU n). argv[1]: nested | hostile | direct | iret | storm [rounds].
 * Lines print signals, si_codes, trap numbers and relations only: no addresses, selectors or counts that
 * depend on timing. */
#include "abi.h"
#include <setjmp.h>
#include <signal.h>
#include <sys/auxv.h>
#include <sys/time.h>
#include <ucontext.h>

#define NONCANON 0x0000800000000000ul
#define NONCANON_HIGH 0xdead000000000000ul
#define KERNEL_TEXT 0xffffffff81000000ul
#define UNMAPPED 0x10000ul
#define UC_STRICT_SS 0x4ul
#define NEST 24

static char alt[1 << 18] __attribute__((aligned(64)));
static unsigned char zero_frame[8192] __attribute__((aligned(64)));
static sigjmp_buf back;
static unsigned short own_cs, own_ss;
static volatile int quiet, mismatches;
static volatile unsigned long alarms;

/* What the last step ended in: the signal (0 = it returned), si_code, the trap number (-1 when Linux leaves
 * a stale one), whether si_addr was the planted address, whether the handler ran on the alternate stack. */
struct result {
    int sig, code, trap, addr_ok, on_alt, refaults;
};
/* A user access to the kernel half is a #GP (#SS for the stack) on a CPU with LASS and a #PF without: for
 * those steps only "a fault signal" is printed. */
static volatile int cpu_dependent;
static volatile struct result got;
static volatile int want_trap;
static volatile unsigned long planted;
/* A handler that returns through the hostile registers the kernel saved in its frame: the return faults
 * again, `returns_left` times, each time with the same register still in the new frame. */
static volatile int returns_left, saved_reg;
static volatile unsigned long saved_val;

static int on_alt(const void *p) {
    unsigned long a = (unsigned long)p;
    return a > (unsigned long)alt && a <= (unsigned long)alt + sizeof alt;
}

static void set(int sig, void *h, unsigned long flags, unsigned long mask) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = h;
    sa.sa_flags = flags | SA_SIGINFO;
    memcpy(&sa.sa_mask, &mask, sizeof mask);
    sigaction(sig, &sa, 0);
}

static unsigned long bit(int sig) {
    return 1ul << (sig - 1);
}

static void send(int sig) {
    syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), sig);
}

static const char *signame(int s) {
    switch (s) {
    case 0: return "returned";
    case SIGSEGV: return "SIGSEGV";
    case SIGBUS: return "SIGBUS";
    case SIGILL: return "SIGILL";
    case SIGTRAP: return "SIGTRAP";
    }
    return "other";
}

static const char *codename(int c) {
    switch (c) {
    case SI_KERNEL: return "SI_KERNEL";
    case SEGV_MAPERR: return "SEGV_MAPERR";
    case SEGV_ACCERR: return "SEGV_ACCERR";
    }
    return "other";
}

static void fault_handler(int sig, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    char here = 0;
    got.sig = sig;
    got.code = si->si_code;
    got.trap = want_trap ? (int)uc->uc_mcontext.gregs[REG_TRAPNO] : -1;
    got.addr_ok = (unsigned long)si->si_addr == planted;
    got.on_alt = on_alt(&here);
    if (returns_left > 0) {
        returns_left--;
        if ((unsigned long)uc->uc_mcontext.gregs[saved_reg] == saved_val)
            got.refaults++;
        return;
    }
    siglongjmp(back, 1);
}

static void alarm_handler(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)si, (void)uc;
    alarms++;
}

/* Prints the step's line, or in a storm compares it with the first round's. */
static struct result first[64], bad_was, bad_now;
static int step, round_no;
/* A copy: callers pass their own stack buffers for composed names. */
static char first_bad_name[64];
static const char *first_bad;

static void report(const char *what) {
    struct result r = got;
    if (quiet) {
        /* A CPU-dependent step is compared as it is printed (a fault signal, the stack): on V2 its exact fault
         * also depends on timing (an interrupt or timer signal right after the program's own iretq to a
         * kernel-half rip makes V2's return refuse that rip with #GP where the fetch alone gives #PF). */
        if (cpu_dependent) {
            r.sig = r.sig == SIGSEGV || r.sig == SIGBUS ? SIGSEGV : r.sig;
            r.code = r.trap = r.addr_ok = 0;
        }
        if (round_no == 0)
            first[step] = r;
        else if (memcmp(&first[step], &r, sizeof r) != 0 && !mismatches++) {
            snprintf(first_bad_name, sizeof first_bad_name, "%s", what);
            first_bad = first_bad_name;
            bad_was = first[step];
            bad_now = r;
        }
        step++;
        return;
    }
    if (!r.sig) {
        printf("%s: returned\n", what);
        return;
    }
    if (cpu_dependent) {
        printf("%s: fault signal %s on-alt-stack %s\n", what, r.sig == SIGSEGV || r.sig == SIGBUS ? "yes" : "no", r.on_alt ? "yes" : "no");
        return;
    }
    printf("%s: %s %s", what, signame(r.sig), codename(r.code));
    if (r.trap >= 0)
        printf(" trap %d", r.trap);
    printf(" addr-is-planted %s on-alt-stack %s", r.addr_ok ? "yes" : "no", r.on_alt ? "yes" : "no");
    if (r.refaults)
        printf(" refaults with the register kept %d", r.refaults);
    printf("\n");
}

static void clear(int trap, unsigned long addr) {
    memset((void *)&got, 0, sizeof got);
    cpu_dependent = addr == KERNEL_TEXT || addr == (unsigned long)KERNEL_ADDR;
    want_trap = trap;
    planted = addr;
}

/* ---- nested deliveries ---------------------------------------------------------------------------- */

static volatile int depth, maxdepth, bad_nest, bad_depth;

static void nest_handler(int sig, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    volatile unsigned long canary = 0x5a5a0000ul + (unsigned long)depth;
    int d = ++depth;
    sigset_t now;
    sigprocmask(SIG_BLOCK, 0, &now);
    if (!on_alt((const void *)&canary))
        bad_nest |= 1;
    if (si->si_code != SI_TKILL || si->si_pid != getpid() || si->si_signo != sig)
        bad_nest |= 2;
    /* uc_stack holds the stored settings (flags 0, `__save_altstack`), sigaltstack says SS_ONSTACK. */
    stack_t q;
    sigaltstack(0, &q);
    if (uc->uc_stack.ss_flags != 0 || uc->uc_stack.ss_sp != alt || uc->uc_stack.ss_size != sizeof alt || q.ss_flags != SS_ONSTACK)
        bad_nest |= 4;
    /* SIGUSR2's sa_mask holds SIGRTMIN+2; SA_NODEFER leaves the signal itself unblocked. */
    if (sigismember(&now, SIGRTMIN + 2) != (sig == SIGUSR2 || d > 1) || sigismember(&now, sig))
        bad_nest |= 8;
    if (d > maxdepth)
        maxdepth = d;
    if (bad_nest && !bad_depth)
        bad_depth = d;
    if (d < NEST)
        send(sig == SIGUSR1 ? SIGUSR2 : SIGUSR1);
    if (canary != 0x5a5a0000ul + (unsigned long)(d - 1))
        bad_nest |= 16;
    depth--;
}

static void nested(void) {
    set(SIGUSR1, nest_handler, SA_ONSTACK | SA_NODEFER, 0);
    set(SIGUSR2, nest_handler, SA_ONSTACK | SA_NODEFER, bit(SIGRTMIN + 2));
    depth = maxdepth = bad_nest = bad_depth = 0;
    sigset_t before, after;
    /* The kernel writes 8 of musl's 128 bytes. */
    memset(&before, 0, sizeof before);
    memset(&after, 0, sizeof after);
    sigprocmask(SIG_BLOCK, 0, &before);
    send(SIGUSR1);
    sigprocmask(SIG_BLOCK, 0, &after);
    int mask_back = !memcmp(&before, &after, sizeof before);
    if (quiet) {
        if ((maxdepth != NEST || depth || bad_nest || !mask_back) && !mismatches++) {
            first_bad = "nested";
            bad_now.sig = maxdepth, bad_now.code = bad_nest, bad_now.trap = depth, bad_now.addr_ok = mask_back;
        }
        return;
    }
    printf("nested: depth %d reached %s, unwound %s, mask back %s\n", NEST, maxdepth == NEST ? "yes" : "no",
           depth == 0 ? "yes" : "no", mask_back ? "yes" : "no");
    if (bad_nest)
        printf("nested: checks failed (%d, first at depth %d)\n", bad_nest, bad_depth);
    else
        printf("nested: checks ok\n");
}

/* ---- hostile frames handed back from a handler ----------------------------------------------------- */

enum edit {
    CS_DATA, CS_NULL, CS_LDT, CS_BEYOND, CS_RPL0,
    SS_NULL, SS_CODE, SS_LDT, SS_BEYOND, SS_NULL_LAX, SS_CODE_LAX,
    RIP_NONCANON, RIP_NONCANON_HIGH, RIP_KERNEL,
    RSP_NONCANON, RSP_KERNEL, RSP_UNMAPPED,
    FP_KERNEL, FP_UNMAPPED, EFL_IOPL,
    EDITS
};

static const char *edit_names[EDITS] = {
    "cs data selector", "cs null", "cs ldt selector", "cs beyond the gdt", "cs rpl 0",
    "ss null (strict)", "ss code selector (strict)", "ss ldt selector (strict)", "ss beyond the gdt (strict)",
    "ss null (not strict)", "ss code selector (not strict)",
    "rip non-canonical", "rip non-canonical high", "rip kernel half",
    "rsp non-canonical", "rsp kernel half", "rsp unmapped",
    "fpstate kernel half", "fpstate unmapped", "rflags iopl 3",
};

static volatile int cur;

static void put_cs(greg_t *g, unsigned v) {
    g[REG_CSGSFS] = (g[REG_CSGSFS] & ~0xfffful) | v;
}

static void put_ss(greg_t *g, unsigned long v) {
    g[REG_CSGSFS] = (g[REG_CSGSFS] & 0x0000fffffffffffful) | (v << 48);
}

static void edit_handler(int sig, siginfo_t *si, void *ucv) {
    (void)sig, (void)si;
    ucontext_t *uc = ucv;
    greg_t *g = uc->uc_mcontext.gregs;
    switch (cur) {
    case CS_DATA: put_cs(g, own_ss); break;
    case CS_NULL: put_cs(g, 0); break;
    case CS_LDT: put_cs(g, 0x0c); break;
    case CS_BEYOND: put_cs(g, 0xfff8); break;
    case CS_RPL0: put_cs(g, own_cs & ~3u); break;
    case SS_NULL: put_ss(g, 0); break;
    case SS_CODE: put_ss(g, own_cs); break;
    case SS_LDT: put_ss(g, 0x0c); break;
    case SS_BEYOND: put_ss(g, 0xfff8); break;
    case SS_NULL_LAX: put_ss(g, 0); uc->uc_flags &= ~UC_STRICT_SS; break;
    case SS_CODE_LAX: put_ss(g, own_cs); uc->uc_flags &= ~UC_STRICT_SS; break;
    case RIP_NONCANON: g[REG_RIP] = NONCANON; break;
    case RIP_NONCANON_HIGH: g[REG_RIP] = NONCANON_HIGH; break;
    case RIP_KERNEL: g[REG_RIP] = KERNEL_TEXT; break;
    /* Deep in the hole: 0x800000000000 itself would make the next push a #PF at a canonical address. */
    case RSP_NONCANON: g[REG_RSP] = NONCANON_HIGH; break;
    case RSP_KERNEL: g[REG_RSP] = (greg_t)KERNEL_ADDR; break;
    case RSP_UNMAPPED: g[REG_RSP] = UNMAPPED; break;
    case FP_KERNEL: uc->uc_mcontext.fpregs = (void *)KERNEL_ADDR; break;
    case FP_UNMAPPED: uc->uc_mcontext.fpregs = (void *)UNMAPPED; break;
    case EFL_IOPL: g[REG_EFL] |= 0x3000; break;
    }
}

static unsigned long rflags_now(void) {
    unsigned long f;
    __asm__ volatile("pushfq; pop %0" : "=r"(f));
    return f;
}

static unsigned short ss_now(void) {
    unsigned short s;
    __asm__ volatile("mov %%ss, %0" : "=r"(s));
    return s;
}

static void hostile(void) {
    set(SIGUSR1, edit_handler, SA_ONSTACK, 0);
    for (int e = 0; e < EDITS; e++) {
        /* A fault from a frame the kernel rejects itself leaves Linux's trap number stale: not compared. */
        int trap = e != FP_KERNEL && e != FP_UNMAPPED;
        unsigned long addr = e == RIP_KERNEL ? KERNEL_TEXT : e == RSP_KERNEL ? (unsigned long)KERNEL_ADDR : 0;
        clear(trap, addr);
        cur = e;
        if (sigsetjmp(back, 1) == 0) {
            send(SIGUSR1);
            /* Returned: the repaired state must be the old one. */
            if (!quiet && (e == SS_NULL_LAX || e == SS_CODE_LAX))
                printf("%s: ss after return is the old one %s\n", edit_names[e], ss_now() == own_ss ? "yes" : "no");
            if (!quiet && e == EFL_IOPL)
                printf("%s: iopl after return %lu\n", edit_names[e], (rflags_now() >> 12) & 3);
        }
        report(edit_names[e]);
        if (e != RIP_NONCANON && e != RIP_NONCANON_HIGH && e != RSP_NONCANON)
            continue;
        /* Again, with the fault handler returning twice through what the kernel saved (rip or rsp). */
        clear(1, 0);
        returns_left = 2;
        saved_reg = e == RSP_NONCANON ? REG_RSP : REG_RIP;
        saved_val = e == RIP_NONCANON ? NONCANON : NONCANON_HIGH;
        if (sigsetjmp(back, 1) == 0)
            send(SIGUSR1);
        returns_left = 0;
        char what[64];
        snprintf(what, sizeof what, "%s, handler returns through it", edit_names[e]);
        report(what);
    }
}

/* ---- rt_sigreturn with no usable frame ----------------------------------------------------------- */

static __attribute__((noinline, noreturn)) void sigreturn_at(unsigned long sp) {
    __asm__ volatile("mov %0, %%rsp\n\tmov $15, %%eax\n\tsyscall\n\tud2" : : "r"(sp) : "memory");
    __builtin_unreachable();
}

static void direct(void) {
    static const char *names[] = { "no frame (unmapped)", "frame in the kernel half", "frame at a non-canonical rsp",
                                   "zeroed frame", "random frame with an ldt cs" };
    static const unsigned long sps[] = { UNMAPPED + 8, (unsigned long)KERNEL_ADDR, NONCANON };
    for (int i = 0; i < 5; i++) {
        /* Linux's trap number is meaningful only where the return itself faulted (cs 0 or an ldt cs). */
        clear(i >= 3, 0);
        unsigned long sp;
        if (i < 3) {
            sp = sps[i];
        } else {
            memset(zero_frame, 0, sizeof zero_frame);
            if (i == 4) {
                /* The general registers (uc_mcontext.gregs at frame + 48, frame = sp - 8) get noise;
                 * rflags stays 0x202 (a random AC or TF would change what the handler runs into), cs is an
                 * ldt selector (no LDT exists: the return must fault), the mask, fpstate and uc_flags are 0. */
                unsigned long x = 0x9e3779b97f4a7c15ul, *g = (unsigned long *)(zero_frame + 48);
                for (int r = 0; r < 23; r++) {
                    x ^= x << 13, x ^= x >> 7, x ^= x << 17;
                    g[r] = x;
                }
                g[REG_EFL] = 0x202;
                g[REG_CSGSFS] = 0x0c | 0x0c00000000000000ul;
                g[REG_ERR] = g[REG_TRAPNO] = g[REG_OLDMASK] = g[REG_CR2] = 0;
                memset(zero_frame + 48 + 23 * 8, 0, sizeof zero_frame - 48 - 23 * 8);
            }
            sp = (unsigned long)zero_frame + 8;
        }
        if (sigsetjmp(back, 1) == 0)
            sigreturn_at(sp);
        report(names[i]);
    }
}

/* ---- the program's own iretq --------------------------------------------------------------------- */

/* iretq with the given cs/ss/rflags; rip 0 = the instruction after it, rsp = the current one. */
static __attribute__((noinline)) void iret_with(unsigned long cs, unsigned long ss, unsigned long fl, unsigned long rip) {
    __asm__ volatile("sub $128, %%rsp\n\t"
                     "mov %%rsp, %%rax\n\t"
                     "push %[ss]\n\t"
                     "push %%rax\n\t"
                     "push %[fl]\n\t"
                     "push %[cs]\n\t"
                     "lea 1f(%%rip), %%rax\n\t"
                     "test %[rip], %[rip]\n\t"
                     "cmovnz %[rip], %%rax\n\t"
                     "push %%rax\n\t"
                     "iretq\n"
                     "1:\n\t"
                     "add $128, %%rsp"
                     :
                     : [ss] "r"(ss), [cs] "r"(cs), [fl] "r"(fl), [rip] "r"(rip)
                     : "rax", "memory", "cc");
}

static void iret(void) {
    struct {
        const char *name;
        unsigned long cs, ss, fl, rip;
    } c[] = {
        { "own selectors", own_cs, own_ss, 0x202, 0 },
        { "ss null", own_cs, 0, 0x202, 0 },
        { "ss rpl 0", own_cs, own_ss & ~3u, 0x202, 0 },
        { "ss code selector", own_cs, own_cs, 0x202, 0 },
        { "ss ldt selector", own_cs, 0x0f, 0x202, 0 },
        { "ss beyond the gdt", own_cs, 0xfffb, 0x202, 0 },
        { "cs data selector", own_ss, own_ss, 0x202, 0 },
        { "cs rpl 0", own_cs & ~3u, own_ss, 0x202, 0 },
        { "cs null", 3, own_ss, 0x202, 0 },
        { "cs ldt selector", 0x0f, own_ss, 0x202, 0 },
        { "rip non-canonical", own_cs, own_ss, 0x202, NONCANON },
        { "rip kernel half", own_cs, own_ss, 0x202, KERNEL_TEXT },
        { "rflags iopl 3", own_cs, own_ss, 0x3202, 0 },
    };
    for (unsigned i = 0; i < sizeof c / sizeof c[0]; i++) {
        clear(1, c[i].rip == KERNEL_TEXT ? KERNEL_TEXT : 0);
        if (sigsetjmp(back, 1) == 0) {
            iret_with(c[i].cs, c[i].ss, c[i].fl, c[i].rip);
            if (!quiet && c[i].fl & 0x3000)
                printf("iretq %s: iopl after it %lu\n", c[i].name, (rflags_now() >> 12) & 3);
        }
        char what[64];
        snprintf(what, sizeof what, "iretq %s", c[i].name);
        report(what);
    }
}

/* ---- the storm ------------------------------------------------------------------------------------ */

static void storm(int rounds) {
    struct itimerval it = { { 0, 300 }, { 0, 300 } };
    set(SIGALRM, alarm_handler, SA_ONSTACK | SA_NODEFER | SA_RESTART, 0);
    setitimer(ITIMER_REAL, &it, 0);
    /* A GS base that points nowhere (musl does not use GS): every entry and NMI meets a foreign one. */
    int fsgs = (getauxval(AT_HWCAP2) & 2) != 0;
    quiet = 1;
    for (round_no = 0; round_no < rounds; round_no++) {
        step = 0;
        if (fsgs) {
            unsigned long gs = 0x00007e00dead0000ul + ((unsigned long)round_no << 12);
            __asm__ volatile("wrgsbase %0" : : "r"(gs));
        }
        nested();
        hostile();
        direct();
        iret();
    }
    while (!alarms)
        pause();
    memset(&it, 0, sizeof it);
    setitimer(ITIMER_REAL, &it, 0);
    if (fsgs)
        __asm__ volatile("wrgsbase %0" : : "r"(0ul));
    quiet = 0;
    printf("storm: %d rounds, every round as the first %s (%d mismatches)\n", rounds, mismatches ? "no" : "yes", mismatches);
    printf("storm: the timer signal arrived %s\n", alarms ? "yes" : "no");
    /* Only on a failure (Linux prints nothing here): the first step that differed and how. */
    if (first_bad)
        printf("storm: first mismatch at \"%s\": sig %d/%d code %d/%d trap %d/%d addr %d/%d alt %d/%d\n", first_bad,
               bad_was.sig, bad_now.sig, bad_was.code, bad_now.code, bad_was.trap, bad_now.trap, bad_was.addr_ok,
               bad_now.addr_ok, bad_was.on_alt, bad_now.on_alt);
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "hostile";
    printf("mode %s\n", m);
    /* The ktest puts one program on every CPU it samples (silent: the transcript stays the same). */
    const char *cpu = getenv("SIGSTORM_CPU");
    if (cpu && atoi(cpu) >= 0 && atoi(cpu) < 1024) {
        unsigned long set[16] = { 0 };
        set[atoi(cpu) / 64] |= 1ul << (atoi(cpu) % 64);
        syscall(SYS_sched_setaffinity, 0, sizeof set, set);
    }
    __asm__ volatile("mov %%cs, %0" : "=r"(own_cs));
    __asm__ volatile("mov %%ss, %0" : "=r"(own_ss));
    stack_t ss = { .ss_sp = alt, .ss_flags = 0, .ss_size = sizeof alt };
    sigaltstack(&ss, 0);
    set(SIGSEGV, fault_handler, SA_ONSTACK, 0);
    set(SIGBUS, fault_handler, SA_ONSTACK, 0);
    if (!strcmp(m, "nested"))
        nested();
    else if (!strcmp(m, "hostile"))
        hostile();
    else if (!strcmp(m, "direct"))
        direct();
    else if (!strcmp(m, "iret"))
        iret();
    else if (!strcmp(m, "storm"))
        storm(argc > 2 ? atoi(argv[2]) : 20);
    else
        return 99;
    printf("done\n");
    return 0;
}
