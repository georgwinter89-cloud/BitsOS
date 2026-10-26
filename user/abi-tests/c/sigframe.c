/* AB-6: signal delivery and the rt_sigframe (gVisor-style checks, the ABI diff against CachyOS).
 * argv[1]: info | context | mask | order | altstack. Every line is the same on Linux and BitsOS when the
 * kernels behave the same: no addresses, sizes or selector numbers are printed, only relations. */
#include "abi.h"
#include <setjmp.h>
#include <signal.h>
#include <sys/wait.h>
#include <ucontext.h>

#define MAGIC1 0x46505853u
#define MAGIC2 0x46505845u

struct ksa {
    unsigned long handler, flags, restorer, mask;
};

static volatile int got_sig, got_code, got_errno, got_pid, got_uid, got_val, got_count;
static volatile unsigned long got_flags, got_mask_word, got_oldmask, got_blocked;

static unsigned long blocked_now(void) {
    unsigned long m = 0;
    syscall(SYS_rt_sigprocmask, SIG_BLOCK, 0, &m, 8);
    return m;
}

static void info_handler(int sig, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    got_sig = sig;
    got_code = si->si_code;
    got_errno = si->si_errno;
    got_pid = si->si_pid;
    got_uid = si->si_uid;
    got_val = si->si_value.sival_int;
    got_flags = uc->uc_flags;
    got_mask_word = *(unsigned long *)&uc->uc_sigmask;
    got_oldmask = uc->uc_mcontext.gregs[REG_OLDMASK];
    got_blocked = blocked_now();
    got_count++;
}

static void set(int sig, void *h, unsigned long flags, unsigned long mask) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = h;
    sa.sa_flags = flags;
    memcpy(&sa.sa_mask, &mask, sizeof mask);
    sigaction(sig, &sa, 0);
}

static unsigned long bit(int sig) {
    return 1ul << (sig - 1);
}

static void show_info(const char *what) {
    printf("%s: sig %d code %d errno %d pid-self %d uid-self %d count %d\n", what, got_sig, got_code, got_errno,
           got_pid == getpid(), got_uid == (int)getuid(), got_count);
    got_count = 0;
}

static int raw_sigqueueinfo(int pid, int sig, int code, int val, int extra) {
    unsigned char b[128];
    memset(b, 0, sizeof b);
    memcpy(b + 8, &code, 4);
    memcpy(b + 16, &pid, 4);
    memcpy(b + 24, &val, 4);
    if (extra)
        b[64] = 1;
    return syscall(SYS_rt_sigqueueinfo, pid, sig, b);
}

static void info(void) {
    set(SIGUSR1, info_handler, SA_SIGINFO, 0);
    set(SIGRTMIN + 1, info_handler, SA_SIGINFO, 0);
    syscall(SYS_kill, getpid(), SIGUSR1);
    show_info("kill");
    printf("uc_flags %#lx\n", got_flags);
    printf("uc_sigmask is the mask before: %s\n", got_mask_word == 0 ? "yes" : "no");
    printf("oldmask equals uc_sigmask: %s\n", got_oldmask == got_mask_word ? "yes" : "no");
    printf("blocked in the handler: %#lx\n", got_blocked);
    syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), SIGUSR1);
    show_info("tgkill");
    union sigval v = { .sival_int = 42 };
    sigqueue(getpid(), SIGRTMIN + 1, v);
    show_info("sigqueue");
    printf("sigqueue value %d\n", got_val);
    SYS("rt_sigqueueinfo(self, NULL)", SYS_rt_sigqueueinfo, getpid(), SIGUSR1, (void *)0);
    printf("rt_sigqueueinfo(self, code 0) = %d\n", raw_sigqueueinfo(getpid(), SIGUSR1, 0, 7, 0));
    show_info("queued with code 0");
    printf("rt_sigqueueinfo(self, code -42, zero tail) = %d\n", raw_sigqueueinfo(getpid(), SIGRTMIN + 1, -42, 9, 0));
    show_info("queued with code -42");
    printf("rt_sigqueueinfo(self, code -42, non-zero tail) = %d (%s)\n", raw_sigqueueinfo(getpid(), SIGRTMIN + 1, -42, 9, 1), ename(errno));
    printf("rt_sigqueueinfo(init, SI_QUEUE) = %d (%s)\n", raw_sigqueueinfo(1, SIGUSR1, -1, 1, 0), ename(errno));
    printf("rt_sigqueueinfo(init, SI_USER) = %d (%s)\n", raw_sigqueueinfo(1, SIGUSR1, 0, 1, 0), ename(errno));
    printf("rt_sigqueueinfo(no such pid, SI_QUEUE) = %d (%s)\n", raw_sigqueueinfo(4000000, SIGUSR1, -1, 1, 0), ename(errno));
    printf("rt_sigqueueinfo(self, signal 65) = %d (%s)\n", raw_sigqueueinfo(getpid(), 65, -1, 1, 0), ename(errno));
    SYS("rt_sigqueueinfo(self, bad pointer)", SYS_rt_sigqueueinfo, getpid(), SIGUSR1, KERNEL_ADDR);
    unsigned char b[128];
    memset(b, 0, sizeof b);
    int code = SI_TKILL;
    memcpy(b + 8, &code, 4);
    SYS("rt_tgsigqueueinfo(self, SI_TKILL)", SYS_rt_tgsigqueueinfo, getpid(), syscall(SYS_gettid), SIGUSR1, b);
    show_info("rt_tgsigqueueinfo");
    SYS("rt_tgsigqueueinfo(tid 0)", SYS_rt_tgsigqueueinfo, getpid(), 0, SIGUSR1, b);
    SYS("rt_tgsigqueueinfo(other tgid)", SYS_rt_tgsigqueueinfo, getpid() + 1000000, syscall(SYS_gettid), SIGUSR1, b);
    /* No SA_RESTORER: x86-64 refuses to build the frame and the program gets SIGSEGV. */
    pid_t c = fork();
    if (c == 0) {
        struct ksa k = { (unsigned long)info_handler, SA_SIGINFO, 0, 0 };
        syscall(SYS_rt_sigaction, SIGUSR2, &k, 0, 8);
        syscall(SYS_kill, getpid(), SIGUSR2);
        _exit(0);
    }
    int st = 0;
    waitpid(c, &st, 0);
    printf("handler without SA_RESTORER: signaled %d sig %d\n", WIFSIGNALED(st), WIFSIGNALED(st) ? WTERMSIG(st) : -1);
}

/* ---- registers and FPU state across a handler ---- */

static unsigned char ymm_in[32] __attribute__((aligned(32)));
static unsigned char ymm_out[32] __attribute__((aligned(32)));
static unsigned char ymm_seen[32] __attribute__((aligned(32)));
static volatile unsigned long h_rip, h_rsp, h_rax, h_rbx, h_r12, h_r15, h_csgsfs, h_efl;
static volatile unsigned h_mxcsr, h_saved_mxcsr, h_magic1, h_magic2, h_ext, h_xsize, h_bv;
static volatile unsigned long h_xfeat, h_fpalign, h_fp_inside;

static void ctx_handler(int sig, siginfo_t *si, void *ucv) {
    unsigned char t[32] __attribute__((aligned(32)));
    unsigned mx;
    __asm__ volatile("vmovdqu %%ymm1, %0\n\tstmxcsr %1" : "=m"(t), "=m"(mx));
    (void)sig;
    (void)si;
    memcpy(ymm_seen, t, 32);
    h_mxcsr = mx;
    ucontext_t *uc = ucv;
    greg_t *g = uc->uc_mcontext.gregs;
    h_rip = g[REG_RIP];
    h_rsp = g[REG_RSP];
    h_rax = g[REG_RAX];
    h_rbx = g[REG_RBX];
    h_r12 = g[REG_R12];
    h_r15 = g[REG_R15];
    h_csgsfs = g[REG_CSGSFS];
    h_efl = g[REG_EFL];
    unsigned char *fp = (unsigned char *)uc->uc_mcontext.fpregs;
    h_fpalign = (unsigned long)fp % 64;
    unsigned long me = (unsigned long)&mx;
    h_fp_inside = (unsigned long)fp > me;
    memcpy((void *)&h_saved_mxcsr, fp + 24, 4);
    memcpy((void *)&h_magic1, fp + 464, 4);
    memcpy((void *)&h_ext, fp + 468, 4);
    memcpy((void *)&h_xfeat, fp + 472, 8);
    memcpy((void *)&h_xsize, fp + 480, 4);
    memcpy((void *)&h_bv, fp + 512, 4);
    if (h_xsize >= 576 && h_xsize < 65536)
        memcpy((void *)&h_magic2, fp + h_xsize, 4);
    /* Change the saved state: MXCSR rounding up, the low dword of xmm1 flipped. */
    unsigned nm = 0x5F80;
    memcpy(fp + 24, &nm, 4);
    fp[160 + 16] ^= 0xFF;
}

static void context(void) {
    set(SIGUSR1, ctx_handler, SA_SIGINFO, 0);
    memset(ymm_in, 0x5a, 32);
    ymm_in[31] = 0xa5;
    unsigned mx = 0x7F80, mx_out = 0, dflt = 0x1F80;
    unsigned long lab = 0, sp = 0, rbx_after = 0, cs = 0, ss = 0;
    long ret;
    long pid = getpid(), tid = syscall(SYS_gettid);
    __asm__ volatile("mov %%cs, %[cs]\n\t"
                     "mov %%ss, %[ss]\n\t"
                     "ldmxcsr %[mx]\n\t"
                     "vmovdqu %[in], %%ymm1\n\t"
                     "mov $0x1111, %%rbx\n\t"
                     "mov $0x1212, %%r12\n\t"
                     "mov $0x1515, %%r15\n\t"
                     "mov %%rsp, %[sp]\n\t"
                     "syscall\n"
                     "1:\n\t"
                     "lea 1b(%%rip), %[lab]\n\t"
                     "vmovdqu %%ymm1, %[out]\n\t"
                     "stmxcsr %[mxo]\n\t"
                     "mov %%rbx, %[rbxa]\n\t"
                     "ldmxcsr %[dflt]\n\t"
                     : "=a"(ret), [lab] "=&r"(lab), [sp] "=&r"(sp), [out] "=m"(ymm_out), [mxo] "=m"(mx_out), [rbxa] "=&r"(rbx_after),
                       [cs] "=&r"(cs), [ss] "=&r"(ss)
                     : "0"((long)SYS_tgkill), "D"(pid), "S"(tid), "d"((long)SIGUSR1), [mx] "m"(mx), [in] "m"(ymm_in), [dflt] "m"(dflt)
                     : "rbx", "r12", "r15", "rcx", "r11", "xmm1", "memory");
    printf("tgkill returned %ld\n", ret);
    printf("saved rip is after the syscall: %s\n", h_rip == lab ? "yes" : "no");
    printf("saved rsp is the program's: %s\n", h_rsp == sp ? "yes" : "no");
    printf("saved rax is the syscall's answer 0: %s\n", h_rax == 0 ? "yes" : "no");
    printf("saved rbx r12 r15: %#lx %#lx %#lx\n", h_rbx, h_r12, h_r15);
    printf("saved cs is the program's: %s\n", (h_csgsfs & 0xffff) == cs ? "yes" : "no");
    printf("saved ss is the program's: %s\n", (h_csgsfs >> 48) == ss ? "yes" : "no");
    printf("saved gs/fs words zero: %s\n", ((h_csgsfs >> 16) & 0xffffffff) == 0 ? "yes" : "no");
    printf("saved rflags has IF: %s\n", (h_efl & 0x200) ? "yes" : "no");
    printf("handler MXCSR %#x (the initial state)\n", h_mxcsr);
    int zero = 1;
    for (int i = 0; i < 32; i++)
        zero &= ymm_seen[i] == 0;
    printf("handler ymm1 zero: %s\n", zero ? "yes" : "no");
    printf("saved MXCSR %#x\n", h_saved_mxcsr);
    printf("fpstate 64-byte aligned: %s, above the handler's stack: %s\n", h_fpalign == 0 ? "yes" : "no", h_fp_inside ? "yes" : "no");
    printf("sw bytes: magic1 %s, extended = xstate_size + 4: %s, xfeatures has x87|SSE|AVX: %s, magic2 %s\n",
           h_magic1 == MAGIC1 ? "ok" : "bad", h_ext == h_xsize + 4 ? "yes" : "no", (h_xfeat & 7) == 7 ? "yes" : "no",
           h_magic2 == MAGIC2 ? "ok" : "bad");
    printf("XSTATE_BV has x87|SSE|AVX: %s\n", (h_bv & 7) == 7 ? "yes" : "no");
    printf("after return: MXCSR %#x, rbx %#lx\n", mx_out, rbx_after);
    int low = ymm_out[0] == (0x5a ^ 0xff), rest = 1;
    for (int i = 1; i < 32; i++)
        rest &= ymm_out[i] == ymm_in[i];
    printf("after return: xmm1 low byte changed by the handler: %s, the rest of ymm1 kept: %s\n", low ? "yes" : "no", rest ? "yes" : "no");
}

/* ---- masks ---- */

static void mask_handler(int sig, siginfo_t *si, void *ucv) {
    (void)si;
    got_sig = sig;
    got_blocked = blocked_now();
    got_count++;
    if (sig == SIGUSR2) {
        ucontext_t *uc = ucv;
        *(unsigned long *)&uc->uc_sigmask |= bit(SIGWINCH);
    }
}

static void masks(void) {
    set(SIGUSR1, mask_handler, SA_SIGINFO, bit(SIGUSR2) | bit(SIGTERM));
    raise(SIGUSR1);
    printf("sa_mask: blocked in the handler %#lx, after %#lx\n", got_blocked, blocked_now());
    set(SIGUSR1, mask_handler, SA_SIGINFO | SA_NODEFER, 0);
    raise(SIGUSR1);
    printf("SA_NODEFER: blocked in the handler %#lx\n", got_blocked);
    set(SIGUSR1, mask_handler, SA_SIGINFO | SA_RESETHAND, 0);
    raise(SIGUSR1);
    struct sigaction o;
    sigaction(SIGUSR1, 0, &o);
    printf("SA_RESETHAND: blocked in the handler %#lx, handler now SIG_DFL: %s, flags %#lx\n", got_blocked,
           o.sa_handler == SIG_DFL ? "yes" : "no", (unsigned long)o.sa_flags);
    set(SIGUSR2, mask_handler, SA_SIGINFO, 0);
    raise(SIGUSR2);
    printf("uc_sigmask changed by the handler: blocked after %#lx\n", blocked_now());
    unsigned long none = 0;
    syscall(SYS_rt_sigprocmask, SIG_SETMASK, &none, 0, 8);
    /* A blocked handled signal waits; unblocking delivers it before sigprocmask returns. */
    got_count = 0;
    unsigned long b = bit(SIGUSR2);
    syscall(SYS_rt_sigprocmask, SIG_BLOCK, &b, 0, 8);
    raise(SIGUSR2);
    printf("blocked: delivered %d\n", got_count);
    syscall(SYS_rt_sigprocmask, SIG_UNBLOCK, &b, 0, 8);
    printf("unblocked: delivered %d\n", got_count);
    syscall(SYS_rt_sigprocmask, SIG_SETMASK, &none, 0, 8);
}

/* ---- order and queueing ---- */

static volatile int seq[32], nseq;
static void order_handler(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    if (nseq < 32)
        seq[nseq++] = sig * 100 + (si->si_code == SI_QUEUE ? si->si_value.sival_int : 0);
}

static void show_seq(const char *what) {
    printf("%s:", what);
    for (int i = 0; i < nseq; i++)
        printf(" %d", seq[i]);
    printf("\n");
    nseq = 0;
}

static void order(void) {
    int sigs[4] = { SIGRTMIN + 3, SIGUSR2, SIGRTMIN + 1, SIGUSR1 };
    unsigned long all = 0;
    for (int i = 0; i < 4; i++)
        all |= bit(sigs[i]);
    for (int round = 0; round < 2; round++) {
        for (int i = 0; i < 4; i++)
            set(sigs[i], order_handler, SA_SIGINFO, round ? ~0ul : 0);
        syscall(SYS_rt_sigprocmask, SIG_BLOCK, &all, 0, 8);
        for (int i = 0; i < 4; i++)
            raise(sigs[i]);
        unsigned long p = 0;
        syscall(SYS_rt_sigpending, &p, 8);
        printf("pending %#lx\n", p);
        syscall(SYS_rt_sigprocmask, SIG_UNBLOCK, &all, 0, 8);
        show_seq(round ? "handlers with everything in sa_mask" : "handlers with an empty sa_mask");
    }
    /* Real-time signals queue once per send, in order; standard ones once. */
    unsigned long rt = bit(SIGRTMIN + 2) | bit(SIGUSR1);
    set(SIGRTMIN + 2, order_handler, SA_SIGINFO, 0);
    set(SIGUSR1, order_handler, SA_SIGINFO, 0);
    syscall(SYS_rt_sigprocmask, SIG_BLOCK, &rt, 0, 8);
    for (int v = 1; v <= 3; v++) {
        union sigval sv = { .sival_int = v };
        sigqueue(getpid(), SIGRTMIN + 2, sv);
        sigqueue(getpid(), SIGUSR1, sv);
    }
    syscall(SYS_rt_sigprocmask, SIG_UNBLOCK, &rt, 0, 8);
    show_seq("three sigqueues each of SIGUSR1 and SIGRTMIN+2");
    /* RLIMIT_SIGPENDING's exact count is per real-user on Linux (other processes of the user skew it) and
     * per thread-group on V2: not compared here (host-dependent). The host/ktest checks V2's own rule. */
}

/* ---- the alternate stack ---- */

static char alt[65536] __attribute__((aligned(16)));
static volatile unsigned long a_local, a_flags_in, a_uc_flags, a_uc_size, a_uc_sp_ok, a_depth;

static void alt_handler(int sig, siginfo_t *si, void *ucv) {
    (void)si;
    char here;
    ucontext_t *uc = ucv;
    a_local = (unsigned long)&here;
    stack_t q;
    sigaltstack(0, &q);
    a_flags_in = q.ss_flags;
    a_uc_flags = uc->uc_stack.ss_flags;
    a_uc_size = uc->uc_stack.ss_size;
    a_uc_sp_ok = uc->uc_stack.ss_sp == alt;
    if (sig == SIGUSR2 && a_depth++ < 1)
        raise(SIGUSR2);
}

static int on_alt(unsigned long a) {
    return a > (unsigned long)alt && a <= (unsigned long)alt + sizeof alt;
}

static void recurse_handler(int sig) {
    (void)sig;
    raise(SIGUSR1);
}

static void altstack(void) {
    stack_t ss = { .ss_sp = alt, .ss_flags = 0, .ss_size = sizeof alt };
    sigaltstack(&ss, 0);
    set(SIGUSR1, alt_handler, SA_SIGINFO | SA_ONSTACK, 0);
    raise(SIGUSR1);
    printf("SA_ONSTACK: handler on the alternate stack %s, sigaltstack in it says %#lx, uc_stack flags %#lx size-ok %d sp-ok %ld\n",
           on_alt(a_local) ? "yes" : "no", a_flags_in, a_uc_flags, a_uc_size == sizeof alt, a_uc_sp_ok);
    set(SIGUSR1, alt_handler, SA_SIGINFO, 0);
    raise(SIGUSR1);
    printf("without SA_ONSTACK: on the alternate stack %s\n", on_alt(a_local) ? "yes" : "no");
    /* A nested signal while on it stays on it, below the first frame. */
    set(SIGUSR2, alt_handler, SA_SIGINFO | SA_ONSTACK | SA_NODEFER, 0);
    a_depth = 0;
    raise(SIGUSR2);
    printf("nested on the alternate stack: %s (depth %lu)\n", on_alt(a_local) ? "yes" : "no", a_depth);
    /* SS_AUTODISARM: disarmed inside the handler, back after sigreturn. */
    ss.ss_flags = SS_AUTODISARM;
    sigaltstack(&ss, 0);
    set(SIGUSR1, alt_handler, SA_SIGINFO | SA_ONSTACK, 0);
    raise(SIGUSR1);
    stack_t q;
    sigaltstack(0, &q);
    printf("SS_AUTODISARM: on it %s, inside %#lx, uc_stack flags %#lx, after return %#x\n", on_alt(a_local) ? "yes" : "no",
           a_flags_in, a_uc_flags, (unsigned)q.ss_flags);
    ss.ss_flags = SS_DISABLE;
    sigaltstack(&ss, 0);
    /* Too small for the frames: the overflow ends the program with SIGSEGV. */
    pid_t c = fork();
    if (c == 0) {
        static char small[8192];
        stack_t s2 = { .ss_sp = small, .ss_flags = 0, .ss_size = sizeof small };
        sigaltstack(&s2, 0);
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_handler = recurse_handler;
        sa.sa_flags = SA_ONSTACK | SA_NODEFER;
        sigaction(SIGUSR1, &sa, 0);
        raise(SIGUSR1);
        _exit(0);
    }
    int st = 0;
    waitpid(c, &st, 0);
    printf("alternate stack overflow: signaled %d sig %d\n", WIFSIGNALED(st), WIFSIGNALED(st) ? WTERMSIG(st) : -1);
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "info";
    printf("mode %s\n", m);
    if (!strcmp(m, "info"))
        info();
    else if (!strcmp(m, "context"))
        context();
    else if (!strcmp(m, "mask"))
        masks();
    else if (!strcmp(m, "order"))
        order();
    else if (!strcmp(m, "altstack"))
        altstack();
    else
        return 99;
    printf("done\n");
    return 0;
}
