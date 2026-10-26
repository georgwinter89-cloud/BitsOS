/* Wave A: rt_sigaction, rt_sigprocmask, rt_sigpending, sigaltstack, kill/tkill/tgkill rules, default actions.
 * Raw syscalls with the kernel's struct sigaction. No signal is ever delivered to a handler here (V2 runs
 * handlers from AB-6 on); the program ends with SIGTERM's default action. */
#include "abi.h"
#include <limits.h>
#include <signal.h>

struct ksa {
    unsigned long handler, flags, restorer, mask;
};

static void handler(int s) {
    (void)s;
}

static unsigned long pending(void) {
    unsigned long p = 0xdead;
    syscall(SYS_rt_sigpending, &p, 8);
    return p;
}

static void actions(void) {
    struct ksa a = { (unsigned long)handler, 0x04000004ul | (1ul << 40), (unsigned long)handler, ~0ul }, o;
    SYS("rt_sigaction(sigsetsize 4)", SYS_rt_sigaction, SIGUSR1, &a, 0, 4);
    SYS("rt_sigaction(bad act)", SYS_rt_sigaction, SIGUSR1, KERNEL_ADDR, 0, 8);
    SYS("rt_sigaction(SIGKILL, act)", SYS_rt_sigaction, SIGKILL, &a, 0, 8);
    SYS("rt_sigaction(SIGSTOP, act)", SYS_rt_sigaction, SIGSTOP, &a, 0, 8);
    SYS("rt_sigaction(0)", SYS_rt_sigaction, 0, 0, &o, 8);
    SYS("rt_sigaction(65)", SYS_rt_sigaction, 65, 0, &o, 8);
    SYS("rt_sigaction(-1)", SYS_rt_sigaction, -1, 0, &o, 8);
    memset(&o, 0x55, sizeof o);
    SYS("rt_sigaction(SIGKILL, query)", SYS_rt_sigaction, SIGKILL, 0, &o, 8);
    printf("SIGKILL: handler %lu flags %#lx mask %#lx\n", o.handler, o.flags, o.mask);
    SYS("rt_sigaction(SIGUSR1, act)", SYS_rt_sigaction, SIGUSR1, &a, 0, 8);
    memset(&o, 0x55, sizeof o);
    SYS("rt_sigaction(SIGUSR1, query)", SYS_rt_sigaction, SIGUSR1, 0, &o, 8);
    yes("handler and restorer read back", o.handler == (unsigned long)handler && o.restorer == (unsigned long)handler);
    printf("SIGUSR1: flags %#lx mask %#lx\n", o.flags, o.mask);
    SYS("rt_sigaction(SIGUSR2, act, kernel oact)", SYS_rt_sigaction, SIGUSR2, &a, KERNEL_ADDR, 8);
    SYS("rt_sigaction(SIGUSR2, query)", SYS_rt_sigaction, SIGUSR2, 0, &o, 8);
    yes("SIGUSR2 was changed before the fault", o.handler == (unsigned long)handler);
    SYS("rt_sigaction(SIGCHLD, query)", SYS_rt_sigaction, SIGCHLD, 0, &o, 8);
    printf("SIGCHLD: handler %lu flags %#lx\n", o.handler, o.flags);
}

static void masks(void) {
    unsigned long set = ~0ul, old = 0x55;
    SYS("rt_sigprocmask(sigsetsize 16)", SYS_rt_sigprocmask, SIG_BLOCK, &set, &old, 16);
    SYS("rt_sigprocmask(how 3)", SYS_rt_sigprocmask, 3, &set, &old, 8);
    SYS("rt_sigprocmask(how 3, no set)", SYS_rt_sigprocmask, 3, 0, &old, 8);
    SYS("rt_sigprocmask(bad set)", SYS_rt_sigprocmask, SIG_BLOCK, KERNEL_ADDR, 0, 8);
    SYS("rt_sigprocmask(bad old)", SYS_rt_sigprocmask, SIG_BLOCK, 0, KERNEL_ADDR, 8);
    SYS("rt_sigprocmask(SIG_BLOCK, all)", SYS_rt_sigprocmask, SIG_BLOCK, &set, &old, 8);
    printf("old mask %#lx\n", old);
    SYS("rt_sigprocmask(query)", SYS_rt_sigprocmask, SIG_BLOCK, 0, &old, 8);
    printf("blocked %#lx\n", old);
    /* Blocked signals wait: SIGUSR1 (handler) and SIGCHLD (default: ignore) both stay pending. */
    SYS("tkill(self, SIGUSR1)", SYS_tkill, syscall(SYS_gettid), SIGUSR1);
    SYS("kill(self, SIGCHLD)", SYS_kill, getpid(), SIGCHLD);
    printf("pending %#lx\n", pending());
    SYS("rt_sigpending(size 9)", SYS_rt_sigpending, &old, 9);
    unsigned char b[2] = { 0xee, 0xee };
    SYS("rt_sigpending(size 1)", SYS_rt_sigpending, b, 1);
    printf("1-byte pending %#x %#x\n", b[0], b[1]);
    /* SIG_IGN discards a pending signal, blocked or not. */
    struct ksa ign = { 1, 0, 0, 0 };
    SYS("rt_sigaction(SIGUSR1, SIG_IGN)", SYS_rt_sigaction, SIGUSR1, &ign, 0, 8);
    printf("pending after SIG_IGN %#lx\n", pending());
    /* Unblocking SIGCHLD (default: ignore) drops it. */
    unsigned long none = 0;
    SYS("rt_sigprocmask(SIG_SETMASK, none)", SYS_rt_sigprocmask, SIG_SETMASK, &none, 0, 8);
    printf("pending after unblock %#lx\n", pending());
}

static void altstack(void) {
    static char mem[16384];
    stack_t ss, oss;
    memset(&oss, 0x55, sizeof oss);
    SYS("sigaltstack(query)", SYS_sigaltstack, 0, &oss);
    printf("initial: flags %d size %zu sp-null %d\n", oss.ss_flags, oss.ss_size, oss.ss_sp == 0);
    ss = (stack_t){ .ss_sp = mem, .ss_flags = 0, .ss_size = 2047 };
    SYS("sigaltstack(2047 bytes)", SYS_sigaltstack, &ss, 0);
    ss = (stack_t){ .ss_sp = mem, .ss_flags = 4, .ss_size = 8192 };
    SYS("sigaltstack(flags 4)", SYS_sigaltstack, &ss, 0);
    SYS("sigaltstack(bad ss)", SYS_sigaltstack, KERNEL_ADDR, 0);
    ss = (stack_t){ .ss_sp = mem, .ss_flags = SS_AUTODISARM, .ss_size = 8192 };
    SYS("sigaltstack(set, SS_AUTODISARM)", SYS_sigaltstack, &ss, &oss);
    printf("old: flags %d\n", oss.ss_flags);
    SYS("sigaltstack(query)", SYS_sigaltstack, 0, &oss);
    printf("now: flags %#x size %zu same sp %d\n", (unsigned)oss.ss_flags, oss.ss_size, oss.ss_sp == mem);
    ss = (stack_t){ .ss_sp = mem, .ss_flags = SS_DISABLE, .ss_size = 1 };
    SYS("sigaltstack(SS_DISABLE)", SYS_sigaltstack, &ss, 0);
    SYS("sigaltstack(query)", SYS_sigaltstack, 0, &oss);
    printf("disabled: flags %d size %zu sp-null %d\n", oss.ss_flags, oss.ss_size, oss.ss_sp == 0);
    SYS("sigaltstack(kernel oss)", SYS_sigaltstack, 0, KERNEL_ADDR);
}

static void kills(void) {
    pid_t pid = getpid();
    SYS("kill(self, 0)", SYS_kill, pid, 0);
    SYS("kill(self, 65)", SYS_kill, pid, 65);
    SYS("kill(self, -1)", SYS_kill, pid, -1);
    SYS("kill(no such pid, SIGTERM)", SYS_kill, 4000000, SIGTERM);
    SYS("kill(no such pid, 99)", SYS_kill, 4000000, 99);
    SYS("kill(INT_MIN, 0)", SYS_kill, INT_MIN, 0);
    SYS("kill(0, 0)", SYS_kill, 0, 0);
    SYS("tkill(0, SIGTERM)", SYS_tkill, 0, SIGTERM);
    SYS("tkill(-5, 0)", SYS_tkill, -5, 0);
    SYS("tgkill(0, tid, 0)", SYS_tgkill, 0, pid, 0);
    SYS("tgkill(pid, 0, 0)", SYS_tgkill, pid, 0, 0);
    SYS("tgkill(no such tgid, tid, 0)", SYS_tgkill, 4000000, pid, 0);
    SYS("tgkill(pid, tid, 0)", SYS_tgkill, pid, syscall(SYS_gettid), 0);
    SYS("tgkill(pid, tid, 65)", SYS_tgkill, pid, syscall(SYS_gettid), 65);
    SYS("kill(self, SIGCHLD)", SYS_kill, pid, SIGCHLD);
    SYS("kill(self, SIGWINCH)", SYS_kill, pid, SIGWINCH);
    SYS("kill(self, SIGURG)", SYS_kill, pid, SIGURG);
    struct ksa ign = { 1, 0, 0, 0 };
    SYS("rt_sigaction(SIGTERM, SIG_IGN)", SYS_rt_sigaction, SIGTERM, &ign, 0, 8);
    SYS("kill(self, SIGTERM) while ignored", SYS_kill, pid, SIGTERM);
    puts("still running after the ignored signals");
    struct ksa dfl = { 0, 0, 0, 0 };
    SYS("rt_sigaction(SIGTERM, SIG_DFL)", SYS_rt_sigaction, SIGTERM, &dfl, 0, 8);
}

int main(void) {
    actions();
    masks();
    altstack();
    kills();
    puts("raising SIGTERM (its default action ends the program)");
    fflush(stdout);
    syscall(SYS_kill, getpid(), SIGTERM);
    puts("not reached");
    return 0;
}
