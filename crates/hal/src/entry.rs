//! Entering and leaving user mode (owner: AB). Contract `UserContext::execute`, modelled on Asterinas' OSTD:
//! the kernel thread of a process calls `execute`, which runs user code until it makes a syscall or traps, then
//! returns with the reason and the saved registers. The kernel handles the syscall or fault in ordinary thread
//! context and calls `execute` again. So the kernel side of user mode is a loop, not a tangle of entry paths.
//!
//! AB-1:
//! - **SYSCALL entry**: `swapgs`, the user rsp parked in the CPU block, the kernel stack of the `execute` caller
//!   (`CpuLocal::kernel_rsp`), every general register into the `UserContext` (rcx = return rip, r11 = rflags,
//!   as the CPU leaves them).
//! - **Return to user** by `sysretq` when it gives exactly the registers `iretq` would: rcx == rip, r11 ==
//!   rflags, no TF/RF, rip below `USER_TOP` (Intel raises SYSRET's #GP for a non-canonical rcx in ring 0 with
//!   the user rsp and GS already loaded, CVE-2012-0217). Everything else returns by `iretq`. A rip that is not a
//!   canonical lower-half address never reaches `iretq` either (its #GP would also come in ring 0 after
//!   `swapgs`): `execute` returns `Trap { vector: 13 }` as if the program had faulted there. The same holds
//!   for the rest of the state the return loads in ring 0: a non-canonical rsp (a program may spin with one;
//!   the IRET after an interrupt would fault on it in ring 0 with the user GS live) comes back as the #SS the
//!   program's next stack access would raise, a non-canonical FS/GS base (wrfsbase/wrmsr #GP in ring 0) as #GP.
//!   `execute` checks this itself, so no later writer of the pub fields (sigreturn, ptrace, clone's TLS, MCP)
//!   can turn a bad value into a kernel fault. AMD keeps a null SS's attributes across SYSRET (Linux's
//!   X86_BUG_SYSRET_SS_ATTRS), so the kernel SS is reloaded first when an interrupt left SS null.
//! - **FS/GS base**: CR4.FSGSBASE is on (CP-1), so programs may change both bases with `wrfsbase`/`wrgsbase`.
//!   Every return to the kernel reads both back into `fs_base`/`gs_base` (`rdfsbase`; `swapgs; rdgsbase;
//!   swapgs` for the inactive GS, as Linux's `x86_gsbase_read_cpu_inactive`), every entry writes them. Without
//!   FSGSBASE the MSRs are used (`user_fsgsbase()` is then false and AT_HWCAP2 does not advertise it).
//! - **Interrupts while user code runs** (`execute(true)`): vectors >= 32 go to `vectors::dispatch` on the
//!   kernel stack (interrupts off, kernel GS), then the program resumes; `execute` does not return for them.
//!   After each one the optional `set_user_irq_hook` hook runs in thread context (the preemption point SC-3
//!   wants); `execute_or_stop` returns to its caller when the hook asks for it (or when no hook is installed),
//!   so a loop like proc's can notice a kill while the program never makes a syscall (B4). NMI, #MC and #DF
//!   from user mode run the kernel's handling (`idt::hal_kernel_trap`, on their IST stack) and return to the
//!   program with `iretq` directly: going back through the kernel would leave NMIs blocked until some later
//!   `iretq`.
//!
//! Signal frames (AB-6, note from SC-1): a thread's user FPU state is `sched::Thread::fpu()` (compacted XSAVES
//! format in xsaves mode). The frame on the user stack uses the standard (non-compacted) format, and the image a
//! program hands back in `rt_sigreturn` must be validated (XSTATE_BV within XCR0, MXCSR reserved bits clear) and
//! converted before it goes into the area; SC adds the conversion helpers in `xsave.rs` when AB-6 asks.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

#[cfg(test)]
use crate::percpu;

/// User registers. Field offsets are used by the assembly below (`HAL_UC_*`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GeneralRegs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

/// How often this context went back to user mode by each path, and how many interrupts it took in user mode.
/// Per context, so the hot path touches no shared cache line; `proc` can sum them per process.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntryStats {
    pub sysret: u64,
    pub iret: u64,
    pub interrupts: u64,
    /// Entries refused because rip was not a canonical lower-half address (returned as #GP).
    pub bad_rip: u64,
    /// Entries refused because rsp was not canonical (returned as #SS).
    pub bad_stack: u64,
    /// Entries refused because fs_base or gs_base was not canonical (returned as #GP).
    pub bad_base: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct UserContext {
    pub regs: GeneralRegs,
    /// User FS base: written on every entry, read back on every return (wrfsbase in user code is kept).
    pub fs_base: u64,
    pub trap_vector: u64,
    pub trap_error: u64,
    pub trap_cr2: u64,
    reason: u64,
    /// User GS base, handled like `fs_base` (it waits in IA32_KERNEL_GS_BASE while the kernel runs).
    pub gs_base: u64,
    pub stats: EntryStats,
}

const REASON_SYSCALL: u64 = 1;
const REASON_IRQ: u64 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReturnReason {
    /// `syscall` instruction: number in `regs.rax`, arguments in rdi, rsi, rdx, r10, r8, r9.
    Syscall,
    /// CPU exception while user code ran (interrupts are handled inside `execute`).
    Trap { vector: u8, error: u64, cr2: u64 },
}

/// RFLAGS bits user code may control (CF PF AF ZF SF TF DF OF, RF, AC, ID); IF is set by the kernel.
const USER_RFLAGS: u64 = 0x0000_0000_0025_0DD5;
const RFLAGS_IF: u64 = 1 << 9;
const RFLAGS_TF: u64 = 1 << 8;
const RFLAGS_RF: u64 = 1 << 16;
/// First address that is not canonical in the lower half (4-level paging; LA57 is refused, spec §2.3).
const CANONICAL_LOW_END: u64 = 0x0000_8000_0000_0000;
const VECTOR_SS: u8 = 12;
const VECTOR_GP: u8 = 13;

/// Canonical with 48-bit addresses (bits 63:47 all equal); kernel-half values included.
fn canonical(v: u64) -> bool {
    ((v << 16) as i64 >> 16) as u64 == v
}

/// Whether the entry code uses rdfsbase/wrfsbase/rdgsbase/wrgsbase (CR4.FSGSBASE on every CPU that ran
/// `init_syscall`). Read by the assembly.
#[unsafe(no_mangle)]
static HAL_ENTRY_FSGSBASE: AtomicU8 = AtomicU8::new(0);
/// Whether CR4.SMAP is on (every CPU): the interrupt paths from user mode then `clac`, because a program can
/// enter with RFLAGS.AC set and interrupt delivery keeps it, which would run the handler without SMAP.
#[unsafe(no_mangle)]
static HAL_ENTRY_SMAP: AtomicU8 = AtomicU8::new(0);
static CR4_DECIDED: AtomicBool = AtomicBool::new(false);
/// Set once the first CPU's flags above are stored (`smap_all` reads nothing half-decided).
static FLAGS_READY: AtomicBool = AtomicBool::new(false);

/// Interrupts that arrived while user code ran, all CPUs (statistics; per context in `UserContext::stats`).
static USER_IRQS: AtomicU64 = AtomicU64::new(0);
static USER_IRQ_HOOK: AtomicUsize = AtomicUsize::new(0);

impl UserContext {
    pub fn new(rip: u64, rsp: u64) -> Self {
        let mut c = UserContext::default();
        c.regs.rip = rip;
        c.regs.rsp = rsp;
        c.regs.rflags = 0x2;
        c
    }

    /// Syscall arguments in Linux order.
    pub fn syscall_args(&self) -> [u64; 6] {
        let r = &self.regs;
        [r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9]
    }

    pub fn set_syscall_return(&mut self, value: i64) {
        self.regs.rax = value as u64;
    }

    /// Runs user code until the next syscall or exception. `interrupts_on`: whether user code runs with IF=1;
    /// interrupts that arrive meanwhile are handled here and the program resumes.
    pub fn execute(&mut self, interrupts_on: bool) -> ReturnReason {
        loop {
            if let Some(r) = self.execute_or_stop(interrupts_on) {
                return r;
            }
        }
    }

    /// As `execute`, but after an interrupt in user mode it returns `None` instead of resuming when the hook
    /// of `set_user_irq_hook` returned true, or when no hook is installed: the caller then looks at its own
    /// state (a kill, a pending signal) and calls again to resume the program where it was.
    pub fn execute_or_stop(&mut self, interrupts_on: bool) -> Option<ReturnReason> {
        loop {
            self.regs.rflags = (self.regs.rflags & USER_RFLAGS) | 0x2 | if interrupts_on { RFLAGS_IF } else { 0 };
            if let Some(vector) = self.refuse_entry() {
                // The return to user would fault in ring 0 after swapgs; the program gets it as its own fault.
                self.trap_vector = vector as u64;
                self.trap_error = 0;
                self.trap_cr2 = 0;
                return Some(ReturnReason::Trap { vector, error: 0, cr2: 0 });
            }
            let sysret = self.sysret_possible();
            if sysret {
                self.stats.sysret += 1;
            } else {
                self.stats.iret += 1;
            }
            imp::execute(self, sysret);
            match self.reason {
                REASON_SYSCALL => return Some(ReturnReason::Syscall),
                REASON_IRQ => {
                    self.stats.interrupts += 1;
                    USER_IRQS.fetch_add(1, Ordering::Relaxed);
                    let h = USER_IRQ_HOOK.load(Ordering::Acquire);
                    if h == 0 {
                        return None;
                    }
                    // SAFETY: only `set_user_irq_hook` stores non-zero values, always a valid `fn() -> bool`.
                    let f: fn() -> bool = unsafe { core::mem::transmute::<usize, fn() -> bool>(h) };
                    if f() {
                        return None;
                    }
                }
                _ => return Some(ReturnReason::Trap { vector: self.trap_vector as u8, error: self.trap_error, cr2: self.trap_cr2 }),
            }
        }
    }

    /// The fault the program gets instead of entering, when the return to user would load something that
    /// faults in ring 0: rip outside the canonical lower half (#GP), a non-canonical rsp (#SS, what its next
    /// stack access would raise), a non-canonical FS or GS base (#GP). Counted in `stats`.
    fn refuse_entry(&mut self) -> Option<u8> {
        if self.regs.rip >= CANONICAL_LOW_END {
            self.stats.bad_rip += 1;
            Some(VECTOR_GP)
        } else if !canonical(self.regs.rsp) {
            self.stats.bad_stack += 1;
            Some(VECTOR_SS)
        } else if !canonical(self.fs_base) || !canonical(self.gs_base) {
            self.stats.bad_base += 1;
            Some(VECTOR_GP)
        } else {
            None
        }
    }

    /// SYSRET restores rip from rcx and rflags from r11 and leaves both registers so; it equals IRET only
    /// when they already hold those values (after a plain syscall). TF/RF and a rip in the last user page
    /// (the next instruction could lie in non-canonical space) take the IRET path.
    fn sysret_possible(&self) -> bool {
        let r = &self.regs;
        r.rcx == r.rip && r.r11 == r.rflags && r.rip < crate::USER_TOP && r.rflags & (RFLAGS_TF | RFLAGS_RF) == 0
    }
}

/// Sets up SYSCALL (EFER.SCE, STAR, LSTAR, FMASK) on this CPU and decides whether the entry code may use the
/// FSGSBASE instructions (every CPU must have CR4.FSGSBASE; one without turns them off for all, before any
/// program runs on it). Selector layout: `gdt.rs`.
pub fn init_syscall() {
    let cr4 = crate::cpu::read_cr4();
    let (fsgs, smap) = (cr4 & crate::cpu::CR4_FSGSBASE != 0, cr4 & crate::cpu::CR4_SMAP != 0);
    if !CR4_DECIDED.swap(true, Ordering::AcqRel) {
        HAL_ENTRY_FSGSBASE.store(fsgs as u8, Ordering::Release);
        HAL_ENTRY_SMAP.store(smap as u8, Ordering::Release);
    } else {
        if !fsgs {
            HAL_ENTRY_FSGSBASE.store(0, Ordering::Release);
        }
        if !smap {
            HAL_ENTRY_SMAP.store(0, Ordering::Release);
        }
    }
    FLAGS_READY.store(true, Ordering::Release);
    imp::init_syscall()
}

/// Whether programs may use rdfsbase/wrfsbase/rdgsbase/wrgsbase (AT_HWCAP2 bit 1, HWCAP2_FSGSBASE): the
/// instructions are enabled and the kernel keeps what they write.
pub fn user_fsgsbase() -> bool {
    HAL_ENTRY_FSGSBASE.load(Ordering::Acquire) != 0
}

/// Whether every CPU that ran `init_syscall` has CR4.SMAP (None before the first one did). The single gate for
/// stac/clac outside CP's code: true implies CPUID.SMAP on every CPU that can run a program, so the instructions
/// cannot #UD; a CPU with SMAP on while this is false only makes its fast user copies fail with EFAULT.
pub fn smap_all() -> Option<bool> {
    FLAGS_READY.load(Ordering::Acquire).then(|| HAL_ENTRY_SMAP.load(Ordering::Acquire) != 0)
}

/// Interrupts taken while user code ran (all CPUs, since boot).
pub fn user_interrupts() -> u64 {
    USER_IRQS.load(Ordering::Relaxed)
}

/// A function `execute` calls in thread context after every interrupt that arrived while user code ran,
/// before the program resumes (the scheduler's preemption point at return to user). It returns true when the
/// thread has something to look at before the program goes on (kill, signal): `execute_or_stop` then returns
/// `None` to its caller (`execute` resumes regardless). One hook; `None` removes it.
pub fn set_user_irq_hook(f: Option<fn() -> bool>) {
    USER_IRQ_HOOK.store(f.map_or(0, |f| f as usize), Ordering::Release);
}

/// The hook `set_user_irq_hook` installed (tests put it back after replacing it for a moment).
pub fn user_irq_hook() -> Option<fn() -> bool> {
    let h = USER_IRQ_HOOK.load(Ordering::Acquire);
    // SAFETY: only `set_user_irq_hook` stores non-zero values, always a valid `fn() -> bool`.
    (h != 0).then(|| unsafe { core::mem::transmute::<usize, fn() -> bool>(h) })
}

/// 64 random bits from RDRAND (None without RDRAND or when it fails ten times in a row, Intel's retry
/// advice). For AT_RANDOM and getrandom (AB-2); `proc` falls back to a TSC mix and logs it.
pub fn hw_random() -> Option<u64> {
    if hw_random_imp::present() { hw_random_imp::rdrand() } else { None }
}

/// 64 bits from RDSEED (the entropy source itself, for seeding; FS-2's generator asks it first). None without
/// RDSEED (CPUID.(7,0):EBX bit 18) or when it stays empty for 100 tries with `pause` in between (Intel's
/// advice: RDSEED may run dry under load).
pub fn hw_seed() -> Option<u64> {
    if hw_random_imp::seed_present() { hw_random_imp::rdseed() } else { None }
}

#[cfg(target_os = "none")]
mod hw_random_imp {
    use core::sync::atomic::{AtomicU8, Ordering};
    static RDRAND: AtomicU8 = AtomicU8::new(0);
    pub fn present() -> bool {
        match RDRAND.load(Ordering::Relaxed) {
            0 => {
                let yes = crate::cpu::cpuid(1, 0).ecx & (1 << 30) != 0;
                RDRAND.store(if yes { 1 } else { 2 }, Ordering::Relaxed);
                yes
            }
            v => v == 1,
        }
    }
    pub fn rdrand() -> Option<u64> {
        for _ in 0..10 {
            let v: u64;
            let ok: u8;
            // SAFETY: RDRAND exists (CPUID.1:ECX bit 30, checked by `present`); it only writes the two output
            // registers and the flags.
            unsafe { core::arch::asm!("rdrand {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack)) };
            if ok != 0 {
                return Some(v);
            }
        }
        None
    }
    static RDSEED: AtomicU8 = AtomicU8::new(0);
    pub fn seed_present() -> bool {
        match RDSEED.load(Ordering::Relaxed) {
            0 => {
                let yes = crate::cpu::cpuid(7, 0).ebx & (1 << 18) != 0;
                RDSEED.store(if yes { 1 } else { 2 }, Ordering::Relaxed);
                yes
            }
            v => v == 1,
        }
    }
    pub fn rdseed() -> Option<u64> {
        for _ in 0..100 {
            let v: u64;
            let ok: u8;
            // SAFETY: RDSEED exists (CPUID.(7,0):EBX bit 18, checked by `seed_present`); it only writes the two
            // output registers and the flags.
            unsafe { core::arch::asm!("rdseed {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack)) };
            if ok != 0 {
                return Some(v);
            }
            core::hint::spin_loop();
        }
        None
    }
}

#[cfg(not(target_os = "none"))]
mod hw_random_imp {
    pub fn present() -> bool {
        false
    }
    pub fn rdrand() -> Option<u64> {
        None
    }
    pub fn seed_present() -> bool {
        false
    }
    pub fn rdseed() -> Option<u64> {
        None
    }
}

/// Called by the assembly for an interrupt (vector >= 32) from user mode: kernel GS, kernel stack, IF=0.
#[cfg(target_os = "none")]
#[unsafe(no_mangle)]
extern "C" fn hal_user_interrupt(frame: &mut crate::idt::TrapFrame) {
    crate::vectors::dispatch(frame.vector as u8, frame);
}

#[cfg(target_os = "none")]
mod imp {
    use super::*;
    use crate::msr;

    unsafe extern "C" {
        fn hal_user_execute(ctx: *mut UserContext, sysret: u64);
        fn hal_syscall_entry();
    }

    pub fn execute(ctx: &mut UserContext, sysret: bool) {
        // SAFETY: ctx is a valid exclusive UserContext for the duration of the call; the assembly saves and
        // restores all callee-saved registers and RFLAGS and returns here on the next syscall, user exception or
        // user-mode interrupt, so this behaves like an ordinary function call. GS points at this CPU's CpuLocal
        // (percpu::init_bsp / install_this_cpu). `refuse_entry` checked everything the return loads in ring 0
        // that could fault there: rip is a canonical lower-half address, rsp, fs_base and gs_base are canonical
        // (wrfsbase/wrgsbase/wrmsr and iretq cannot #GP/#SS). `sysret` is true only when SYSRET is equivalent to
        // IRET (`sysret_possible`).
        unsafe { hal_user_execute(ctx as *mut UserContext, sysret as u64) };
    }

    pub fn init_syscall() {
        let efer = msr::read(msr::IA32_EFER);
        msr::write(msr::IA32_EFER, efer | msr::EFER_SCE);
        // STAR: [47:32] kernel CS (SS = +8), [63:48] base for SYSRET (CS = +16 | 3, SS = +8 | 3).
        let star = ((crate::gdt::KERNEL_CS as u64) << 32) | (((crate::gdt::USER_SS as u64 & !3) - 8) << 48);
        msr::write(msr::IA32_STAR, star);
        msr::write(msr::IA32_LSTAR, hal_syscall_entry as *const () as u64);
        // Mask IF, DF, TF, AC, NT on entry.
        msr::write(msr::IA32_FMASK, (1 << 9) | (1 << 10) | (1 << 8) | (1 << 18) | (1 << 14));
    }

    /// Called from the assembly with the kernel rsp to use as rsp0.
    #[unsafe(no_mangle)]
    extern "C" fn hal_set_rsp0(top: u64) {
        crate::gdt::set_rsp0(top);
    }
}

// Offsets: UserContext (HAL_UC_*, checked by the layout test), CpuLocal (8 kernel rsp, 16 user rsp scratch,
// 24 *UserContext), TrapFrame (HAL_TF_*, idt.rs). Selectors: user SS 0x1b, user CS 0x23, kernel SS 0x10.
#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .set HAL_UC_RAX, 0
    .set HAL_UC_RBX, 8
    .set HAL_UC_RCX, 16
    .set HAL_UC_RDX, 24
    .set HAL_UC_RSI, 32
    .set HAL_UC_RDI, 40
    .set HAL_UC_RBP, 48
    .set HAL_UC_RSP, 56
    .set HAL_UC_R8, 64
    .set HAL_UC_R9, 72
    .set HAL_UC_R10, 80
    .set HAL_UC_R11, 88
    .set HAL_UC_R12, 96
    .set HAL_UC_R13, 104
    .set HAL_UC_R14, 112
    .set HAL_UC_R15, 120
    .set HAL_UC_RIP, 128
    .set HAL_UC_RFLAGS, 136
    .set HAL_UC_FSBASE, 144
    .set HAL_UC_VECTOR, 152
    .set HAL_UC_ERROR, 160
    .set HAL_UC_CR2, 168
    .set HAL_UC_REASON, 176
    .set HAL_UC_GSBASE, 184

    .set HAL_TF_R15, 0
    .set HAL_TF_R14, 8
    .set HAL_TF_R13, 16
    .set HAL_TF_R12, 24
    .set HAL_TF_R11, 32
    .set HAL_TF_R10, 40
    .set HAL_TF_R9, 48
    .set HAL_TF_R8, 56
    .set HAL_TF_RBP, 64
    .set HAL_TF_RDI, 72
    .set HAL_TF_RSI, 80
    .set HAL_TF_RDX, 88
    .set HAL_TF_RCX, 96
    .set HAL_TF_RBX, 104
    .set HAL_TF_RAX, 112
    .set HAL_TF_VECTOR, 120
    .set HAL_TF_ERROR, 128
    .set HAL_TF_RIP, 136
    .set HAL_TF_RFLAGS, 152
    .set HAL_TF_RSP, 160

    .text
    # hal_user_execute(ctx: rdi, sysret: rsi)
    .global hal_user_execute
hal_user_execute:
    pushfq
    # No interrupt between here and the return to user: one inside the swapgs window would run with the user
    # GS base. hal_return_to_kernel's popfq gives the caller its IF back.
    cli
    pushq %rbp
    pushq %rbx
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    movq %rdi, %gs:24
    movq %rsp, %gs:8
    movq %rdi, %rbx
    movq %rsi, %r12
    movq %rsp, %rdi
    call hal_set_rsp0
    movq %rbx, %rdi
    cmpb $0, HAL_ENTRY_FSGSBASE(%rip)
    je 1f
    movq HAL_UC_FSBASE(%rdi), %rax
    wrfsbase %rax
    jmp 2f
1:
    # Without FSGSBASE: FS base by MSR; the user GS base waits in KERNEL_GS_BASE for the swapgs below.
    movl $0xC0000100, %ecx
    movq HAL_UC_FSBASE(%rdi), %rax
    movq %rax, %rdx
    shrq $32, %rdx
    wrmsr
    movl $0xC0000102, %ecx
    movq HAL_UC_GSBASE(%rdi), %rax
    movq %rax, %rdx
    shrq $32, %rdx
    wrmsr
2:
    testq %r12, %r12
    jz hal_user_iret

    # SYSRET path. AMD loads only the SS selector, not its attributes: after an interrupt left SS null the
    # program would run on an unusable SS (Linux X86_BUG_SYSRET_SS_ATTRS); reload the kernel SS then.
    movw %ss, %ax
    cmpw $0x10, %ax
    je 3f
    movw $0x10, %ax
    movw %ax, %ss
3:
    movq HAL_UC_GSBASE(%rdi), %rcx
    swapgs
    cmpb $0, HAL_ENTRY_FSGSBASE(%rip)
    je 4f
    wrgsbase %rcx
4:
    movq HAL_UC_RAX(%rdi), %rax
    movq HAL_UC_RBX(%rdi), %rbx
    movq HAL_UC_RDX(%rdi), %rdx
    movq HAL_UC_RSI(%rdi), %rsi
    movq HAL_UC_RBP(%rdi), %rbp
    movq HAL_UC_R8(%rdi), %r8
    movq HAL_UC_R9(%rdi), %r9
    movq HAL_UC_R10(%rdi), %r10
    movq HAL_UC_R12(%rdi), %r12
    movq HAL_UC_R13(%rdi), %r13
    movq HAL_UC_R14(%rdi), %r14
    movq HAL_UC_R15(%rdi), %r15
    movq HAL_UC_RIP(%rdi), %rcx
    movq HAL_UC_RFLAGS(%rdi), %r11
    # From here rsp is the user's; interrupts are off, and NMI/#MC/#DF use their IST stacks.
    movq HAL_UC_RSP(%rdi), %rsp
    movq HAL_UC_RDI(%rdi), %rdi
    sysretq

hal_user_iret:
    pushq $0x1b
    pushq HAL_UC_RSP(%rdi)
    pushq HAL_UC_RFLAGS(%rdi)
    pushq $0x23
    pushq HAL_UC_RIP(%rdi)
    movq HAL_UC_GSBASE(%rdi), %rcx
    swapgs
    cmpb $0, HAL_ENTRY_FSGSBASE(%rip)
    je 5f
    wrgsbase %rcx
5:
    movq HAL_UC_RAX(%rdi), %rax
    movq HAL_UC_RBX(%rdi), %rbx
    movq HAL_UC_RCX(%rdi), %rcx
    movq HAL_UC_RDX(%rdi), %rdx
    movq HAL_UC_RSI(%rdi), %rsi
    movq HAL_UC_RBP(%rdi), %rbp
    movq HAL_UC_R8(%rdi), %r8
    movq HAL_UC_R9(%rdi), %r9
    movq HAL_UC_R10(%rdi), %r10
    movq HAL_UC_R11(%rdi), %r11
    movq HAL_UC_R12(%rdi), %r12
    movq HAL_UC_R13(%rdi), %r13
    movq HAL_UC_R14(%rdi), %r14
    movq HAL_UC_R15(%rdi), %r15
    movq HAL_UC_RDI(%rdi), %rdi
    iretq

    # rdi = UserContext, kernel GS, IF=0. Reads the user FS/GS base back, then returns from hal_user_execute.
hal_return_to_kernel:
    cmpb $0, HAL_ENTRY_FSGSBASE(%rip)
    je 6f
    rdfsbase %rax
    movq %rax, HAL_UC_FSBASE(%rdi)
    swapgs
    rdgsbase %rax
    swapgs
    movq %rax, HAL_UC_GSBASE(%rdi)
    jmp 7f
6:
    movl $0xC0000100, %ecx
    rdmsr
    shlq $32, %rdx
    orq %rdx, %rax
    movq %rax, HAL_UC_FSBASE(%rdi)
    movl $0xC0000102, %ecx
    rdmsr
    shlq $32, %rdx
    orq %rdx, %rax
    movq %rax, HAL_UC_GSBASE(%rdi)
7:
    movq %gs:8, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbx
    popq %rbp
    popfq
    ret

    .global hal_syscall_entry
hal_syscall_entry:
    swapgs
    movq %rsp, %gs:16
    # The kernel stack of the execute() caller; the space below its saved rsp is free while user code runs.
    movq %gs:8, %rsp
    pushq %rdi
    movq %gs:24, %rdi
    movq %rax, HAL_UC_RAX(%rdi)
    movq %rbx, HAL_UC_RBX(%rdi)
    movq %rcx, HAL_UC_RCX(%rdi)
    movq %rdx, HAL_UC_RDX(%rdi)
    movq %rsi, HAL_UC_RSI(%rdi)
    popq HAL_UC_RDI(%rdi)
    movq %rbp, HAL_UC_RBP(%rdi)
    movq %r8, HAL_UC_R8(%rdi)
    movq %r9, HAL_UC_R9(%rdi)
    movq %r10, HAL_UC_R10(%rdi)
    movq %r11, HAL_UC_R11(%rdi)
    movq %r12, HAL_UC_R12(%rdi)
    movq %r13, HAL_UC_R13(%rdi)
    movq %r14, HAL_UC_R14(%rdi)
    movq %r15, HAL_UC_R15(%rdi)
    movq %rcx, HAL_UC_RIP(%rdi)
    movq %r11, HAL_UC_RFLAGS(%rdi)
    movq %gs:16, %rax
    movq %rax, HAL_UC_RSP(%rdi)
    movq $1, HAL_UC_REASON(%rdi)
    jmp hal_return_to_kernel

    # From hal_trap_common with a TrapFrame at rsp whose CS is the user's; GS is still the user's.
    .global hal_trap_from_user
hal_trap_from_user:
    cmpq $2, HAL_TF_VECTOR(%rsp)
    je hal_trap_from_user_paranoid
    cmpq $18, HAL_TF_VECTOR(%rsp)
    je hal_trap_from_user_paranoid
    cmpq $8, HAL_TF_VECTOR(%rsp)
    je hal_trap_from_user_paranoid
    # CR2 first (r8 is saved in the frame): an NMI or #MC can come at any instruction, and a #PF inside its
    # handler would overwrite CR2 (Linux saves it around NMIs; CP change request for hal_kernel_trap).
    xorl %r8d, %r8d
    cmpq $14, HAL_TF_VECTOR(%rsp)
    jne 10f
    movq %cr2, %r8
10:
    swapgs
    movq %gs:24, %rax
    movq HAL_TF_R15(%rsp), %rcx
    movq %rcx, HAL_UC_R15(%rax)
    movq HAL_TF_R14(%rsp), %rcx
    movq %rcx, HAL_UC_R14(%rax)
    movq HAL_TF_R13(%rsp), %rcx
    movq %rcx, HAL_UC_R13(%rax)
    movq HAL_TF_R12(%rsp), %rcx
    movq %rcx, HAL_UC_R12(%rax)
    movq HAL_TF_R11(%rsp), %rcx
    movq %rcx, HAL_UC_R11(%rax)
    movq HAL_TF_R10(%rsp), %rcx
    movq %rcx, HAL_UC_R10(%rax)
    movq HAL_TF_R9(%rsp), %rcx
    movq %rcx, HAL_UC_R9(%rax)
    movq HAL_TF_R8(%rsp), %rcx
    movq %rcx, HAL_UC_R8(%rax)
    movq HAL_TF_RBP(%rsp), %rcx
    movq %rcx, HAL_UC_RBP(%rax)
    movq HAL_TF_RDI(%rsp), %rcx
    movq %rcx, HAL_UC_RDI(%rax)
    movq HAL_TF_RSI(%rsp), %rcx
    movq %rcx, HAL_UC_RSI(%rax)
    movq HAL_TF_RDX(%rsp), %rcx
    movq %rcx, HAL_UC_RDX(%rax)
    movq HAL_TF_RCX(%rsp), %rcx
    movq %rcx, HAL_UC_RCX(%rax)
    movq HAL_TF_RBX(%rsp), %rcx
    movq %rcx, HAL_UC_RBX(%rax)
    movq HAL_TF_RAX(%rsp), %rcx
    movq %rcx, HAL_UC_RAX(%rax)
    movq HAL_TF_VECTOR(%rsp), %rcx
    movq %rcx, HAL_UC_VECTOR(%rax)
    movq HAL_TF_ERROR(%rsp), %rcx
    movq %rcx, HAL_UC_ERROR(%rax)
    movq HAL_TF_RIP(%rsp), %rcx
    movq %rcx, HAL_UC_RIP(%rax)
    movq HAL_TF_RFLAGS(%rsp), %rcx
    movq %rcx, HAL_UC_RFLAGS(%rax)
    movq HAL_TF_RSP(%rsp), %rcx
    movq %rcx, HAL_UC_RSP(%rax)
    cmpq $32, HAL_TF_VECTOR(%rsp)
    jb 8f
    # An interrupt: handled here (EOI included), then back to execute(), which resumes the program.
    movq %rax, %rbx
    cmpb $0, HAL_ENTRY_SMAP(%rip)
    je 9f
    clac
9:
    movq %rsp, %rdi
    cld
    call hal_user_interrupt
    movq %rbx, %rdi
    movq $3, HAL_UC_REASON(%rdi)
    jmp hal_return_to_kernel
8:
    movq %r8, HAL_UC_CR2(%rax)
    movq $2, HAL_UC_REASON(%rax)
    movq %rax, %rdi
    jmp hal_return_to_kernel

    # NMI, #MC, #DF from user mode: the kernel's handling on the IST stack, then iretq straight back.
hal_trap_from_user_paranoid:
    swapgs
    cmpb $0, HAL_ENTRY_SMAP(%rip)
    je 1f
    clac
1:
    movq %rsp, %rdi
    cld
    call hal_kernel_trap
    swapgs
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rcx
    popq %rbx
    popq %rax
    addq $16, %rsp
    iretq
"#,
    options(att_syntax)
);

#[cfg(not(target_os = "none"))]
mod imp {
    use super::*;
    pub fn execute(_ctx: &mut UserContext, _sysret: bool) {
        panic!("user mode exists only in the kernel build");
    }
    pub fn init_syscall() {}
}

pub mod ab4_progs;

/// Machine code of AB's ktest programs (`crates/syscall/src/ktests.rs`), assembled here because only the
/// trusted base may name assembly symbols. Position-independent; each ends in `exit_group(code)`, where 0 means
/// every check passed and any other value names the failed check (the numbers in the assembly comments). Inert
/// data in .rodata until a test copies it into a program.
pub mod test_programs {
    /// Bad user pointers in syscalls give -EFAULT, never a kernel fault. Codes 11-23.
    pub fn bad_pointers() -> &'static [u8] {
        imp::bad_pointers()
    }
    /// Unknown syscall numbers give -ENOSYS (4001 three times, -1, an x32 number). Codes 31-35.
    pub fn enosys() -> &'static [u8] {
        imp::enosys()
    }
    /// wrfsbase/wrgsbase values survive syscalls, sleeps and yields, arch_prctl sees and sets them. Codes 41-52.
    pub fn fsgsbase() -> &'static [u8] {
        imp::fsgsbase()
    }
    /// Wave-A calls through the entry: getpid, gettid, set_tid_address, uname, clock_gettime, brk,
    /// mmap/mprotect/munmap, getrandom, writev, poll, getuid, sigaltstack, rt_sigaction. Codes 61-83.
    pub fn wave_a() -> &'static [u8] {
        imp::wave_a()
    }
    /// Endless loop for `execute` tests: sets ten registers to patterns, spins, checks them and the FS/GS base
    /// (expected in r14/r15) and makes a syscall with rax = 0 (all fine) or the number of the failed check.
    pub fn register_loop() -> &'static [u8] {
        imp::register_loop()
    }

    /// AB-2: writes its initial rsp (8 bytes) to stdout and ends with 0; the ktest then reads the stack it was
    /// started with (argv, envp, auxv) from its address space. Code 1.
    pub fn stack_report() -> &'static [u8] {
        imp::stack_report()
    }
    /// AB-2: one user fault chosen by argc: 1 read at 0, 2 ud2, 3 write into its own code, 4 divide by zero,
    /// 5 jump to an unmapped address, 6 touch 4 MiB down the stack (no fault: exits 0), 7 touch 9 MiB down
    /// (below the stack), 8 int3, 9 run code on the stack (NX). Exit 99 for any other argc.
    pub fn faults() -> &'static [u8] {
        imp::faults()
    }
    /// AB-2: sleeps argc x 10 ms, then `exit_group(argc)`; with argc >= 100 it sleeps 10 ms forever.
    pub fn sleep_exit() -> &'static [u8] {
        imp::sleep_exit()
    }
    /// AB-2: wait4 as a parent: -EINVAL for WEXITED (1), -ECHILD for a pid that is no child (2), then three
    /// blocking `wait4(-1, &status, 0, &rusage)` (3: failed; 4: write failed; 6: rusage not zeroed), each
    /// writing pid and status as two quadwords to stdout, then -ECHILD with WNOHANG (5).
    pub fn waiter() -> &'static [u8] {
        imp::waiter()
    }
    /// AB-2: writes argc MiB of 'A' to stdout in writes of up to 64 KiB (for output caps), then exits 0.
    /// Code 1: a write returned 0 or an error.
    pub fn flood() -> &'static [u8] {
        imp::flood()
    }

    #[cfg(target_os = "none")]
    mod imp {
        macro_rules! blob {
            ($f:ident, $start:ident, $end:ident) => {
                pub fn $f() -> &'static [u8] {
                    unsafe extern "C" {
                        static $start: u8;
                        static $end: u8;
                    }
                    // SAFETY: both symbols are labels of the same .rodata block (the global_asm below), end after
                    // start; the bytes between them are constant and live for the whole kernel run.
                    unsafe {
                        let s = &raw const $start;
                        let e = &raw const $end;
                        core::slice::from_raw_parts(s, e as usize - s as usize)
                    }
                }
            };
        }
        blob!(bad_pointers, hal_tp_badptr_start, hal_tp_badptr_end);
        blob!(enosys, hal_tp_enosys_start, hal_tp_enosys_end);
        blob!(fsgsbase, hal_tp_fsgs_start, hal_tp_fsgs_end);
        blob!(wave_a, hal_tp_wavea_start, hal_tp_wavea_end);
        blob!(register_loop, hal_tp_regloop_start, hal_tp_regloop_end);
        blob!(stack_report, hal_tp_stackrep_start, hal_tp_stackrep_end);
        blob!(faults, hal_tp_faults_start, hal_tp_faults_end);
        blob!(sleep_exit, hal_tp_sleepexit_start, hal_tp_sleepexit_end);
        blob!(waiter, hal_tp_waiter_start, hal_tp_waiter_end);
        blob!(flood, hal_tp_flood_start, hal_tp_flood_end);
    }

    #[cfg(not(target_os = "none"))]
    mod imp {
        pub fn bad_pointers() -> &'static [u8] {
            &[]
        }
        pub fn enosys() -> &'static [u8] {
            &[]
        }
        pub fn fsgsbase() -> &'static [u8] {
            &[]
        }
        pub fn wave_a() -> &'static [u8] {
            &[]
        }
        pub fn register_loop() -> &'static [u8] {
            &[]
        }
        pub fn stack_report() -> &'static [u8] {
            &[]
        }
        pub fn faults() -> &'static [u8] {
            &[]
        }
        pub fn sleep_exit() -> &'static [u8] {
            &[]
        }
        pub fn waiter() -> &'static [u8] {
            &[]
        }
        pub fn flood() -> &'static [u8] {
            &[]
        }
    }
}

#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .pushsection .rodata.hal_test_programs, "a"

    .macro HAL_TP_EXPECT code, value
    movl $\code, %r15d
    cmpq $\value, %rax
    jne 9f
    .endm

    .macro HAL_TP_EXIT
    xorl %r15d, %r15d
9:
    movl $231, %eax
    movl %r15d, %edi
    syscall
    ud2
    .endm

    .macro HAL_TP_REG reg, value
    movabsq $\value, %r11
    cmpq %r11, \reg
    jne 8f
    incl %eax
    .endm

    .global hal_tp_badptr_start
    .global hal_tp_badptr_end
hal_tp_badptr_start:
    # rbx = end of the stack mapping (proc's STACK_TOP = USER_TOP since AB-2); nothing is mapped above it.
    movabsq $0x7ffffffff000, %rbx
    # 11: write() from an unmapped low address
    movl $1, %eax
    movl $1, %edi
    movl $0x10, %esi
    movl $5, %edx
    syscall
    HAL_TP_EXPECT 11, -14
    # 12: write() from the kernel half
    movl $1, %eax
    movl $1, %edi
    movabsq $0xffff888000000000, %rsi
    movl $8, %edx
    syscall
    HAL_TP_EXPECT 12, -14
    # 13: write() from a non-canonical address
    movl $1, %eax
    movl $1, %edi
    movabsq $0x8000000000000000, %rsi
    movl $8, %edx
    syscall
    HAL_TP_EXPECT 13, -14
    # 14: clock_gettime() into the program's own read-only code
    movl $228, %eax
    xorl %edi, %edi
    leaq hal_tp_badptr_start(%rip), %rsi
    syscall
    HAL_TP_EXPECT 14, -14
    # 15: clock_gettime() into the kernel half (must not write there)
    movl $228, %eax
    xorl %edi, %edi
    movabsq $0xffff888000001000, %rsi
    syscall
    HAL_TP_EXPECT 15, -14
    # 16: write() from a buffer whose first half is mapped and the rest is not
    movl $1, %eax
    movl $1, %edi
    leaq -4(%rbx), %rsi
    movl $8, %edx
    syscall
    HAL_TP_EXPECT 16, -14
    # 17: uname() into unmapped memory
    movl $63, %eax
    movl $0x10, %edi
    syscall
    HAL_TP_EXPECT 17, -14
    # 18: nanosleep() reading unmapped memory
    movl $35, %eax
    movl $0x10, %edi
    xorl %esi, %esi
    syscall
    HAL_TP_EXPECT 18, -14
    # 19: open() with an unmapped path
    movl $2, %eax
    movl $0x10, %edi
    xorl %esi, %esi
    xorl %edx, %edx
    syscall
    HAL_TP_EXPECT 19, -14
    # 20: open() with a path that runs into unmapped memory before its NUL
    movb $0x61, -3(%rbx)
    movb $0x62, -2(%rbx)
    movb $0x63, -1(%rbx)
    movl $2, %eax
    leaq -3(%rbx), %rdi
    xorl %esi, %esi
    xorl %edx, %edx
    syscall
    HAL_TP_EXPECT 20, -14
    # 21: writev() with an iovec pointing at unmapped memory
    subq $16, %rsp
    movq $0x10, (%rsp)
    movq $4, 8(%rsp)
    movl $20, %eax
    movl $1, %edi
    movq %rsp, %rsi
    movl $1, %edx
    syscall
    addq $16, %rsp
    HAL_TP_EXPECT 21, -14
    # 22: writev() whose iovec array itself is unmapped
    movl $20, %eax
    movl $1, %edi
    movl $0x10, %esi
    movl $1, %edx
    syscall
    HAL_TP_EXPECT 22, -14
    # 23: a good write() still works afterwards
    movl $1, %eax
    movl $1, %edi
    leaq .Lhal_tp_badptr_msg(%rip), %rsi
    movl $(.Lhal_tp_badptr_msg_end - .Lhal_tp_badptr_msg), %edx
    syscall
    HAL_TP_EXPECT 23, (.Lhal_tp_badptr_msg_end - .Lhal_tp_badptr_msg)
    HAL_TP_EXIT
.Lhal_tp_badptr_msg:
    .ascii "ab-1: bad pointers gave EFAULT\n"
.Lhal_tp_badptr_msg_end:
hal_tp_badptr_end:

    .global hal_tp_enosys_start
    .global hal_tp_enosys_end
hal_tp_enosys_start:
    movl $4001, %eax
    syscall
    HAL_TP_EXPECT 31, -38
    movl $4001, %eax
    syscall
    HAL_TP_EXPECT 32, -38
    movl $4001, %eax
    syscall
    HAL_TP_EXPECT 33, -38
    movq $-1, %rax
    syscall
    HAL_TP_EXPECT 34, -38
    # x32 getpid (bit 30): not an x86-64 number
    movl $0x40000027, %eax
    syscall
    HAL_TP_EXPECT 35, -38
    HAL_TP_EXIT
hal_tp_enosys_end:

    .global hal_tp_fsgs_start
    .global hal_tp_fsgs_end
hal_tp_fsgs_start:
    # r12 = FS value, r13 = GS value, both from the pid so two instances differ.
    movl $39, %eax
    syscall
    movq %rax, %r12
    shlq $20, %r12
    movabsq $0x7f0000000000, %rcx
    addq %rcx, %r12
    leaq 0x8000(%r12), %r13
    wrfsbase %r12
    wrgsbase %r13
    subq $32, %rsp
    # 41/42: arch_prctl(ARCH_GET_FS) sees the wrfsbase value
    movl $158, %eax
    movl $0x1003, %edi
    movq %rsp, %rsi
    syscall
    HAL_TP_EXPECT 41, 0
    movl $42, %r15d
    cmpq %r12, (%rsp)
    jne 9f
    # 43/44: arch_prctl(ARCH_GET_GS) sees the wrgsbase value
    movl $158, %eax
    movl $0x1004, %edi
    movq %rsp, %rsi
    syscall
    HAL_TP_EXPECT 43, 0
    movl $44, %r15d
    cmpq %r13, (%rsp)
    jne 9f
    # 45/46: both survive sleeps (other threads and the other instance run meanwhile) and yields
    movl $10, %ebx
1:
    movq $0, (%rsp)
    movq $2000000, 8(%rsp)
    movl $35, %eax
    movq %rsp, %rdi
    xorl %esi, %esi
    syscall
    movl $24, %eax
    syscall
    movl $45, %r15d
    rdfsbase %rcx
    cmpq %r12, %rcx
    jne 9f
    movl $46, %r15d
    rdgsbase %rcx
    cmpq %r13, %rcx
    jne 9f
    decl %ebx
    jnz 1b
    # 47/48: arch_prctl(ARCH_SET_FS) reaches the program
    addq $0x1000, %r12
    movl $158, %eax
    movl $0x1002, %edi
    movq %r12, %rsi
    syscall
    HAL_TP_EXPECT 47, 0
    movl $48, %r15d
    rdfsbase %rcx
    cmpq %r12, %rcx
    jne 9f
    # 49/50: arch_prctl(ARCH_SET_GS) reaches the program
    addq $0x1000, %r13
    movl $158, %eax
    movl $0x1001, %edi
    movq %r13, %rsi
    syscall
    HAL_TP_EXPECT 49, 0
    movl $50, %r15d
    rdgsbase %rcx
    cmpq %r13, %rcx
    jne 9f
    # 51: an FS-relative store uses the base (FS = rsp, so it lands at 8(%rsp))
    wrfsbase %rsp
    movq $0x5a5a, %fs:8
    movl $51, %r15d
    cmpq $0x5a5a, 8(%rsp)
    jne 9f
    # 52: arch_prctl(ARCH_SET_FS) refuses a kernel address (EPERM)
    movl $158, %eax
    movl $0x1002, %edi
    movabsq $0xffff888000000000, %rsi
    syscall
    HAL_TP_EXPECT 52, -1
    HAL_TP_EXIT
hal_tp_fsgs_end:

    .global hal_tp_wavea_start
    .global hal_tp_wavea_end
hal_tp_wavea_start:
    subq $1024, %rsp
    # 61: getpid > 0
    movl $39, %eax
    syscall
    movl $61, %r15d
    testq %rax, %rax
    jle 9f
    movq %rax, %r12
    # 62: gettid == pid (one thread)
    movl $186, %eax
    syscall
    movl $62, %r15d
    cmpq %r12, %rax
    jne 9f
    # 63: set_tid_address returns the tid
    movl $218, %eax
    leaq 1000(%rsp), %rdi
    syscall
    movl $63, %r15d
    cmpq %r12, %rax
    jne 9f
    # 64/65: uname, sysname "Linux"
    movl $63, %eax
    movq %rsp, %rdi
    syscall
    HAL_TP_EXPECT 64, 0
    movl $65, %r15d
    cmpl $0x756e694c, (%rsp)
    jne 9f
    cmpb $0x78, 4(%rsp)
    jne 9f
    # 66/67: clock_gettime(CLOCK_MONOTONIC), nsec below one second
    movl $228, %eax
    movl $1, %edi
    leaq 512(%rsp), %rsi
    syscall
    HAL_TP_EXPECT 66, 0
    movl $67, %r15d
    cmpq $1000000000, 520(%rsp)
    jae 9f
    # 68/69: brk(0) gives the break, brk(+8192) grows it; the new memory is writable
    movl $12, %eax
    xorl %edi, %edi
    syscall
    movl $68, %r15d
    testq %rax, %rax
    jz 9f
    movq %rax, %r13
    movl $12, %eax
    leaq 8192(%r13), %rdi
    syscall
    movl $69, %r15d
    leaq 8192(%r13), %rcx
    cmpq %rcx, %rax
    jne 9f
    movq $0x1234, (%r13)
    movq $0x5678, 8184(%r13)
    # 70: mmap(NULL, 8192, RW, PRIVATE|ANONYMOUS, -1, 0)
    movl $9, %eax
    xorl %edi, %edi
    movl $8192, %esi
    movl $3, %edx
    movl $0x22, %r10d
    movq $-1, %r8
    xorl %r9d, %r9d
    syscall
    movl $70, %r15d
    testq %rax, %rax
    js 9f
    movq %rax, %r14
    movq $0x77, (%r14)
    movq $0x88, 4096(%r14)
    # 71: getrandom 16 bytes
    movl $318, %eax
    leaq 600(%rsp), %rdi
    movl $16, %esi
    xorl %edx, %edx
    syscall
    HAL_TP_EXPECT 71, 16
    # 72: mprotect the second page read-only
    movl $10, %eax
    leaq 4096(%r14), %rdi
    movl $4096, %esi
    movl $1, %edx
    syscall
    HAL_TP_EXPECT 72, 0
    # 73: the kernel does not write into it either (clock_gettime gives EFAULT)
    movl $228, %eax
    xorl %edi, %edi
    leaq 4096(%r14), %rsi
    syscall
    HAL_TP_EXPECT 73, -14
    # 74: the first page still holds its value, then munmap both
    movl $74, %r15d
    cmpq $0x77, (%r14)
    jne 9f
    movl $11, %eax
    movq %r14, %rdi
    movl $8192, %esi
    syscall
    HAL_TP_EXPECT 74, 0
    # 75: writev with two pieces
    leaq .Lhal_tp_wa_msg1(%rip), %rcx
    movq %rcx, 640(%rsp)
    movq $(.Lhal_tp_wa_msg2 - .Lhal_tp_wa_msg1), 648(%rsp)
    leaq .Lhal_tp_wa_msg2(%rip), %rcx
    movq %rcx, 656(%rsp)
    movq $(.Lhal_tp_wa_end - .Lhal_tp_wa_msg2), 664(%rsp)
    movl $20, %eax
    movl $1, %edi
    leaq 640(%rsp), %rsi
    movl $2, %edx
    syscall
    HAL_TP_EXPECT 75, (.Lhal_tp_wa_end - .Lhal_tp_wa_msg1)
    # 76/77: poll(fd 1, POLLOUT) is ready with revents = POLLOUT
    movl $1, 700(%rsp)
    movw $4, 704(%rsp)
    movw $0, 706(%rsp)
    movl $7, %eax
    leaq 700(%rsp), %rdi
    movl $1, %esi
    xorl %edx, %edx
    syscall
    HAL_TP_EXPECT 76, 1
    movl $77, %r15d
    cmpw $4, 706(%rsp)
    jne 9f
    # 78: getuid == 1000
    movl $102, %eax
    syscall
    HAL_TP_EXPECT 78, 1000
    # 79/80: sigaltstack(NULL, &old) reports SS_DISABLE
    movl $131, %eax
    xorl %edi, %edi
    leaq 720(%rsp), %rsi
    syscall
    HAL_TP_EXPECT 79, 0
    movl $80, %r15d
    cmpl $2, 728(%rsp)
    jne 9f
    # 81: rt_sigaction is accepted
    movl $13, %eax
    movl $10, %edi
    xorl %esi, %esi
    xorl %edx, %edx
    movl $8, %r10d
    syscall
    HAL_TP_EXPECT 81, 0
    # 82: brk memory kept its contents through all of the above
    movl $82, %r15d
    cmpq $0x1234, (%r13)
    jne 9f
    cmpq $0x5678, 8184(%r13)
    jne 9f
    # 83: registers the kernel must keep (r12 = pid) survived every syscall
    movl $39, %eax
    syscall
    movl $83, %r15d
    cmpq %r12, %rax
    jne 9f
    HAL_TP_EXIT
.Lhal_tp_wa_msg1:
    .ascii "ab-1: wave-A "
.Lhal_tp_wa_msg2:
    .ascii "calls work\n"
.Lhal_tp_wa_end:
hal_tp_wavea_end:

    .global hal_tp_regloop_start
    .global hal_tp_regloop_end
hal_tp_regloop_start:
    # r14/r15 = the FS/GS base the kernel gave this context.
0:
    movabsq $0x1111111111111111, %rbx
    movabsq $0x2222222222222222, %rbp
    movabsq $0x3333333333333333, %r12
    movabsq $0x4444444444444444, %r13
    movabsq $0x5555555555555555, %r8
    movabsq $0x6666666666666666, %r9
    movabsq $0x7777777777777777, %r10
    movabsq $0x0888888888888888, %rdx
    movabsq $0x0999999999999999, %rsi
    movabsq $0x0aaaaaaaaaaaaaaa, %rdi
    movl $100000, %ecx
1:
    decl %ecx
    jnz 1b
    movl $1, %eax
    HAL_TP_REG %rbx, 0x1111111111111111
    HAL_TP_REG %rbp, 0x2222222222222222
    HAL_TP_REG %r12, 0x3333333333333333
    HAL_TP_REG %r13, 0x4444444444444444
    HAL_TP_REG %r8, 0x5555555555555555
    HAL_TP_REG %r9, 0x6666666666666666
    HAL_TP_REG %r10, 0x7777777777777777
    HAL_TP_REG %rdx, 0x0888888888888888
    HAL_TP_REG %rsi, 0x0999999999999999
    HAL_TP_REG %rdi, 0x0aaaaaaaaaaaaaaa
    rdfsbase %r11
    cmpq %r14, %r11
    jne 8f
    incl %eax
    rdgsbase %r11
    cmpq %r15, %r11
    jne 8f
    xorl %eax, %eax
8:
    syscall
    jmp 0b
hal_tp_regloop_end:

    # ---- AB-2 programs ----

    .global hal_tp_stackrep_start
    .global hal_tp_stackrep_end
hal_tp_stackrep_start:
    movq %rsp, %rbx
    subq $8, %rsp
    movq %rbx, (%rsp)
    movl $1, %eax
    movl $1, %edi
    movq %rsp, %rsi
    movl $8, %edx
    syscall
    HAL_TP_EXPECT 1, 8
    HAL_TP_EXIT
hal_tp_stackrep_end:

    .global hal_tp_faults_start
    .global hal_tp_faults_end
hal_tp_faults_start:
    movq (%rsp), %rax
    cmpq $1, %rax
    je 1f
    cmpq $2, %rax
    je 2f
    cmpq $3, %rax
    je 3f
    cmpq $4, %rax
    je 4f
    cmpq $5, %rax
    je 5f
    cmpq $6, %rax
    je 6f
    cmpq $7, %rax
    je 7f
    cmpq $8, %rax
    je 8f
    cmpq $9, %rax
    je 10f
    movl $99, %r15d
    jmp 9f
1:  # read from address 0
    xorl %ecx, %ecx
    movq (%rcx), %rax
    ud2
2:  # invalid instruction
    ud2
3:  # write into the program's own (read-only) code
    leaq hal_tp_faults_start(%rip), %rcx
    movb $0, (%rcx)
    ud2
4:  # divide by zero
    xorl %ecx, %ecx
    xorl %edx, %edx
    movl $1, %eax
    divl %ecx
    ud2
5:  # jump to an unmapped address
    movl $0x10, %ecx
    jmp *%rcx
6:  # 4 MiB down the 8 MiB stack: filled on the first touch, no fault
    leaq -0x400000(%rsp), %rcx
    movq $0x5a5a, (%rcx)
    movl $61, %r15d
    cmpq $0x5a5a, (%rcx)
    jne 9f
    xorl %r15d, %r15d
    jmp 9f
7:  # 9 MiB down: below the stack mapping
    leaq -0x900000(%rsp), %rcx
    movq $1, (%rcx)
    ud2
8:  # breakpoint
    int3
    ud2
10: # code on the stack (not executable)
    leaq -64(%rsp), %rcx
    movb $0xc3, (%rcx)
    jmp *%rcx
    HAL_TP_EXIT
hal_tp_faults_end:

    .global hal_tp_sleepexit_start
    .global hal_tp_sleepexit_end
hal_tp_sleepexit_start:
    movq (%rsp), %rbx
    subq $16, %rsp
    movq $0, (%rsp)
    cmpq $100, %rbx
    jae .Lhal_tp_se_forever
    imulq $10000000, %rbx, %rax
    movq %rax, 8(%rsp)
    movl $35, %eax
    movq %rsp, %rdi
    xorl %esi, %esi
    syscall
    movl %ebx, %r15d
    jmp 9f
.Lhal_tp_se_forever:
    movq $10000000, 8(%rsp)
    movl $35, %eax
    movq %rsp, %rdi
    xorl %esi, %esi
    syscall
    jmp .Lhal_tp_se_forever
    HAL_TP_EXIT
hal_tp_sleepexit_end:

    .global hal_tp_waiter_start
    .global hal_tp_waiter_end
hal_tp_waiter_start:
    subq $256, %rsp
    # 1: WEXITED is waitid's: EINVAL
    movl $61, %eax
    movq $-1, %rdi
    xorl %esi, %esi
    movl $4, %edx
    xorl %r10d, %r10d
    syscall
    HAL_TP_EXPECT 1, -22
    # 2: a pid that is no child: ECHILD
    movl $61, %eax
    movl $99999, %edi
    xorl %esi, %esi
    xorl %edx, %edx
    xorl %r10d, %r10d
    syscall
    HAL_TP_EXPECT 2, -10
    movl $3, %r12d
.Lhal_tp_waiter_loop:
    movl $0xFFFFFFFF, 16(%rsp)
    movq $-1, 32(%rsp)
    movl $61, %eax
    movq $-1, %rdi
    leaq 16(%rsp), %rsi
    xorl %edx, %edx
    leaq 32(%rsp), %r10
    syscall
    movl $3, %r15d
    testq %rax, %rax
    jle 9f
    movl $6, %r15d
    cmpq $0, 32(%rsp)
    jne 9f
    movq %rax, (%rsp)
    movl 16(%rsp), %ecx
    movq %rcx, 8(%rsp)
    movl $1, %eax
    movl $1, %edi
    movq %rsp, %rsi
    movl $16, %edx
    syscall
    HAL_TP_EXPECT 4, 16
    decl %r12d
    jnz .Lhal_tp_waiter_loop
    # 5: no children left
    movl $61, %eax
    movq $-1, %rdi
    leaq 16(%rsp), %rsi
    movl $1, %edx
    xorl %r10d, %r10d
    syscall
    HAL_TP_EXPECT 5, -10
    HAL_TP_EXIT
hal_tp_waiter_end:

    .global hal_tp_flood_start
    .global hal_tp_flood_end
hal_tp_flood_start:
    # r12 = bytes still to write (argc MiB), r13 = a 64 KiB buffer of 'A' just below the initial rsp.
    movq (%rsp), %r12
    shlq $20, %r12
    leaq -0x10000(%rsp), %r13
    movq %r13, %rdi
    movl $0x41, %eax
    movl $0x10000, %ecx
    cld
    rep stosb
1:
    movl $0x10000, %edx
    cmpq %rdx, %r12
    cmovbq %r12, %rdx
    movl $1, %eax
    movl $1, %edi
    movq %r13, %rsi
    syscall
    movl $1, %r15d
    testq %rax, %rax
    jle 9f
    subq %rax, %r12
    jnz 1b
    HAL_TP_EXIT
hal_tp_flood_end:

    .popsection
"#,
    options(att_syntax)
);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_matches_assembly_offsets() {
        assert_eq!(core::mem::offset_of!(UserContext, regs.rcx), 16);
        assert_eq!(core::mem::offset_of!(UserContext, regs.rdi), 40);
        assert_eq!(core::mem::offset_of!(UserContext, regs.rsp), 56);
        assert_eq!(core::mem::offset_of!(UserContext, regs.r11), 88);
        assert_eq!(core::mem::offset_of!(UserContext, regs.r15), 120);
        assert_eq!(core::mem::offset_of!(UserContext, regs.rip), 128);
        assert_eq!(core::mem::offset_of!(UserContext, regs.rflags), 136);
        assert_eq!(core::mem::offset_of!(UserContext, fs_base), 144);
        assert_eq!(core::mem::offset_of!(UserContext, trap_vector), 152);
        assert_eq!(core::mem::offset_of!(UserContext, trap_error), 160);
        assert_eq!(core::mem::offset_of!(UserContext, trap_cr2), 168);
        assert_eq!(core::mem::offset_of!(UserContext, reason), 176);
        assert_eq!(core::mem::offset_of!(UserContext, gs_base), 184);
        assert_eq!(core::mem::offset_of!(percpu::CpuLocal, kernel_rsp), percpu::CL_KERNEL_RSP);
        assert_eq!(core::mem::offset_of!(percpu::CpuLocal, user_rsp_scratch), percpu::CL_USER_RSP);
        assert_eq!(core::mem::offset_of!(percpu::CpuLocal, user_ctx), percpu::CL_USER_CTX);
        assert_eq!(core::mem::size_of::<crate::idt::TrapFrame>(), 22 * 8);
        assert_eq!(core::mem::offset_of!(crate::idt::TrapFrame, rax), 112);
        assert_eq!(core::mem::offset_of!(crate::idt::TrapFrame, vector), 120);
        assert_eq!(core::mem::offset_of!(crate::idt::TrapFrame, rip), 136);
        assert_eq!(core::mem::offset_of!(crate::idt::TrapFrame, rflags), 152);
        assert_eq!(core::mem::offset_of!(crate::idt::TrapFrame, rsp), 160);
        assert_eq!(crate::gdt::KERNEL_SS, 0x10);
        assert_eq!(crate::gdt::USER_SS, 0x1b);
        assert_eq!(crate::gdt::USER_CS, 0x23);
    }

    fn after_syscall(rip: u64, rflags: u64) -> UserContext {
        let mut c = UserContext::new(rip, 0x7000_0000);
        c.regs.rflags = rflags;
        c.regs.rcx = rip;
        c.regs.r11 = rflags;
        c
    }

    #[test]
    fn sysret_only_when_equal_to_iret() {
        assert!(after_syscall(0x40_1000, 0x246).sysret_possible());
        // rcx or r11 changed (signal delivery, a trap, ptrace): IRET keeps them.
        let mut c = after_syscall(0x40_1000, 0x246);
        c.regs.rcx = 5;
        assert!(!c.sysret_possible());
        let mut c = after_syscall(0x40_1000, 0x246);
        c.regs.r11 = 0x202;
        assert!(!c.sysret_possible());
        // TF and RF take IRET.
        assert!(!after_syscall(0x40_1000, 0x346).sysret_possible());
        assert!(!after_syscall(0x40_1000, 0x10246).sysret_possible());
        // Intel's SYSRET #GP: nothing at or above USER_TOP, canonical or not.
        assert!(!after_syscall(crate::USER_TOP, 0x246).sysret_possible());
        assert!(!after_syscall(CANONICAL_LOW_END, 0x246).sysret_possible());
        assert!(!after_syscall(0xFFFF_8000_0000_0000, 0x246).sysret_possible());
        assert!(after_syscall(crate::USER_TOP - 16, 0x246).sysret_possible());
    }

    /// The host `imp::execute` panics, so every case here proves the state never reaches the return to user.
    #[test]
    fn state_that_faults_in_ring_0_never_enters() {
        let base = UserContext::new(0x40_1000, 0x7FFF_0000);
        let cases: [(fn(&mut UserContext), u8); 7] = [
            (|c| c.regs.rip = CANONICAL_LOW_END, VECTOR_GP),
            (|c| c.regs.rsp = 0x8000_0000_0000_0000, VECTOR_SS),
            (|c| c.regs.rsp = 0x0000_8000_0000_0000, VECTOR_SS),
            (|c| c.fs_base = 0x8000_0000_0000_0000, VECTOR_GP),
            (|c| c.gs_base = 0x0001_0000_0000_0000, VECTOR_GP),
            (|c| c.fs_base = 0xFFFF_7FFF_FFFF_FFFF, VECTOR_GP),
            (|c| c.gs_base = 0x0000_8000_0000_1000, VECTOR_GP),
        ];
        for (i, (set, want)) in cases.iter().enumerate() {
            let mut c = base;
            set(&mut c);
            assert_eq!(c.execute(true), ReturnReason::Trap { vector: *want, error: 0, cr2: 0 }, "case {i}");
            assert_eq!(c.trap_vector, *want as u64);
            assert_eq!(c.stats.sysret + c.stats.iret, 0, "case {i} reached the return to user");
            assert_eq!(c.stats.bad_rip + c.stats.bad_stack + c.stats.bad_base, 1);
        }
    }

    #[test]
    fn canonical_allows_both_halves() {
        assert!(canonical(0));
        assert!(canonical(0x0000_7FFF_FFFF_FFFF));
        assert!(canonical(0xFFFF_8000_0000_0000));
        assert!(canonical(u64::MAX));
        assert!(!canonical(0x0000_8000_0000_0000));
        assert!(!canonical(0xFFFF_7FFF_FFFF_FFFF));
        // Kernel-half bases and stacks are canonical: the program faults on them itself, in ring 3.
        let mut c = UserContext::new(0x40_1000, 0xFFFF_8880_0000_0000);
        c.fs_base = 0xFFFF_FFFF_8000_0000;
        c.gs_base = 0xFFFF_8000_0000_0000;
        assert_eq!(c.refuse_entry(), None);
    }

    #[test]
    fn user_rflags_mask_keeps_privileged_bits_out() {
        assert_eq!(USER_RFLAGS & RFLAGS_IF, 0);
        // IOPL and NT never come from user state.
        assert_eq!(USER_RFLAGS & (3 << 12 | 1 << 14), 0);
    }
}
