//! IDT and trap stubs (owner: CP; the user-mode return path is AB's `entry.rs`).
//!
//! Every vector has a 16-byte stub at `hal_vec_base + 16 * vector` that pushes a dummy error code where the CPU
//! does not, then the vector, then jumps to `hal_trap_common`, which saves all GPRs into a `TrapFrame`. Traps
//! from user mode go to `entry`'s return path (back to `UserContext::execute`'s caller), except NMI, #MC and
//! #DF; those and traps from kernel mode go to `kernel_trap`: interrupts to `vectors::dispatch`, exceptions to a
//! registered hook or a panic. NMI/#MC/#DF never `swapgs`: like Linux's paranoid entry with FSGSBASE
//! (linux-6.6/arch/x86/entry/entry_64.S:915) the GS base is read and replaced by this CPU's block, then written
//! back, whatever CS says (CP-8).
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
    // SAFETY: every store into HOOKS (`set_exception_hook`, `SavedHook`) is 0 or a valid `ExceptionHook`.
    (h != 0).then(|| unsafe { core::mem::transmute::<usize, ExceptionHook>(h) })
}

/// Removes the hook of `vector` (the exception panics again).
pub fn clear_exception_hook(vector: u8) {
    if let Some(h) = HOOKS.get(vector as usize) {
        h.store(0, Ordering::Release);
    }
}

/// The hook a test (or a chaining owner) replaced, kept without a lock: exception hooks run in exception context
/// on any CPU, where the interrupted code may hold any lock.
pub struct SavedHook(AtomicUsize);

impl SavedHook {
    pub const fn new() -> SavedHook {
        SavedHook(AtomicUsize::new(0))
    }

    /// Installs `hook` for `vector` in one swap and keeps the hook it replaced.
    pub fn install(&self, vector: u8, hook: ExceptionHook) {
        if let Some(h) = HOOKS.get(vector as usize) {
            self.0.store(h.swap(hook as usize, Ordering::AcqRel), Ordering::Release);
        }
    }

    /// Passes `frame` to the replaced hook; false when there was none.
    pub fn call(&self, frame: &mut TrapFrame) -> bool {
        let h = self.0.load(Ordering::Acquire);
        // SAFETY: `install` stores only values taken from HOOKS, which holds 0 or a valid `ExceptionHook`.
        h != 0 && unsafe { core::mem::transmute::<usize, ExceptionHook>(h) }(frame)
    }

    /// Puts the replaced hook back if `ours` is still the one installed (a later change by someone else stays).
    pub fn restore(&self, vector: u8, ours: ExceptionHook) -> bool {
        let Some(h) = HOOKS.get(vector as usize) else { return false };
        h.compare_exchange(ours as usize, self.0.load(Ordering::Acquire), Ordering::AcqRel, Ordering::Acquire).is_ok()
    }
}

impl Default for SavedHook {
    fn default() -> SavedHook {
        SavedHook::new()
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

/// Called by the stubs for traps from kernel mode, and for NMI, #MC and #DF from user mode too.
#[unsafe(no_mangle)]
extern "C" fn hal_kernel_trap(frame: &mut TrapFrame) {
    let v = frame.vector;
    // First, before anything may use GS (percpu.rs `paranoid_gs_enter`): NMI/#DF/#MC ignore IF, so they can come
    // from user mode or hit a swapgs window; #NP/#SS/#GP/#PF can be raised by the return to user itself, after the
    // program's GS base is loaded (CP-8: the 27.10. #GP at the NMI's iretq, whose panic path then recursed
    // through the #PF hook on gs:[..] until #DF).
    let foreign = gs_fault_suspect(v, frame.rip);
    let _gs = (matches!(v as u8, NMI | DOUBLE_FAULT | MACHINE_CHECK) || foreign).then(crate::percpu::paranoid_gs_enter);
    // The guard writes the foreign base back on this CPU when it drops, so a hook may run on this path only if it
    // cannot be preempted: the interrupted code had IF off (every return window, the probes). With IF on and a
    // foreign GS base the kernel state is already broken: report at once.
    if foreign && frame.rflags & RFLAGS_IF != 0 {
        user_return_panic(frame, crate::cpu::read_cr2());
    }
    if v == NMI as u64 && frame.cs & 3 != 0 && AMD_SS_TEST.load(Ordering::Relaxed) {
        frame.ss &= !3;
        AMD_SS_FRAMES.fetch_add(1, Ordering::Relaxed);
    }
    if v >= 32 {
        crate::vectors::dispatch(v as u8, frame);
        // CP-5: the scheduler may preempt the interrupted kernel thread now (after the EOI, depth back to 0).
        crate::vectors::irq_exit_from_kernel(frame);
        return;
    }
    let hook = HOOKS[v as usize].load(Ordering::Acquire);
    if hook != 0 {
        // SAFETY: every store into HOOKS (`set_exception_hook`, `SavedHook`) is 0 or a valid `ExceptionHook`.
        let f: ExceptionHook = unsafe { core::mem::transmute::<usize, ExceptionHook>(hook) };
        if f(frame) {
            user_return_fixup(frame);
            return;
        }
    }
    let cr2 = crate::cpu::read_cr2();
    if foreign {
        user_return_panic(frame, cr2);
    }
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

const RFLAGS_IF: u64 = 1 << 9;
/// IOPL, NT and VM: a program cannot set them, so a user frame that has them was not saved by the CPU.
const RFLAGS_NEVER_USER: u64 = 3 << 12 | 1 << 14 | 1 << 17;

static AMD_SS_TEST: AtomicBool = AtomicBool::new(false);
static AMD_SS_FRAMES: AtomicU64 = AtomicU64::new(0);
static USER_FRAMES_FIXED: AtomicU64 = AtomicU64::new(0);

/// User frames the stubs' `iretq` had to repair since boot (`user_return_fixup`). On an AMD CPU with the STAR
/// base 0x10 every NMI that hits a program after `sysretq` counts here (the Mini-PC check of CP-8).
pub fn user_frames_fixed() -> u64 {
    USER_FRAMES_FIXED.load(Ordering::Relaxed)
}

/// Test only (`cp.entry.nmi_user_ss`): while on, every NMI from user mode gets the frame an AMD CPU saves after
/// `sysretq` (SS without RPL 3), which QEMU never builds. One relaxed load on the NMI-from-user path.
pub fn test_amd_sysret_ss(on: bool) {
    AMD_SS_TEST.store(on, Ordering::Release);
}

/// NMI frames `test_amd_sysret_ss` changed since boot.
pub fn test_amd_sysret_ss_frames() -> u64 {
    AMD_SS_FRAMES.load(Ordering::Relaxed)
}

/// The lower-half GS base check: bit 63 once `arm_gs_base_check` found FSGSBASE on every CPU, the low bits count
/// the APs between loading the IDT and the end of their set-up (`ap_setup_begin`/`ap_setup_end`), which may not
/// have CR4.FSGSBASE yet. The check runs only at exactly `GS_CHECK_ARMED`: one load and compare on the #PF path.
static GS_BASE_CHECK: AtomicU64 = AtomicU64::new(0);
const GS_CHECK_ARMED: u64 = 1 << 63;

/// Lets `hal_kernel_trap` read the GS base of a kernel #NP/#SS/#GP/#PF. CP's SMP bring-up calls it when every
/// CPU has run its set-up. Without FSGSBASE on every CPU that ran `entry::init_syscall` it stays off and only
/// the window test is left; calling it early is harmless, since an AP in its set-up holds the check off.
pub fn arm_gs_base_check() {
    if crate::entry::user_fsgsbase() {
        GS_BASE_CHECK.fetch_or(GS_CHECK_ARMED, Ordering::AcqRel);
    } else {
        GS_BASE_CHECK.fetch_and(!GS_CHECK_ARMED, Ordering::AcqRel);
    }
}

/// Whether the lower-half GS base check runs now (`arm_gs_base_check`, no AP in its set-up).
pub fn gs_base_check_armed() -> bool {
    GS_BASE_CHECK.load(Ordering::Acquire) == GS_CHECK_ARMED
}

/// An AP loads the IDT before its CR4 is set: until `ap_setup_end` its `rdgsbase` could #UD. Only the bare-metal AP
/// entry (`smp::hal_ap_main`) calls it.
#[cfg(target_os = "none")]
pub(crate) fn ap_setup_begin() {
    GS_BASE_CHECK.fetch_add(1, Ordering::AcqRel);
}

/// The AP has its CR4 and has run `entry::init_syscall` (or refused and parks itself, running no code that can
/// fault); an AP without FSGSBASE turns the check off for good. An AP parked by INIT in between leaves the count
/// raised: the check then stays off (the safe side) and the window test remains.
pub(crate) fn ap_setup_end() {
    if !crate::entry::user_fsgsbase() {
        GS_BASE_CHECK.fetch_and(!GS_CHECK_ARMED, Ordering::AcqRel);
    }
    GS_BASE_CHECK.fetch_sub(1, Ordering::AcqRel);
}

/// Whether a kernel-mode exception may have come with a GS base that is not this CPU's block (then
/// `paranoid_gs_enter` puts the block in first). Only #NP, #SS, #GP and #PF: raised in a return-to-user window
/// (`in_user_gs_window`) or, with any GS base in the lower half, anywhere (every CPU block is in the kernel half;
/// a GS base there never belongs to the kernel). A few instructions on the kernel #PF path.
#[inline(always)]
fn gs_fault_suspect(v: u64, rip: u64) -> bool {
    if !(11..=14).contains(&v) {
        return false;
    }
    in_user_gs_window(rip) || (GS_BASE_CHECK.load(Ordering::Relaxed) == GS_CHECK_ARMED && (crate::cpu::rdgsbase() as i64) >= 0)
}

/// The kernel instructions that run with the program's GS base and can fault: `hal_user_execute` up to
/// `hal_syscall_entry` (entry.rs: the sysretq and iretq windows and their wrgsbase), and the stubs' `iretq`
/// (`hal_trap_iret`), which returns NMI, #MC and #DF to user mode after their GS base is back.
pub fn in_user_gs_window(rip: u64) -> bool {
    let (start, end, iret) = imp::user_gs_windows();
    rip == iret || (rip >= start && rip < end)
}

/// The return of NMI, #MC or #DF to user mode by the stubs' `iretq` uses the frame the CPU saved. AMD's SYSRET
/// sets SS to `STAR[63:48] + 8` without forcing RPL 3 (Intel and QEMU force it), so a program that came back
/// by `sysretq` runs with SS = 0x18; an NMI saves that, and `iretq` with SS.RPL 0 != CS.RPL 3 raises #GP(0x18)
/// in ring 0 (CP-8, the Mini-PC crashes of 26. and 27.10.). The GDT has two user code segments (64-bit 0x33,
/// 32-bit 0x23 since AB-10a) and one user data segment (gdt.rs), so a user frame goes back with its own code
/// segment if it is one of those (else 0x33), SS 0x2b, and RFLAGS a program can have; IF stays as saved
/// (`execute(false)` runs programs with IF off). Every repaired frame counts (`user_frames_fixed`).
fn user_return_fixup(frame: &mut TrapFrame) {
    if frame.cs & 3 == 0 {
        return;
    }
    let cs = if frame.cs == crate::gdt::USER32_CS as u64 { frame.cs } else { crate::gdt::USER_CS as u64 };
    let ss = crate::gdt::USER_SS as u64;
    let rflags = (frame.rflags & !RFLAGS_NEVER_USER) | 0x2;
    if frame.cs != cs || frame.ss != ss || frame.rflags != rflags {
        frame.cs = cs;
        frame.ss = ss;
        frame.rflags = rflags;
        USER_FRAMES_FIXED.fetch_add(1, Ordering::Relaxed);
    }
}

/// A kernel fault in a return-to-user window or with a lower-half GS base: the GS base is this CPU's again
/// (`paranoid_gs_enter`), so the panic path works; for the stubs' `iretq` the frame it tried shows too.
#[cold]
fn user_return_panic(frame: &TrapFrame, cr2: u64) -> ! {
    let (_, _, iret) = imp::user_gs_windows();
    let w = |i: u64| crate::panic::read_kernel_u64(frame.rsp.wrapping_add(8 * i)).unwrap_or(u64::MAX);
    let at = if frame.rip == iret { "the stubs' iretq" } else if in_user_gs_window(frame.rip) { "entry.rs's return to user mode" } else { "kernel code with a lower-half GS base" };
    let tried = if frame.rip == iret { [w(0), w(1), w(2), w(3), w(4)] } else { [u64::MAX; 5] };
    panic!(
        "kernel {} in {at} at rip {:#x}, error {:#x}, cr2 {cr2:#x}, rsp {:#x}; iret frame rip {:#x} cs {:#x} rflags {:#x} rsp {:#x} ss {:#x} \
         (the GS base was fixed for this report)",
        exception_name(frame.vector),
        frame.rip,
        frame.error,
        frame.rsp,
        tried[0],
        tried[1],
        tried[2],
        tried[3],
        tried[4]
    )
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
    jz 1f
    # From user mode: NMI, #MC and #DF stay here (paranoid: hal_kernel_trap takes the GS base from the APIC ID,
    # never from CS, and the hardware frame goes back by the iretq below); everything else to entry.rs.
    cmpq $2, 120(%rsp)
    je 2f
    cmpq $18, 120(%rsp)
    je 2f
    cmpq $8, 120(%rsp)
    jne hal_trap_from_user
2:
    # A program can enter with RFLAGS.AC set, and interrupt delivery keeps it (SMAP would be off).
    cmpb $0, HAL_ENTRY_SMAP(%rip)
    je 1f
    clac
1:
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
    .global hal_trap_iret
hal_trap_iret:
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

/// `probe_load(NonCanonical)` (#GP) while the GS base is `gs`, a lower-half value as a program may set it.
/// Returns the loaded value and the GS base the resumed code found (CP-8: the hook must run with this CPU's
/// block, the interrupted code must get `gs` back). Checks its own conditions and gives None without them:
/// FSGSBASE (`entry::user_fsgsbase`), the lower-half check armed (`arm_gs_base_check`; outside the return
/// windows only that check catches the fault) and `gs` canonical in the lower half (a kernel-half base there
/// would let the hook chain run on other memory; that case is `probe_iret_with_foreign_gs`). Interrupts are
/// off while `gs` is loaded.
pub fn probe_load_with_gs(gs: u64) -> Option<(u64, u64)> {
    if !crate::entry::user_fsgsbase() || !gs_base_check_armed() || gs >= 1 << 47 {
        return None;
    }
    let _irq = crate::cpu::IrqGuard::new();
    Some(imp::probe_load_gs(NON_CANONICAL, gs))
}

/// What a kernel-half foreign GS base points at in `probe_iret_with_foreign_gs`: memory of nobody, so even a
/// check that failed would only let the hook chain touch this.
static DECOY_GS: [AtomicU64; 512] = [const { AtomicU64::new(0) }; 512];

/// The GS base `probe_iret_with_foreign_gs` loads.
pub fn probe_iret_decoy() -> u64 {
    DECOY_GS.as_ptr() as u64
}

/// The 27.10. fault at kernel level: the stubs' `iretq` (`hal_trap_iret`) with a frame whose SS is the user data
/// selector with RPL 0 (#GP 0x18), while the GS base is a kernel-half value that is not this CPU's block
/// (`probe_iret_decoy`), which only the window test of `gs_fault_suspect` catches. The #GP hook must call
/// `probe_iret_repair` and return its answer; the `iretq` then runs again. Returns the GS base the resumed code
/// found (the decoy again). None without FSGSBASE. Interrupts are off while the decoy is loaded.
pub fn probe_iret_with_foreign_gs() -> Option<u64> {
    if !crate::entry::user_fsgsbase() {
        return None;
    }
    let _irq = crate::cpu::IrqGuard::new();
    Some(imp::probe_iret_gs(probe_iret_decoy()))
}

/// For a #GP hook during `probe_iret_with_foreign_gs`: if `frame` is that probe's fault, gives the frame the
/// `iretq` tried the kernel SS and returns true; false for every other #GP.
pub fn probe_iret_repair(frame: &TrapFrame) -> bool {
    let (_, _, iret) = imp::user_gs_windows();
    let resume = imp::probe_iret_resume();
    if frame.vector != GENERAL_PROTECTION as u64 || frame.rip != iret || frame.error != 0x18 {
        return false;
    }
    if crate::panic::read_kernel_u64(frame.rsp) != Some(resume) {
        return false;
    }
    imp::probe_iret_set_ss(frame.rsp, crate::gdt::KERNEL_SS as u64);
    true
}

/// Checks that the window `in_user_gs_window` uses still holds entry.rs's return paths: [hal_user_execute,
/// hal_syscall_entry) is short and holds one `sysretq`, an `iretq` and the four `swapgs` of the sysret, iret and
/// return-to-kernel paths, and `hal_trap_iret` is an `iretq`. A reorder of entry.rs would otherwise turn CR-2's
/// window test off without a sound.
pub fn user_gs_window_check() -> Result<(), &'static str> {
    let (start, end, iret) = imp::user_gs_windows();
    if start >= end || end - start > 4096 {
        return Err("hal_user_execute .. hal_syscall_entry is not one short block");
    }
    let byte = |a: u64| crate::panic::read_kernel_u64(a & !7).map(|w| (w >> (8 * (a & 7))) as u8);
    if byte(iret) != Some(0x48) || byte(iret + 1) != Some(0xcf) {
        return Err("hal_trap_iret is not an iretq");
    }
    if (start..end).any(|a| byte(a).is_none()) {
        return Err("the window's code cannot be read");
    }
    let ops = window_ops(&mut (start..end).map(|a| byte(a).unwrap_or(0)));
    if ops.sysretq != 1 || ops.iretq < 1 || ops.swapgs < 4 {
        return Err("the window lacks the sysretq, the iretq or a swapgs of entry.rs's return paths");
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct WindowOps {
    swapgs: u32,
    sysretq: u32,
    iretq: u32,
}

/// Counts `swapgs` (0f 01 f8), `sysretq` (48 0f 07) and `iretq` (48 cf) in a byte stream.
fn window_ops(bytes: &mut dyn Iterator<Item = u8>) -> WindowOps {
    let mut ops = WindowOps::default();
    let (mut b2, mut b1) = (0u8, 0u8);
    for b in bytes {
        match (b2, b1, b) {
            (0x0f, 0x01, 0xf8) => ops.swapgs += 1,
            (0x48, 0x0f, 0x07) => ops.sysretq += 1,
            (_, 0x48, 0xcf) => ops.iretq += 1,
            _ => {}
        }
        (b2, b1) = (b1, b);
    }
    ops
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

    # (addr, gs, out): the probe load with the GS base set to `gs`; *out = the GS base at the resume.
    .global hal_probe_load_gs
hal_probe_load_gs:
    movq %rdx, %r8
    rdgsbase %r9
    wrgsbase %rsi
    call hal_probe_load
    rdgsbase %rcx
    wrgsbase %r9
    movq %rcx, (%r8)
    ret

    # (gs, out): the stubs' iretq with a kernel frame whose SS is 0x18 (user data, RPL 0: #GP 0x18) while the GS
    # base is `gs`; *out = the GS base at the resume. The #GP hook sets SS to the kernel's (`probe_iret_repair`)
    # and the iretq runs again. r8-r10 survive the #GP (the stubs save every register).
    .global hal_probe_iret_gs
hal_probe_iret_gs:
    movq %rsi, %r8
    rdgsbase %r9
    movq %rsp, %r10
    pushq $0x18
    pushq %r10
    pushfq
    pushq $0x08
    leaq hal_probe_iret_resume(%rip), %rax
    pushq %rax
    wrgsbase %rdi
    jmp hal_trap_iret
    .global hal_probe_iret_resume
hal_probe_iret_resume:
    rdgsbase %rcx
    wrgsbase %r9
    movq %rcx, (%r8)
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
        fn hal_probe_load_gs(addr: u64, gs: u64, out: *mut u64) -> u64;
        fn hal_probe_iret_gs(gs: u64, out: *mut u64);
        static hal_probe_iret_resume: u8;
        // The same signature as entry.rs's declaration (one symbol, one signature); only its address is used here.
        fn hal_user_execute(ctx: *mut crate::entry::UserContext, sysret: u64);
        fn hal_syscall_entry();
        fn hal_trap_iret();
        fn hal_soft_int_nmi();
        fn hal_soft_int_mc();
        static hal_soft_int_nmi_ret: u8;
        static hal_soft_int_mc_ret: u8;
    }

    #[inline(always)]
    pub fn user_gs_windows() -> (u64, u64, u64) {
        (hal_user_execute as *const () as u64, hal_syscall_entry as *const () as u64, hal_trap_iret as *const () as u64)
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

    pub fn probe_load_gs(addr: u64, gs: u64) -> (u64, u64) {
        let mut after = 0u64;
        // SAFETY: as `probe_load`; in addition the GS base is `gs` from just before the load until the resume,
        // which the caller runs with interrupts off (NMI and #MC fix their GS base themselves), and the old base
        // is written back before the return. Clobbers rax, rcx, r8, r9 (caller-saved).
        let v = unsafe { hal_probe_load_gs(addr, gs, &mut after) };
        (v, after)
    }

    pub fn probe_iret_gs(gs: u64) -> u64 {
        let mut after = 0u64;
        // SAFETY: the stub pushes a 5-word iret frame on its own stack and jumps to the stubs' iretq, whose #GP
        // goes through hal_kernel_trap; the caller's #GP hook repairs that frame's SS (`probe_iret_repair`), so
        // the iretq returns to hal_probe_iret_resume with rsp as before the pushes. `gs` is canonical (a static's
        // address), loaded only between the pushes and the resume, with interrupts off (the caller's IrqGuard;
        // NMI and #MC fix their GS base themselves); the old base is written back. Without the hook the #GP
        // panics, as any unhandled kernel fault. Clobbers rax, rcx, r8-r10 (caller-saved).
        unsafe { hal_probe_iret_gs(gs, &mut after) };
        after
    }

    pub fn probe_iret_resume() -> u64 {
        &raw const hal_probe_iret_resume as u64
    }

    pub fn probe_iret_set_ss(frame_rsp: u64, ss: u64) {
        // SAFETY: `probe_iret_repair` found hal_probe_iret_resume at frame_rsp: this is hal_probe_iret_gs's iret
        // frame (rip, cs, rflags, rsp, ss) on this CPU's stack, live until the iretq that runs after the hook.
        unsafe { ((frame_rsp + 32) as *mut u64).write_volatile(ss) };
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
                // Interrupt gate, present; #BP reachable from ring 3 (int3 becomes SIGTRAP later), and since
                // AB-10a `int 0x80` (vector reserved in `vectors`; entry.rs answers the TLS/LDT calls).
                let attr: u64 = if v == 3 || v == crate::entry::INT80_VECTOR as u64 { 0xEE } else { 0x8E };
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
    pub fn probe_load_gs(_addr: u64, _gs: u64) -> (u64, u64) {
        (0, 0)
    }
    pub fn probe_iret_gs(_gs: u64) -> u64 {
        0
    }
    pub fn probe_iret_resume() -> u64 {
        0
    }
    pub fn probe_iret_set_ss(_frame_rsp: u64, _ss: u64) {}
    pub fn user_gs_windows() -> (u64, u64, u64) {
        (0x1000, 0x1100, 0x2000)
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_return_fixup_sets_rpl_of_user_frames_only() {
        // The frame an AMD CPU saves for an NMI after sysretq, and the one Intel saves.
        for (cs, ss, want) in [(0x33u64, 0x28u64, 0x2bu64), (0x33, 0x2b, 0x2b), (0x23, 0x28, 0x2b), (0x08, 0x10, 0x10), (0x08, 0x00, 0x00)] {
            let mut f = TrapFrame { cs, ss, rflags: 0x246, ..TrapFrame::default() };
            user_return_fixup(&mut f);
            assert_eq!(f.ss, want, "cs {cs:#x} ss {ss:#x}");
            assert_eq!(f.cs, cs);
            assert_eq!(f.rflags, 0x246);
        }
    }

    #[test]
    fn user_return_fixup_forces_the_user_segments_and_flags() {
        let fixed = user_frames_fixed();
        // A user frame with a foreign RPL-3 code selector, IOPL 3, NT and VM: back with the GDT's user segments
        // and the flags a program can have; IF and AC stay as saved.
        let mut f = TrapFrame { cs: 0x2b, ss: 0x2b, rflags: 0x4_0202 | 3 << 12 | 1 << 14 | 1 << 17, ..TrapFrame::default() };
        user_return_fixup(&mut f);
        assert_eq!((f.cs, f.ss, f.rflags), (0x33, 0x2b, 0x4_0202));
        let mut g = TrapFrame { cs: 0x33, ss: 0x2b, rflags: 0x2, ..TrapFrame::default() };
        user_return_fixup(&mut g);
        assert_eq!((g.cs, g.ss, g.rflags), (0x33, 0x2b, 0x2));
        // AB-10a: 32-bit code keeps its code segment; an LDT code selector does not survive.
        let mut h = TrapFrame { cs: 0x23, ss: 0x2b, rflags: 0x2, ..TrapFrame::default() };
        user_return_fixup(&mut h);
        assert_eq!((h.cs, h.ss), (0x23, 0x2b));
        let mut l = TrapFrame { cs: 0x0f, ss: 0x2b, rflags: 0x2, ..TrapFrame::default() };
        user_return_fixup(&mut l);
        assert_eq!(l.cs, 0x33);
        assert!(user_frames_fixed() > fixed);
    }

    #[test]
    fn window_ops_count_the_return_instructions() {
        // swapgs; mov; sysretq; swapgs; iretq; swapgs; rdgsbase; swapgs, and look-alikes that are not.
        let code = [
            0x0f, 0x01, 0xf8, 0x48, 0x89, 0xc8, 0x48, 0x0f, 0x07, 0x0f, 0x01, 0xf8, 0x48, 0xcf, 0x0f, 0x01, 0xf8, 0xf3,
            0x48, 0x0f, 0xae, 0xc8, 0x0f, 0x01, 0xf8, 0x0f, 0x01, 0xf9, 0x48, 0x0f, 0x05,
        ];
        let ops = window_ops(&mut code.iter().copied());
        assert_eq!(ops, WindowOps { swapgs: 4, sysretq: 1, iretq: 1 });
    }

    #[test]
    fn probes_refuse_without_their_conditions() {
        // On the host FSGSBASE is never on: both probes refuse instead of running.
        assert_eq!(probe_load_with_gs(0), None);
        assert_eq!(probe_iret_with_foreign_gs(), None);
        assert!(!probe_iret_repair(&TrapFrame { vector: 13, error: 0x18, rip: 0x2000, ..TrapFrame::default() }));
    }

    #[test]
    fn windows_and_suspect_vectors() {
        // Host stand-ins: entry window 0x1000..0x1100, the stubs' iretq at 0x2000.
        assert!(in_user_gs_window(0x1000) && in_user_gs_window(0x10ff) && in_user_gs_window(0x2000));
        assert!(!in_user_gs_window(0xfff) && !in_user_gs_window(0x1100) && !in_user_gs_window(0x2001));
        for v in 11..=14 {
            assert!(gs_fault_suspect(v, 0x2000), "vector {v}");
        }
        for v in [0u64, 2, 3, 8, 10, 15, 18, 32, 255] {
            assert!(!gs_fault_suspect(v, 0x2000), "vector {v}");
        }
        // Outside the windows only the GS base decides, and the check is off until armed.
        assert!(!gs_fault_suspect(14, 0x3000));
    }
}
