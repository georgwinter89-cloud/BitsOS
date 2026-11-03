/* AB-10a: the TLS/LDT ABI and the selector layout Wine's WoW64 needs. A 64-bit program that sets a TLS slot
 * and an LDT entry the way Wine does (int $0x80 with the 32-bit ABI and a buffer in the low 4 GiB, since int
 * 0x80 truncates pointer arguments to 32 bits), reads them back, checks the selector numbers programs hard-code,
 * and faults on a bad far return. `code32` runs code with CS 0x23 (int 0x80 from there, TLS and LDT selectors in
 * use, a fault and a timer signal in 32-bit code and sigreturn back into it); `threads`, `fork`, `exec` and `ldtr` check
 * that TLS slots and selectors are per thread, the LDT per process (shared by threads, copied by fork, gone at
 * exec, never leaking into another process on the same CPU); `hostile` feeds int 0x80 dirty registers, odd numbers and bad pointers. Prints relations and errno
 * names only: a difference means a kernel difference.
 *
 * On x86-64 the 64-bit set_thread_area (205) and get_thread_area (211) have no handler and answer ENOSYS
 * (verified on CachyOS); only modify_ldt (154) is a real 64-bit syscall. The TLS calls are reached through int
 * 0x80 (243/244), as Wine does. Needs CONFIG_IA32_EMULATION and CONFIG_MODIFY_LDT_SYSCALL, which CachyOS has. */
#include "abi.h"
#include <pthread.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <ucontext.h>

struct user_desc {
    unsigned int entry_number;
    unsigned int base_addr;
    unsigned int limit;
    unsigned int seg_32bit : 1;
    unsigned int contents : 2;
    unsigned int read_exec_only : 1;
    unsigned int limit_in_pages : 1;
    unsigned int seg_not_present : 1;
    unsigned int useable : 1;
    unsigned int lm : 1;
};

/* int $0x80 with up to three 32-bit arguments (ebx, ecx, edx); the result in eax, sign-extended. */
static long int80(int nr, unsigned long a, unsigned long b, unsigned long c) {
    long r;
    __asm__ volatile("int $0x80" : "=a"(r) : "a"(nr), "b"(a), "c"(b), "d"(c) : "memory");
    return r;
}

#define NR32_modify_ldt 123
#define NR32_set_thread_area 243
#define NR32_get_thread_area 244

static void show32(const char *label, long r) {
    if (r < 0 && r >= -4095)
        printf("%s = -%s\n", label, ename((int)-r));
    else
        printf("%s = %ld\n", label, r);
}

/* A page in the low 4 GiB for the int 0x80 user_desc (its pointer argument is truncated to 32 bits). */
static void *low;
static struct user_desc *ud;

static struct user_desc data_seg(int entry) {
    struct user_desc d = {0};
    d.entry_number = entry;
    d.limit = 0xfffff;
    d.seg_32bit = 1;
    d.limit_in_pages = 1;
    d.useable = 1;
    return d;
}

static void tls(void) {
    /* 64-bit set/get_thread_area answer ENOSYS on x86-64. */
    struct user_desc g = data_seg(12);
    SYS("set_thread_area (64-bit)", SYS_set_thread_area, &g);
    SYS("get_thread_area (64-bit)", SYS_get_thread_area, &g);

    /* int 0x80 allocation (entry -1): the kernel picks a free TLS slot and writes it back. */
    *ud = data_seg(-1);
    show32("int80 set_thread_area alloc", int80(NR32_set_thread_area, (unsigned long)ud, 0, 0));
    int slot = ud->entry_number;
    yes("entry is a TLS slot 12-14", slot >= 12 && slot <= 14);

    /* Read it back through int 0x80. */
    struct user_desc *g2 = ud;
    memset(g2, 0, sizeof *g2);
    g2->entry_number = slot;
    show32("int80 get_thread_area", int80(NR32_get_thread_area, (unsigned long)g2, 0, 0));
    yes("base read back", g2->base_addr == 0);
    yes("limit read back", g2->limit == 0xfffff);
    yes("32-bit flag read back", g2->seg_32bit == 1);
    yes("pages flag read back", g2->limit_in_pages == 1);

    /* A fixed slot with a non-zero base. */
    *ud = data_seg(slot);
    ud->base_addr = 0x1000;
    show32("int80 set_thread_area fixed", int80(NR32_set_thread_area, (unsigned long)ud, 0, 0));
    memset(ud, 0, sizeof *ud);
    ud->entry_number = slot;
    int80(NR32_get_thread_area, (unsigned long)ud, 0, 0);
    yes("new base read back", ud->base_addr == 0x1000);

    /* Errors. */
    *ud = data_seg(99);
    show32("int80 set_thread_area out of range", int80(NR32_set_thread_area, (unsigned long)ud, 0, 0));
    *ud = data_seg(slot);
    ud->contents = 2; /* a code segment: not allowed in TLS */
    show32("int80 set_thread_area code seg", int80(NR32_set_thread_area, (unsigned long)ud, 0, 0));
    show32("int80 set_thread_area EFAULT", int80(NR32_set_thread_area, 0xffff000, 0, 0));

    /* Clearing a slot (LDT_empty). */
    memset(ud, 0, sizeof *ud);
    ud->entry_number = slot;
    ud->read_exec_only = 1;
    ud->seg_not_present = 1;
    show32("int80 set_thread_area clear", int80(NR32_set_thread_area, (unsigned long)ud, 0, 0));
}

static void ldt(void) {
    /* modify_ldt as a 64-bit syscall: write (func 0x11), then read (func 0). */
    struct user_desc d = data_seg(0);
    show("modify_ldt write", syscall(SYS_modify_ldt, 0x11, &d, sizeof d));
    unsigned char buf[16] = {0};
    long n = syscall(SYS_modify_ldt, 0, buf, sizeof buf);
    show("modify_ldt read", n);
    yes("entry 0 present (not all zero)", n == 16 && (buf[5] & 0x80));

    /* The same through int 0x80 (Wine's path) with a low buffer: write entry 1, read it back. */
    *ud = data_seg(1);
    show32("int80 modify_ldt write", int80(NR32_modify_ldt, 1, (unsigned long)ud, sizeof *ud));
    unsigned char *b2 = (unsigned char *)ud;
    memset(b2, 0, 32);
    long n2 = int80(NR32_modify_ldt, 0, (unsigned long)b2, 32);
    show32("int80 modify_ldt read", n2);
    yes("entry 1 present", n2 >= 16 && (b2[13] & 0x80));

    /* func 2 reads the default LDT size; errors. */
    unsigned char def[256] = {0};
    show("modify_ldt read_default", syscall(SYS_modify_ldt, 2, def, sizeof def));
    show("modify_ldt bad func", syscall(SYS_modify_ldt, 5, buf, sizeof buf));
    show("modify_ldt short", syscall(SYS_modify_ldt, 0x11, &d, 8));
    struct user_desc big = data_seg(8192);
    show("modify_ldt out of range", syscall(SYS_modify_ldt, 0x11, &big, sizeof big));
}

/* int 0x80 with a number outside the 32-bit table answers -ENOSYS on both kernels, never a signal. (Real
 * 32-bit syscalls like getpid work on CachyOS's full IA32 emulation but are -ENOSYS on V2 by design, so they
 * are not compared here.) */
static void int80_enosys(void) {
    show32("int80 nr 500", int80(500, 0, 0, 0));
    show32("int80 nr 1000", int80(1000, 0, 0, 0));
}

static sigjmp_buf jb;
static volatile int caught, caught_signo;

static void onseg(int sig, siginfo_t *si, void *uc) {
    (void)si;
    (void)uc;
    caught = 1;
    caught_signo = sig;
    siglongjmp(jb, 1);
}

static void selectors(void) {
    unsigned short cs, ss;
    __asm__ volatile("mov %%cs, %0; mov %%ss, %1" : "=r"(cs), "=r"(ss));
    yes("cs is 0x33", cs == 0x33);
    yes("ss is 0x2b", ss == 0x2b);

    struct sigaction sa = {0}, old;
    sa.sa_sigaction = onseg;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, &old);
    caught = 0;
    if (sigsetjmp(jb, 1) == 0) {
        unsigned long bad_cs = 0x2b; /* a data selector used as code */
        __asm__ volatile(
            "leaq 1f(%%rip), %%rax\n\t"
            "pushq %0\n\t"
            "pushq %%rax\n\t"
            "lretq\n\t"
            "1:\n\t"
            :
            : "r"(bad_cs)
            : "rax", "memory", "cc");
    }
    yes("bad far return faulted", caught);
    yes("fault was SIGSEGV", caught_signo == SIGSEGV);
    sigaction(SIGSEGV, &old, NULL);
}

/* ---- code in a 32-bit segment ----------------------------------------------------------------------
 * The program is static-PIE (loaded high), so the code that runs with CS 0x23 is copied into a page below
 * 4 GiB; it is position-independent. b_enter64(stack, entry, eax, ebx, ecx, edx) switches to a low stack,
 * loads DS/ES with 0x2b (a 64-bit program starts with them null, which 32-bit code cannot use) and far-returns
 * to `entry` in 32-bit mode; every 32-bit routine ends in b_to64, which far-returns to b_back64 (CS 0x33),
 * where the 64-bit state comes back. The routine's eax is the result. */
__asm__(".pushsection .rodata.wow64blob, \"a\"\n"
        ".globl wow_blob_start, wow_blob_end, b_enter64, b_f_int80, b_f_cs, b_f_ud2, b_f_spin, b_f_ds, b_f_gs\n"
        "wow_blob_start:\n"
        ".code64\n"
        "b_enter64:\n"
        "  push %rbx\n push %rbp\n push %r12\n push %r13\n push %r14\n push %r15\n"
        "  mov %rsp, b_saved_rsp(%rip)\n"
        "  mov %ds, %eax\n mov %eax, b_saved_ds(%rip)\n mov %es, %eax\n mov %eax, b_saved_es(%rip)\n"
        "  mov $0x2b, %eax\n mov %eax, %ds\n mov %eax, %es\n"
        "  mov %rdi, %rsp\n"
        "  mov %edx, %eax\n mov %ecx, %ebx\n mov %r8d, %ecx\n mov %r9d, %edx\n"
        "  pushq $0x23\n pushq %rsi\n lretq\n"
        "b_back64:\n"
        "  mov b_saved_rsp(%rip), %rsp\n"
        "  mov %eax, %r12d\n"
        "  mov b_saved_ds(%rip), %eax\n mov %eax, %ds\n mov b_saved_es(%rip), %eax\n mov %eax, %es\n"
        "  mov %r12d, %eax\n"
        "  pop %r15\n pop %r14\n pop %r13\n pop %r12\n pop %rbp\n pop %rbx\n"
        "  ret\n"
        ".balign 8\n"
        "b_saved_rsp: .quad 0\n b_saved_ds: .long 0\n b_saved_es: .long 0\n"
        ".code32\n"
        "b_to64:\n"
        "  call 1f\n"
        "1: pop %ecx\n"
        "  add $(b_back64 - 1b), %ecx\n"
        "  pushl $0x33\n pushl %ecx\n lret\n"
        /* int $0x80 from 32-bit code: eax = nr, ebx/ecx/edx = arguments. */
        "b_f_int80:\n  int $0x80\n  jmp b_to64\n"
        /* The CS this code runs with. */
        "b_f_cs:\n  xor %eax, %eax\n  mov %cs, %ax\n  jmp b_to64\n"
        /* A fault in 32-bit code: the SIGILL handler steps over the ud2 (2 bytes); then the CS again. */
        "b_f_ud2:\n  ud2\n  xor %eax, %eax\n  mov %cs, %ax\n  jmp b_to64\n"
        /* Spins until the counter at ebx changes (eax 1) or ecx runs out (eax 0). */
        "b_f_spin:\n  mov (%ebx), %edx\n"
        "2: cmp (%ebx), %edx\n  jne 3f\n  pause\n  dec %ecx\n  jnz 2b\n  xor %eax, %eax\n  jmp b_to64\n"
        "3: mov $1, %eax\n  jmp b_to64\n"
        /* eax = a selector loaded into DS, ebx = an offset: the dword at ds:ebx, DS back to 0x2b. */
        "b_f_ds:\n  mov %eax, %ds\n  mov (%ebx), %eax\n  mov $0x2b, %ecx\n  mov %ecx, %ds\n  jmp b_to64\n"
        /* The same through GS (Wine's TLS selector), GS back to null. */
        "b_f_gs:\n  mov %eax, %gs\n  mov %gs:(%ebx), %eax\n  xor %ecx, %ecx\n  mov %ecx, %gs\n  jmp b_to64\n"
        "wow_blob_end:\n"
        ".code64\n"
        ".popsection\n");
extern const unsigned char wow_blob_start[], wow_blob_end[], b_enter64[], b_f_int80[], b_f_cs[], b_f_ud2[], b_f_spin[],
    b_f_ds[], b_f_gs[];

/* The copied code (low code page, read/write/execute), its stack (16 pages, room for a signal frame) and a low
 * data page that LDT/TLS segments name. */
static unsigned char *code32, *stack32, *data32;
#define STACK32_SIZE (16 * 4096)

static int setup32(void) {
    code32 = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS | MAP_32BIT, -1, 0);
    stack32 = mmap(NULL, STACK32_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_32BIT, -1, 0);
    data32 = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_32BIT, -1, 0);
    if (code32 == MAP_FAILED || stack32 == MAP_FAILED || data32 == MAP_FAILED) {
        printf("MAP_32BIT for 32-bit code failed: -%s\n", ename(errno));
        return 0;
    }
    memcpy(code32, wow_blob_start, wow_blob_end - wow_blob_start);
    return 1;
}

/* Runs the 32-bit routine `fn` (one of the b_f_* labels) with eax/ebx/ecx/edx; its eax. */
static unsigned int run32(const unsigned char *fn, unsigned int eax, unsigned int ebx, unsigned int ecx, unsigned int edx) {
    typedef unsigned long (*enter_t)(unsigned long, unsigned long, unsigned long, unsigned long, unsigned long, unsigned long);
    enter_t enter = (enter_t)(code32 + (b_enter64 - wow_blob_start));
    unsigned long entry = (unsigned long)(code32 + (fn - wow_blob_start));
    return (unsigned int)enter((unsigned long)stack32 + STACK32_SIZE - 64, entry, eax, ebx, ecx, edx);
}

static int in_code32(unsigned long ip) {
    return ip >= (unsigned long)code32 && ip < (unsigned long)code32 + 4096;
}

static volatile int frame_cs, frame_ss, frame_in32;

static void on_ill32(int sig, siginfo_t *si, void *ucv) {
    (void)sig, (void)si;
    ucontext_t *uc = ucv;
    unsigned long csgsfs = uc->uc_mcontext.gregs[REG_CSGSFS];
    frame_cs = csgsfs & 0xffff;
    frame_ss = (csgsfs >> 48) & 0xffff;
    frame_in32 = in_code32(uc->uc_mcontext.gregs[REG_RIP]);
    uc->uc_mcontext.gregs[REG_RIP] += 2; /* over the ud2 */
}

static volatile int alarm32, alarm32_cs;

static void on_alarm32(int sig, siginfo_t *si, void *ucv) {
    (void)sig, (void)si;
    ucontext_t *uc = ucv;
    if (in_code32(uc->uc_mcontext.gregs[REG_RIP])) {
        alarm32_cs = uc->uc_mcontext.gregs[REG_CSGSFS] & 0xffff;
        ((volatile unsigned int *)data32)[0]++;
    }
    alarm32++;
}

static void code32_mode(void) {
    if (!setup32())
        return;
    printf("cs in 32-bit code = %#x\n", run32(b_f_cs, 0, 0, 0, 0));

    /* int 0x80 from 32-bit code: set_thread_area (entry -1) and get it back; an unknown number. */
    *ud = data_seg(-1);
    ud->base_addr = (unsigned int)(unsigned long)data32;
    show32("32-bit int80 set_thread_area", (int)run32(b_f_int80, NR32_set_thread_area, (unsigned int)(unsigned long)ud, 0, 0));
    int slot = ud->entry_number;
    yes("32-bit int80 entry is a TLS slot", slot >= 12 && slot <= 14);
    show32("32-bit int80 nr 1000", (int)run32(b_f_int80, 1000, 0, 0, 0));

    /* The TLS selector in GS reads the low data page (Wine's fs/gs TLS). */
    ((unsigned int *)data32)[4] = 0x5eed1234;
    yes("TLS selector reads its segment", run32(b_f_gs, slot * 8 + 3, 16, 0, 0) == 0x5eed1234);

    /* An LDT data segment over the data page, used through DS. */
    struct user_desc d = data_seg(0);
    d.base_addr = (unsigned int)(unsigned long)data32;
    d.limit = 0xfff;
    d.limit_in_pages = 0;
    show("modify_ldt write data segment", syscall(SYS_modify_ldt, 0x11, &d, sizeof d));
    ((unsigned int *)data32)[8] = 0xfeedf00d;
    yes("LDT selector 0x7 reads its segment", run32(b_f_ds, 0x7, 32, 0, 0) == 0xfeedf00d);

    /* A fault in 32-bit code: the frame's CS is 0x23, and sigreturn goes back into 32-bit code. */
    struct sigaction sa = {0}, old;
    sa.sa_sigaction = on_ill32;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGILL, &sa, &old);
    unsigned int cs_after = run32(b_f_ud2, 0, 0, 0, 0);
    sigaction(SIGILL, &old, NULL);
    yes("fault frame rip is in the 32-bit code", frame_in32);
    printf("fault frame cs = %#x\n", frame_cs);
    printf("fault frame ss = %#x\n", frame_ss);
    printf("cs after sigreturn = %#x\n", cs_after);

    /* A timer signal while 32-bit code spins: its frame has CS 0x23, the code resumes and sees the count. */
    sa.sa_sigaction = on_alarm32;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigaction(SIGALRM, &sa, &old);
    struct itimerval it = { { 0, 1000 }, { 0, 1000 } }, off = {0};
    setitimer(ITIMER_REAL, &it, 0);
    unsigned int seen = run32(b_f_spin, 0, (unsigned int)(unsigned long)data32, 0xffffffffu, 0);
    setitimer(ITIMER_REAL, &off, 0);
    sigaction(SIGALRM, &old, NULL);
    yes("timer signal reached 32-bit code", seen == 1);
    printf("timer frame cs = %#x\n", alarm32_cs);
}

/* ---- per thread and per process --------------------------------------------------------------------- */

struct tls_check {
    unsigned char *area; /* low memory this thread's TLS segment covers */
    unsigned int marker;
    int own_only;
    int base_back;
};

static void *tls_thread(void *arg) {
    struct tls_check *c = arg;
    /* Its own user_desc in its own low area (int 0x80 truncates the pointer). */
    struct user_desc *u = (struct user_desc *)(c->area + 64);
    *u = data_seg(12);
    u->base_addr = (unsigned int)(unsigned long)c->area;
    ((volatile unsigned int *)c->area)[0] = c->marker;
    if (int80(NR32_set_thread_area, (unsigned long)u, 0, 0) != 0)
        return NULL;
    unsigned short sel = 12 * 8 + 3;
    __asm__ volatile("mov %0, %%gs" : : "r"(sel));
    c->own_only = 1;
    for (int i = 0; i < 200; i++) {
        sched_yield();
        unsigned int v;
        __asm__ volatile("movl %%gs:0, %0" : "=r"(v));
        if (v != c->marker)
            c->own_only = 0;
    }
    memset(u, 0, sizeof *u);
    u->entry_number = 12;
    int80(NR32_get_thread_area, (unsigned long)u, 0, 0);
    c->base_back = u->base_addr == (unsigned int)(unsigned long)c->area;
    sel = 0;
    __asm__ volatile("mov %0, %%gs" : : "r"(sel));
    return NULL;
}

static void *ldt_thread(void *arg) {
    (void)arg;
    struct user_desc d = data_seg(2);
    syscall(SYS_modify_ldt, 0x11, &d, sizeof d);
    return NULL;
}

static void threads_mode(void) {
    struct tls_check a = { (unsigned char *)low + 1024, 0xaaaa0001, 0, 0 };
    struct tls_check b = { (unsigned char *)low + 2048, 0xbbbb0002, 0, 0 };
    pthread_t ta, tb, tl;
    pthread_create(&ta, NULL, tls_thread, &a);
    pthread_create(&tb, NULL, tls_thread, &b);
    pthread_join(ta, NULL);
    pthread_join(tb, NULL);
    yes("thread A saw only its own TLS segment", a.own_only);
    yes("thread B saw only its own TLS segment", b.own_only);
    yes("thread A's slot has its base", a.base_back);
    yes("thread B's slot has its base", b.base_back);
    unsigned short gs;
    __asm__ volatile("mov %%gs, %0" : "=r"(gs));
    yes("main thread gs still null", gs == 0);
    memset(ud, 0, sizeof *ud);
    ud->entry_number = 12;
    int80(NR32_get_thread_area, (unsigned long)ud, 0, 0);
    yes("main thread's slot 12 untouched", ud->base_addr == 0 && ud->seg_not_present == 1);

    pthread_create(&tl, NULL, ldt_thread, NULL);
    pthread_join(tl, NULL);
    unsigned char buf[24] = {0};
    long n = syscall(SYS_modify_ldt, 0, buf, sizeof buf);
    show("ldt bytes the thread's write left", n);
    yes("thread's LDT entry 2 seen by main", n == 24 && (buf[21] & 0x80));
}

static void fork_mode(void) {
    struct user_desc d = data_seg(0);
    d.base_addr = 0x10000;
    syscall(SYS_modify_ldt, 0x11, &d, sizeof d);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        unsigned char buf[16] = {0};
        long n = syscall(SYS_modify_ldt, 0, buf, 16);
        unsigned int base = buf[2] | buf[3] << 8 | buf[4] << 16 | (unsigned)buf[7] << 24;
        printf("child: inherited entry 0 %s\n", n == 16 && base == 0x10000 ? "yes" : "no");
        struct user_desc e = data_seg(0);
        e.base_addr = 0x20000;
        int ok = syscall(SYS_modify_ldt, 0x11, &e, sizeof e) == 0 && syscall(SYS_modify_ldt, 0, buf, 16) == 16;
        base = buf[2] | buf[3] << 8 | buf[4] << 16 | (unsigned)buf[7] << 24;
        printf("child: rewrote its own entry 0 %s\n", ok && base == 0x20000 ? "yes" : "no");
        fflush(stdout);
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    unsigned char buf[16] = {0};
    long n = syscall(SYS_modify_ldt, 0, buf, 16);
    unsigned int base = buf[2] | buf[3] << 8 | buf[4] << 16 | (unsigned)buf[7] << 24;
    show("parent ldt read after the child", n);
    yes("parent's entry 0 kept its base", base == 0x10000);
}

static void exec_mode(char *self) {
    struct user_desc d = data_seg(0);
    syscall(SYS_modify_ldt, 0x11, &d, sizeof d);
    *ud = data_seg(13);
    int80(NR32_set_thread_area, (unsigned long)ud, 0, 0);
    unsigned short sel = 0x7;
    __asm__ volatile("mov %0, %%es" : : "r"(sel));
    printf("before exec: ldt and tls slot 13 set\n");
    fflush(stdout);
    char *av[] = { self, "exec-child", NULL };
    execve("/proc/self/exe", av, environ);
    printf("execve failed: -%s\n", ename(errno));
}

static void exec_child(void) {
    unsigned char buf[16];
    show("after exec: modify_ldt read", syscall(SYS_modify_ldt, 0, buf, sizeof buf));
    for (int i = 12; i <= 14; i++) {
        memset(ud, 0, sizeof *ud);
        ud->entry_number = i;
        int80(NR32_get_thread_area, (unsigned long)ud, 0, 0);
        printf("after exec: tls slot %d empty %s\n", i, ud->base_addr == 0 && ud->limit == 0 && ud->seg_not_present ? "yes" : "no");
    }
    unsigned short ds, es, fs, gs;
    __asm__ volatile("mov %%ds, %0; mov %%es, %1; mov %%fs, %2; mov %%gs, %3" : "=r"(ds), "=r"(es), "=r"(fs), "=r"(gs));
    printf("after exec: ds %#x es %#x fs %#x gs %#x\n", ds, es, fs, gs);
}

/* ---- hostile int 0x80 ------------------------------------------------------------------------------- */

static long int80_raw(unsigned long rax, unsigned long rbx, unsigned long rcx, unsigned long rdx) {
    long r;
    __asm__ volatile("int $0x80" : "=a"(r) : "a"(rax), "b"(rbx), "c"(rcx), "d"(rdx) : "memory");
    return r;
}

static void hostile_mode(void) {
    /* Only eax is the number and only the low halves are arguments (Linux zero-extends them). */
    *ud = data_seg(-1);
    show32("int80 dirty high rax set_thread_area", int80_raw(0xdead000000000000ul | NR32_set_thread_area, (unsigned long)ud, 0, 0));
    int slot = ud->entry_number;
    memset(ud, 0, sizeof *ud);
    ud->entry_number = slot;
    show32("int80 dirty high rbx get_thread_area", int80_raw(NR32_get_thread_area, 0xbeef000000000000ul | (unsigned long)ud, 0, 0));
    yes("dirty high rbx read the low buffer", ud->limit == 0xfffff);
    show32("int80 nr -1", int80_raw(0xfffffffful, 0, 0, 0));
    show32("int80 nr 0x7fffffff", int80_raw(0x7ffffffful, 0, 0, 0));
    show32("int80 nr 0x80000000", int80_raw(0x80000000ul, 0, 0, 0));
    /* Bad pointers and sizes. */
    show32("int80 modify_ldt write EFAULT", int80_raw(NR32_modify_ldt, 1, 0xffff000, 16));
    show32("int80 modify_ldt read EFAULT", int80_raw(NR32_modify_ldt, 0, 0xffff000, 16));
    show32("int80 get_thread_area EFAULT", int80_raw(NR32_get_thread_area, 0xffff000, 0, 0));
    show("modify_ldt write EFAULT", syscall(SYS_modify_ldt, 0x11, (void *)16, 16));
    show("modify_ldt read EFAULT", syscall(SYS_modify_ldt, 0, (void *)16, 16));
    *ud = data_seg(11);
    show32("int80 get_thread_area entry 11", int80_raw(NR32_get_thread_area, (unsigned long)ud, 0, 0));
    *ud = data_seg(15);
    show32("int80 get_thread_area entry 15", int80_raw(NR32_get_thread_area, (unsigned long)ud, 0, 0));
    /* A 32-bit code segment with a conforming bit is refused (contents 3 not present is a hole). */
    struct user_desc d = data_seg(3);
    d.contents = 3;
    show("modify_ldt write conforming code", syscall(SYS_modify_ldt, 0x11, &d, sizeof d));
    /* An LDT selector whose descriptor is not present faults (#NP) when loaded. */
    struct user_desc np = data_seg(4);
    np.seg_not_present = 1;
    show("modify_ldt write not present", syscall(SYS_modify_ldt, 0x11, &np, sizeof np));
    struct sigaction sa = {0}, old_segv, old_bus;
    sa.sa_sigaction = onseg;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, &old_segv);
    sigaction(SIGBUS, &sa, &old_bus);
    caught = 0;
    if (sigsetjmp(jb, 1) == 0) {
        unsigned short sel = 4 * 8 + 7;
        __asm__ volatile("mov %0, %%gs" : : "r"(sel));
    }
    printf("loading a not-present LDT selector: %s\n", !caught ? "no fault" : caught_signo == SIGBUS ? "SIGBUS" : caught_signo == SIGSEGV ? "SIGSEGV" : "other");
    caught = 0;
    if (sigsetjmp(jb, 1) == 0) {
        unsigned short sel = 100 * 8 + 7; /* beyond the LDT's entries */
        __asm__ volatile("mov %0, %%gs" : : "r"(sel));
    }
    printf("loading an LDT selector beyond its end: %s\n", !caught ? "no fault" : caught_signo == SIGBUS ? "SIGBUS" : caught_signo == SIGSEGV ? "SIGSEGV" : "other");
    sigaction(SIGSEGV, &old_segv, NULL);
    sigaction(SIGBUS, &old_bus, NULL);
    unsigned short z = 0;
    __asm__ volatile("mov %0, %%gs" : : "r"(z));
}


/* Two processes on one CPU, only the child with an LDT: while they switch back and forth, an LDT selector
 * must fault in the parent every time (its LDTR is null; the child's table never leaks into it). */
static void ldtr_mode(void) {
    cpu_set_t set;
    CPU_ZERO(&set);
    int cpu = sched_getcpu();
    CPU_SET(cpu < 0 ? 0 : cpu, &set);
    yes("pinned to one CPU", sched_setaffinity(0, sizeof set, &set) == 0);
    int pfd[2];
    if (pipe(pfd) != 0)
        return;
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        struct user_desc d = data_seg(0);
        int ok = syscall(SYS_modify_ldt, 0x11, &d, sizeof d) == 0;
        unsigned short sel = 0x7;
        __asm__ volatile("mov %0, %%gs" : : "r"(sel));
        if (write(pfd[1], ok ? "y" : "n", 1) != 1)
            _exit(1);
        for (int i = 0; i < 2000; i++)
            sched_yield();
        unsigned short gs;
        __asm__ volatile("mov %%gs, %0" : "=r"(gs));
        _exit(gs == 0x7 ? 0 : 2);
    }
    char x = 0;
    if (read(pfd[0], &x, 1) != 1)
        x = 0;
    yes("child made its LDT", x == 'y');
    struct sigaction sa = {0}, old;
    sa.sa_sigaction = onseg;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, &old);
    int faults = 0;
    for (int i = 0; i < 500; i++) {
        caught = 0;
        if (sigsetjmp(jb, 1) == 0) {
            unsigned short sel = 0x7;
            __asm__ volatile("mov %0, %%gs" : : "r"(sel));
        }
        faults += caught;
        sched_yield();
    }
    sigaction(SIGSEGV, &old, NULL);
    unsigned short z = 0;
    __asm__ volatile("mov %0, %%gs" : : "r"(z));
    int st = 0;
    waitpid(c, &st, 0);
    yes("an LDT selector faulted every time in the process without an LDT", faults == 500);
    yes("the child kept its LDT selector across the switches", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

int main(int argc, char **argv) {
    const char *what = argc > 1 ? argv[1] : "all";
    low = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_32BIT, -1, 0);
    if (low == MAP_FAILED) {
        printf("MAP_32BIT failed: -%s\n", ename(errno));
        return 1;
    }
    ud = (struct user_desc *)low;
    if (!strcmp(what, "tls") || !strcmp(what, "all"))
        tls();
    if (!strcmp(what, "ldt") || !strcmp(what, "all"))
        ldt();
    if (!strcmp(what, "enosys") || !strcmp(what, "all"))
        int80_enosys();
    if (!strcmp(what, "selectors") || !strcmp(what, "all"))
        selectors();
    if (!strcmp(what, "code32"))
        code32_mode();
    if (!strcmp(what, "threads"))
        threads_mode();
    if (!strcmp(what, "fork"))
        fork_mode();
    if (!strcmp(what, "hostile"))
        hostile_mode();
    if (!strcmp(what, "ldtr"))
        ldtr_mode();
    if (!strcmp(what, "exec-child"))
        exec_child();
    if (!strcmp(what, "exec"))
        exec_mode(argv[0]);
    printf("done\n");
    return 0;
}
