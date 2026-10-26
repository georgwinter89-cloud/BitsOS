//! IDT and trap stubs (owner: CP; the user-mode return path is AB's `entry.rs`).
//!
//! Every vector has a 16-byte stub at `hal_vec_base + 16 * vector` that pushes a dummy error code where the CPU
//! does not, then the vector, then jumps to `hal_trap_common`, which saves all GPRs into a `TrapFrame`. Traps
//! from user mode go to `entry`'s return path (back to `UserContext::execute`'s caller); traps from kernel
//! mode go to `kernel_trap`: interrupts to `vectors::dispatch`, exceptions to a registered hook or a panic.
//! #DF, NMI and #MC gates switch to their IST stacks (`gdt.rs`). The probes below cause deliberate faults at
//! known instructions so the hook and IST paths can be tested (CP-1 ktests).

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Saved state of a trap, lowest address first (the order the stubs push in reverse).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

pub const PAGE_FAULT: u8 = 14;
pub const GENERAL_PROTECTION: u8 = 13;
pub const DOUBLE_FAULT: u8 = 8;
pub const NMI: u8 = 2;
pub const MACHINE_CHECK: u8 = 18;
pub const SPURIOUS: u8 = 0xFF;

/// A hook for a kernel-mode exception. Returns true if handled (the trap returns to the interrupted code).
pub type ExceptionHook = fn(&mut TrapFrame) -> bool;

static HOOKS: [AtomicUsize; 32] = [const { AtomicUsize::new(0) }; 32];

/// Registers the handler for kernel-mode exception `vector` (< 32), e.g. MM's kernel page-fault handler or
/// AB's user-copy fixups. One hook per vector; the owner chains if needed.
pub fn set_exception_hook(vector: u8, hook: ExceptionHook) {
    if (vector as usize) < HOOKS.len() {
        HOOKS[vector as usize].store(hook as usize, Ordering::Release);
    }
}

/// The hook registered for kernel-mode exception `vector`, if any (for chaining, and for tests that install
/// a hook for a moment and put the old one back).
pub fn exception_hook(vector: u8) -> Option<ExceptionHook> {
    let h = HOOKS.get(vector as usize)?.load(Ordering::Acquire);
    // SAFETY: only `set_exception_hook` stores non-zero values into HOOKS, always a valid `ExceptionHook`.
    (h != 0).then(|| unsafe { core::mem::transmute::<usize, ExceptionHook>(h) })
}

/// Removes the hook of `vector` (the exception panics again).
pub fn clear_exception_hook(vector: u8) {
    if let Some(h) = HOOKS.get(vector as usize) {
        h.store(0, Ordering::Release);
    }
}

pub fn exception_name(v: u64) -> &'static str {
    match v {
        0 => "divide error (#DE)",
        1 => "debug (#DB)",
        2 => "NMI",
        3 => "breakpoint (#BP)",
        4 => "overflow (#OF)",
        5 => "bound range (#BR)",
        6 => "invalid opcode (#UD)",
        7 => "device not available (#NM)",
        8 => "double fault (#DF)",
        10 => "invalid TSS (#TS)",
        11 => "segment not present (#NP)",
        12 => "stack fault (#SS)",
        13 => "general protection (#GP)",
        14 => "page fault (#PF)",
        16 => "x87 FP error (#MF)",
        17 => "alignment check (#AC)",
        18 => "machine check (#MC)",
        19 => "SIMD FP error (#XM)",
        20 => "virtualization (#VE)",
        21 => "control protection (#CP)",
        _ => "interrupt",
    }
}

/// Called by the stubs for traps that came from kernel mode.
#[unsafe(no_mangle)]
extern "C" fn hal_kernel_trap(frame: &mut TrapFrame) {
    let v = frame.vector;
    // First, before anything may use GS: NMI/#DF/#MC ignore IF and can hit a swapgs window (percpu.rs).
    let _gs = matches!(v as u8, NMI | DOUBLE_FAULT | MACHINE_CHECK).then(crate::percpu::paranoid_gs_enter);
    if v >= 32 {
        crate::vectors::dispatch(v as u8, frame);
        // CP-5: the scheduler may preempt the interrupted kernel thread now (after the EOI, depth back to 0).
        crate::vectors::irq_exit_from_kernel(frame);
        return;
    }
    let hook = HOOKS[v as usize].load(Ordering::Acquire);
    if hook != 0 {
        // SAFETY: only `set_exception_hook` stores into HOOKS, always a valid `ExceptionHook`.
        let f: ExceptionHook = unsafe { core::mem::transmute::<usize, ExceptionHook>(hook) };
        if f(frame) {
            return;
        }
    }
    let cr2 = crate::cpu::read_cr2();
    panic!(
        "kernel {} at rip {:#x}, error {:#x}, cr2 {:#x}, rsp {:#x}, rax {:#x} rbx {:#x} rcx {:#x} rdx {:#x} rsi {:#x} rdi {:#x} rbp {:#x}",
        exception_name(v),
        frame.rip,
        frame.error,
        cr2,
        frame.rsp,
        frame.rax,
        frame.rbx,
        frame.rcx,
        frame.rdx,
        frame.rsi,
        frame.rdi,
        frame.rbp
    );
}

#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .text
    .global hal_vec_base
    .p2align 4
hal_vec_base:
    .set vecnum, 0
    .rept 256
    .p2align 4
    .if (vecnum == 8) || (vecnum == 10) || (vecnum == 11) || (vecnum == 12) || (vecnum == 13) || (vecnum == 14) || (vecnum == 17) || (vecnum == 21) || (vecnum == 29) || (vecnum == 30)
    .else
    pushq $0
    .endif
    pushq $vecnum
    jmp hal_trap_common
    .set vecnum, vecnum + 1
    .endr

    .global hal_trap_common
hal_trap_common:
    pushq %rax
    pushq %rbx
    pushq %rcx
    pushq %rdx
    pushq %rsi
    pushq %rdi
    pushq %rbp
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    testq $3, 144(%rsp)
    jnz hal_trap_from_user
    movq %rsp, %rdi
    cld
    call hal_kernel_trap
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

// ---------------------------------------------------------------------------------------------------------
// Deliberate faults for self-tests (CP's ktests; RB's crash tests can use them too). Every probe touches only
// an address hal chooses: a non-canonical one (#GP before any access) or the guard page of an IST stack (#PF).

/// Where the probe load faults, and where a hook resumes it (`frame.rip = resume_rip`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeSite {
    pub fault_rip: u64,
    pub resume_rip: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeTarget {
    /// 0x8000_0000_0000_0000: #GP with error code 0.
    NonCanonical,
    /// The first guard page below IST stack `n` (1-based): #PF, not present, read.
    IstGuard(u8),
}

pub const NON_CANONICAL: u64 = 0x8000_0000_0000_0000;

impl ProbeTarget {
    /// The address the probe loads from; `None` when it would not fault (a boot stack has no guard page).
    pub fn address(self) -> Option<u64> {
        match self {
            ProbeTarget::NonCanonical => Some(NON_CANONICAL),
            ProbeTarget::IstGuard(n) => crate::gdt::ist_stack(n)?.guard,
        }
    }
}

/// Loads 8 bytes from `t`'s address, which faults. A hook on the vector must move `rip` to
/// `probe_site().resume_rip` (and may set `rax`, the returned value); without one the fault is a kernel
/// panic like any other. `None` when the target has no faulting address.
pub fn probe_load(t: ProbeTarget) -> Option<u64> {
    let addr = t.address()?;
    Some(imp::probe_load(addr))
}

pub fn probe_site() -> ProbeSite {
    imp::probe_site()
}

/// `int 2`: enters the NMI gate with its IST switch without a real NMI (the IST2 test). Test-only. Needs a
/// hook on vector 2 that accepts only frames `is_soft_int_frame` recognises (and chains everything else, so a
/// real NMI is never swallowed); the handler returns to the next instruction.
///
/// Limitation: a software `int 2` does not block NMIs. A real NMI arriving while the soft frame is on IST2
/// restarts at the IST2 top and overwrites that frame, and the soft handler's iretq then crashes. Call it only
/// while nothing sends NMIs: hold `apic::pause_nmi_ipis()` (no NMI IPI leaves any CPU meanwhile) and let one
/// sent before settle; RB's NMI watchdog must be quiet too.
pub fn soft_int_nmi() {
    imp::soft_int_nmi()
}

/// `int 18`: enters the #MC gate and its IST3 stack without a machine check (the IST3 test); test-only, hook
/// and limitation as above (a real #MC during it clobbers IST3, but a real #MC is fatal anyway).
pub fn soft_int_machine_check() {
    imp::soft_int_mc()
}

/// Whether `f` is the frame of `soft_int_nmi` (vector 2) or `soft_int_machine_check` (vector 18): the return
/// address is the instruction after the `int` in hal's own stub and the trap came from kernel code. A real
/// NMI or #MC never has that `rip`, so hooks use this to tell the two apart.
pub fn is_soft_int_frame(f: &TrapFrame) -> bool {
    let ret = match f.vector as u8 {
        NMI => imp::soft_int_ret().0,
        MACHINE_CHECK => imp::soft_int_ret().1,
        _ => return false,
    };
    ret != 0 && f.rip == ret && f.cs == crate::gdt::KERNEL_CS as u64
}

/// The IST index (0 = none) gate `vector` of the loaded IDT switches to (bits 32-34 of the gate).
pub fn gate_ist(vector: u8) -> u8 {
    imp::gate_ist(vector)
}

/// Result of `double_fault_selftest`.
#[derive(Clone, Copy, Debug)]
pub struct DoubleFaultReport {
    /// Address of the `TrapFrame` the #DF handler got (it lives on the stack the handler ran on).
    pub frame_addr: u64,
    pub error: u64,
    pub stack: crate::gdt::IstStack,
}

static DF_ARMED: AtomicBool = AtomicBool::new(false);
/// The CPU running the self-test (a #DF on another CPU meanwhile is not the test's).
static DF_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
static DF_FRAME: AtomicU64 = AtomicU64::new(0);
static DF_ERROR: AtomicU64 = AtomicU64::new(0);
static DF_PREV: AtomicUsize = AtomicUsize::new(0);
/// One self-test at a time (it owns the scratch IDT).
static DF_BUSY: AtomicBool = AtomicBool::new(false);

fn df_hook(f: &mut TrapFrame) -> bool {
    let mine = DF_CPU.load(Ordering::Acquire) == crate::percpu::this_cpu();
    if !mine || !DF_ARMED.swap(false, Ordering::AcqRel) {
        let prev = DF_PREV.load(Ordering::Acquire);
        // SAFETY: DF_PREV holds 0 or a value taken from HOOKS (a valid ExceptionHook).
        return prev != 0 && unsafe { core::mem::transmute::<usize, ExceptionHook>(prev) }(f);
    }
    DF_FRAME.store(f as *mut TrapFrame as u64, Ordering::Relaxed);
    DF_ERROR.store(f.error, Ordering::Relaxed);
    imp::scratch_gate_present(GENERAL_PROTECTION, true);
    // CS:RIP of a #DF are undefined (SDM 6.15); SS:RSP are the interrupted ones, so resume the probe there.
    f.rip = imp::probe_site().resume_rip;
    f.cs = crate::gdt::KERNEL_CS as u64;
    f.rax = 0;
    true
}

/// Causes a real double fault and resumes from it: this CPU loads a private copy of the IDT whose #GP gate is
/// marked not present, so the #GP of a probe load becomes #NP during delivery, which the CPU turns into #DF (SDM
/// table 6-5); the other CPUs keep the real IDT. Shows the #DF gate, IST1 and the resume path work. Only under a
/// hypervisor: the SDM (6.15, interrupt 8) calls the state after a #DF undefined and not restartable, and on real
/// hardware a wrong frame means a triple fault and a reset (into Windows on the Mini-PC). The check is here, not
/// only in the caller, so no safe caller can run it on hardware.
pub fn double_fault_selftest() -> Result<DoubleFaultReport, &'static str> {
    if !crate::cpu::live().has("hypervisor") {
        return Err("hardware: resuming from #DF is architecturally undefined; runs under a hypervisor only");
    }
    if DF_BUSY.swap(true, Ordering::AcqRel) {
        return Err("another double-fault self-test runs");
    }
    let _irq = crate::cpu::IrqGuard::new();
    let prev = HOOKS[DOUBLE_FAULT as usize].load(Ordering::Acquire);
    DF_PREV.store(prev, Ordering::Release);
    DF_CPU.store(crate::percpu::this_cpu(), Ordering::Release);
    set_exception_hook(DOUBLE_FAULT, df_hook);
    DF_ARMED.store(true, Ordering::Release);
    imp::load_scratch_without_gp();
    let _ = imp::probe_load(NON_CANONICAL);
    load_this_cpu();
    HOOKS[DOUBLE_FAULT as usize].store(prev, Ordering::Release);
    DF_CPU.store(usize::MAX, Ordering::Release);
    let armed = DF_ARMED.swap(false, Ordering::AcqRel);
    DF_BUSY.store(false, Ordering::Release);
    if armed {
        return Err("no double fault arrived");
    }
    let stack = crate::gdt::ist_stack(crate::gdt::IST_DOUBLE_FAULT).ok_or("no IST1 stack")?;
    Ok(DoubleFaultReport { frame_addr: DF_FRAME.load(Ordering::Relaxed), error: DF_ERROR.load(Ordering::Relaxed), stack })
}

/// `init` was called / has written the IDT.
static INIT_TAKEN: AtomicBool = AtomicBool::new(false);
static IDT_READY: AtomicBool = AtomicBool::new(false);

/// Builds the IDT and loads it on the BSP. Once: later calls do nothing (the gates never change afterwards).
pub fn init() {
    if INIT_TAKEN.swap(true, Ordering::AcqRel) {
        return;
    }
    imp::init();
    IDT_READY.store(true, Ordering::Release);
}

/// Loads the kernel's IDT on this CPU (every AP at start-up; the BSP's `init` does it once). Does nothing before
/// `init` wrote the table: an all-zero IDT would turn the next exception into a triple fault.
#[cfg_attr(not(target_os = "none"), allow(dead_code))]
pub(crate) fn load_this_cpu() {
    if IDT_READY.load(Ordering::Acquire) {
        imp::load_this_cpu()
    }
}

#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .text
    .p2align 4
    .global hal_probe_load
hal_probe_load:
    .global hal_probe_load_fault
hal_probe_load_fault:
    movq (%rdi), %rax
    .global hal_probe_load_resume
hal_probe_load_resume:
    ret

    .global hal_soft_int_nmi
hal_soft_int_nmi:
    int $2
    .global hal_soft_int_nmi_ret
hal_soft_int_nmi_ret:
    ret

    .global hal_soft_int_mc
hal_soft_int_mc:
    int $18
    .global hal_soft_int_mc_ret
hal_soft_int_mc_ret:
    ret
"#,
    options(att_syntax)
);

#[cfg(target_os = "none")]
mod imp {
    use core::cell::UnsafeCell;

    struct Idt(UnsafeCell<[u64; 512]>);
    // SAFETY: IDT is written by `init` on the BSP before interrupts are enabled and never again; SCRATCH only by
    // the CPU holding DF_BUSY (double-fault self-test, interrupts off).
    unsafe impl Sync for Idt {}
    static IDT: Idt = Idt(UnsafeCell::new([0; 512]));

    unsafe extern "C" {
        static hal_vec_base: u8;
        static hal_probe_load_fault: u8;
        static hal_probe_load_resume: u8;
        fn hal_probe_load(addr: u64) -> u64;
        fn hal_soft_int_nmi();
        fn hal_soft_int_mc();
        static hal_soft_int_nmi_ret: u8;
        static hal_soft_int_mc_ret: u8;
    }

    pub fn soft_int_ret() -> (u64, u64) {
        (&raw const hal_soft_int_nmi_ret as u64, &raw const hal_soft_int_mc_ret as u64)
    }

    pub fn gate_ist(v: u8) -> u8 {
        // SAFETY: an aligned 8-byte read of the IDT's own static; gates change only in `init`.
        let e = unsafe { (IDT.0.get() as *const u64).add(v as usize * 2).read_volatile() };
        ((e >> 32) & 0x7) as u8
    }

    pub fn probe_site() -> super::ProbeSite {
        super::ProbeSite { fault_rip: &raw const hal_probe_load_fault as u64, resume_rip: &raw const hal_probe_load_resume as u64 }
    }

    pub fn probe_load(addr: u64) -> u64 {
        // SAFETY: `addr` comes from `ProbeTarget::address` (non-canonical, or an IST guard page that
        // `install_guarded_ist` verified unmapped), so the load faults without touching memory; the
        // function clobbers only rax (its return value), which the C ABI allows. If nothing resumes the
        // fault, hal_kernel_trap panics as for any kernel fault.
        unsafe { hal_probe_load(addr) }
    }

    pub fn soft_int_nmi() {
        // SAFETY: `int 2` enters the NMI gate (DPL 0, IST2) like an interrupt; the trap stubs save and
        // restore every register, and a hook returning true resumes after the instruction. It does not block
        // NMIs: a real NMI meanwhile would restart at the IST2 top and overwrite this frame, so callers run it
        // only while nothing sends NMIs (documented on the public function; test-only).
        unsafe { hal_soft_int_nmi() }
    }

    pub fn soft_int_mc() {
        // SAFETY: as soft_int_nmi, gate 18 (IST3), which pushes no error code, matching its stub.
        unsafe { hal_soft_int_mc() }
    }

    /// A private IDT for `double_fault_selftest`: a copy of the real one with one gate changed, loaded only on
    /// the CPU running the test (DF_BUSY makes it one at a time).
    static SCRATCH: Idt = Idt(UnsafeCell::new([0; 512]));

    fn lidt(t: &Idt) {
        #[repr(C, packed)]
        struct Ptr {
            limit: u16,
            base: u64,
        }
        let ptr = Ptr { limit: (512 * 8 - 1) as u16, base: t.0.get() as u64 };
        // SAFETY: `t` is one of the two static, fully written IDTs (every gate points at a stub in
        // hal_vec_base); lidt only changes where this CPU looks for gates.
        unsafe { core::arch::asm!("lidt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags)) };
    }

    pub fn load_this_cpu() {
        lidt(&IDT);
    }

    /// Copies the real IDT into SCRATCH with the #GP gate not present and loads it on this CPU.
    pub fn load_scratch_without_gp() {
        // SAFETY: DF_BUSY gives the caller SCRATCH alone and interrupts are off; IDT is only read here (gates
        // never change after `init`).
        unsafe {
            core::ptr::copy_nonoverlapping(IDT.0.get() as *const u64, SCRATCH.0.get() as *mut u64, 512);
        }
        scratch_gate_present(super::GENERAL_PROTECTION, false);
        lidt(&SCRATCH);
    }

    /// Sets or clears the present bit (47) of gate `v` in the scratch IDT.
    pub fn scratch_gate_present(v: u8, present: bool) {
        // SAFETY: one aligned 8-byte store into SCRATCH, which only the self-test's CPU uses (DF_BUSY), with
        // interrupts off; the CPU reads gates only at delivery.
        unsafe {
            let p = (SCRATCH.0.get() as *mut u64).add(v as usize * 2);
            let e = p.read_volatile();
            p.write_volatile(if present { e | 1 << 47 } else { e & !(1 << 47) });
        }
    }

    pub fn init() {
        let base = &raw const hal_vec_base as u64;
        // SAFETY: BSP start-up, interrupts off; nobody reads IDT before lidt below.
        unsafe {
            let idt = &mut *IDT.0.get();
            for v in 0..256u64 {
                let addr = base + v * 16;
                let ist: u64 = match v as u8 {
                    super::NMI => crate::gdt::IST_NMI as u64,
                    super::DOUBLE_FAULT => crate::gdt::IST_DOUBLE_FAULT as u64,
                    super::MACHINE_CHECK => crate::gdt::IST_MACHINE_CHECK as u64,
                    _ => 0,
                };
                // Interrupt gate, present; #BP reachable from ring 3 (int3 becomes SIGTRAP later).
                let attr: u64 = if v == 3 { 0xEE } else { 0x8E };
                idt[(v * 2) as usize] = (addr & 0xFFFF)
                    | ((crate::gdt::KERNEL_CS as u64) << 16)
                    | (ist << 32)
                    | (attr << 40)
                    | (((addr >> 16) & 0xFFFF) << 48);
                idt[(v * 2 + 1) as usize] = addr >> 32;
            }
        }
        lidt(&IDT);
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    pub fn init() {}
    pub fn probe_site() -> super::ProbeSite {
        super::ProbeSite { fault_rip: 0, resume_rip: 0 }
    }
    pub fn probe_load(_addr: u64) -> u64 {
        0
    }
    pub fn soft_int_nmi() {}
    pub fn soft_int_mc() {}
    pub fn soft_int_ret() -> (u64, u64) {
        (0, 0)
    }
    pub fn gate_ist(_v: u8) -> u8 {
        0
    }
    pub fn scratch_gate_present(_v: u8, _present: bool) {}
    pub fn load_scratch_without_gp() {}
    pub fn load_this_cpu() {}
}

