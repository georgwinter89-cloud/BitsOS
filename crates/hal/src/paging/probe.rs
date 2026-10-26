//! Deliberate page faults for MM's ktests (W^X, NX, guard pages), caught through the #PF exception hook
//! (CP's `idt::set_exception_hook`). Each probe touches only an address that cannot be harmed:
//! - `read_unmapped`: a load from an address the loaded root does not map (a guard page);
//! - `write_same`: re-writes the byte already there, and only into a page the tables say is read-only (so on a
//!   correct CPU the write never happens; a writable page is reported without writing);
//! - `exec`: calls an address that holds a `ret` byte (0xC3), so even if NX failed nothing else would run.
//!
//! Interrupts are off and the hook is installed only for the duration of one probe; other faults chain to the
//! previous hook.
//!
//! MM-6: `UserProbes` installs the hook for a whole session in which any CPU (an AP in an IPI handler too) may
//! load or store a user address through its own TLB (`user_read`, `user_write`): the shootdown ktests check
//! that no CPU reaches a page after `munmap`/`mprotect` returned. The access runs with EFLAGS.AC set (SMAP).
//! Every user probe counts itself in `IN_FLIGHT` before it checks the session; closing a session waits until
//! no probe is in flight before it takes the hook away, so a probe still running on another CPU never meets a
//! fault nobody resumes. A probe that never finishes (a CPU stuck inside it) keeps the session's hook and the
//! probe slot for good (`leaked_sessions`) instead of risking a kernel #PF.

use super::PageFlags;
use crate::idt::{self, TrapFrame};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// The access faulted: CR2 and the #PF error code (bit 0 present, 1 write, 4 instruction fetch).
    Faulted { addr: u64, error: u64 },
    /// The access completed without a fault.
    NoFault,
    /// `write_same` only: the tables allow writing, so the probe did not write.
    WritableNotProbed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// `read_unmapped` was given a mapped address.
    Mapped,
    /// Not in the kernel image or the direct map.
    OutsideKernel,
    /// `exec` target is not mapped or does not hold 0xC3.
    NotRet,
    /// Host build.
    Unsupported,
    /// The address is not canonical (the access would raise #GP, which the probe does not catch).
    NonCanonical,
    /// Another probe is running (the #PF hook is one global slot).
    Busy,
    /// `user_read`/`user_write` outside a `UserProbes` session, or not a user address.
    NoSession,
}

pub const PF_PRESENT: u64 = 1;
pub const PF_WRITE: u64 = 1 << 1;
pub const PF_FETCH: u64 = 1 << 4;

static FAULT_ADDR: AtomicU64 = AtomicU64::new(0);
static FAULT_ERROR: AtomicU64 = AtomicU64::new(0);
static EXEC_TARGET: AtomicU64 = AtomicU64::new(0);
static PREV_HOOK: AtomicUsize = AtomicUsize::new(0);
/// Held for the whole of one probe: the hook slot and PREV_HOOK are global, the IrqGuard only local.
static BUSY: AtomicBool = AtomicBool::new(false);
/// A `UserProbes` session is open (its hook is installed).
static USER_SESSION: AtomicBool = AtomicBool::new(false);
/// User/vmap probes between their session check and the end of their access (any CPU).
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
/// Sessions whose close gave up waiting for a probe in flight: hook and probe slot stay taken.
static LEAKED: AtomicU64 = AtomicU64::new(0);
/// How long closing a session waits for the probes in flight (one load or store each; this only runs out
/// when a CPU stopped inside a probe).
const CLOSE_WAIT_NS: u64 = 1_000_000_000;
const FAULT_MARK: u64 = 1 << 63;

fn canonical(addr: u64) -> bool {
    (((addr << 16) as i64) >> 16) as u64 == addr
}

fn in_kernel(addr: u64) -> bool {
    let (s, e) = super::kernel_image();
    (addr >= s && addr < e) || crate::virt_to_phys(addr).is_some()
}

/// Loads 8 bytes from `addr`, which the loaded root must not map.
pub fn read_unmapped(addr: u64) -> Result<Probe, Refused> {
    if !canonical(addr) {
        return Err(Refused::NonCanonical);
    }
    if super::translate(super::current_root(), addr).is_some() {
        return Err(Refused::Mapped);
    }
    run(|| imp::read(addr))
}

/// Writes back the byte at `addr` (kernel image or direct map) if the tables say the page is read-only.
pub fn write_same(addr: u64) -> Result<Probe, Refused> {
    if !in_kernel(addr) {
        return Err(Refused::OutsideKernel);
    }
    if let Some((_, fl)) = super::translate(super::current_root(), addr)
        && fl.contains(PageFlags::WRITABLE)
    {
        return Ok(Probe::WritableNotProbed);
    }
    run(|| imp::write_same(addr))
}

/// Calls `addr` (kernel image or direct map, present, holding 0xC3).
pub fn exec(addr: u64) -> Result<Probe, Refused> {
    if !in_kernel(addr) {
        return Err(Refused::OutsideKernel);
    }
    if super::translate(super::current_root(), addr).is_none() || imp::byte(addr) != Some(0xC3) {
        return Err(Refused::NotRet);
    }
    run(|| {
        EXEC_TARGET.store(addr, Ordering::Relaxed);
        let r = imp::exec(addr);
        EXEC_TARGET.store(0, Ordering::Relaxed);
        r
    })
}

fn run(f: impl FnOnce() -> Option<u64>) -> Result<Probe, Refused> {
    if BUSY.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return Err(Refused::Busy);
    }
    let r = run_exclusive(f);
    BUSY.store(false, Ordering::Release);
    r
}

fn run_exclusive(f: impl FnOnce() -> Option<u64>) -> Result<Probe, Refused> {
    let _irq = crate::cpu::IrqGuard::new();
    let prev = idt::exception_hook(idt::PAGE_FAULT);
    PREV_HOOK.store(prev.map_or(0, |h| h as usize), Ordering::Relaxed);
    FAULT_ADDR.store(0, Ordering::Relaxed);
    FAULT_ERROR.store(0, Ordering::Relaxed);
    idt::set_exception_hook(idt::PAGE_FAULT, hook);
    let faulted = f();
    match prev {
        Some(h) => idt::set_exception_hook(idt::PAGE_FAULT, h),
        None => idt::clear_exception_hook(idt::PAGE_FAULT),
    }
    match faulted {
        None => Err(Refused::Unsupported),
        Some(0) => Ok(Probe::NoFault),
        Some(_) => Ok(Probe::Faulted { addr: FAULT_ADDR.load(Ordering::Relaxed), error: FAULT_ERROR.load(Ordering::Relaxed) }),
    }
}

fn hook(f: &mut TrapFrame) -> bool {
    // The exact rip of a user probe's access identifies the fault; no session check, so a probe that passed
    // its check just before the session started closing is still resumed.
    if let Some(r) = imp::user_resume_for(f.rip) {
        // Any CPU: the fault goes back in rdx (marked), the probe's resume path clears AC.
        f.rdx = FAULT_MARK | f.error;
        f.rip = r;
        return true;
    }
    let target = EXEC_TARGET.load(Ordering::Relaxed);
    let resume = if let Some(r) = imp::resume_for(f.rip) {
        Some(r)
    } else if target != 0 && f.rip == target && f.error & PF_FETCH != 0 && f.cs == crate::gdt::KERNEL_CS as u64 {
        // The fetch faulted at the call target: drop the return address the call pushed.
        f.rsp += 8;
        imp::exec_resume()
    } else {
        None
    };
    if let Some(r) = resume {
        FAULT_ADDR.store(crate::cpu::read_cr2(), Ordering::Relaxed);
        FAULT_ERROR.store(f.error, Ordering::Relaxed);
        f.rip = r;
        f.rax = 1;
        return true;
    }
    let prev = PREV_HOOK.load(Ordering::Relaxed);
    // SAFETY: PREV_HOOK holds 0 or a value taken from `idt::exception_hook` (a valid ExceptionHook).
    prev != 0 && unsafe { core::mem::transmute::<usize, idt::ExceptionHook>(prev) }(f)
}

/// An open session for `user_read`/`user_write` (see the module comment). Dropping it restores the old hook.
pub struct UserProbes {
    prev: Option<idt::ExceptionHook>,
}

/// Opens a session (`Busy` while another probe or session runs; `Unsupported` on the host).
pub fn user_probes() -> Result<UserProbes, Refused> {
    if !crate::BARE_METAL {
        return Err(Refused::Unsupported);
    }
    if BUSY.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return Err(Refused::Busy);
    }
    let _irq = crate::cpu::IrqGuard::new();
    let prev = idt::exception_hook(idt::PAGE_FAULT);
    PREV_HOOK.store(prev.map_or(0, |h| h as usize), Ordering::Relaxed);
    idt::set_exception_hook(idt::PAGE_FAULT, hook);
    USER_SESSION.store(true, Ordering::Release);
    Ok(UserProbes { prev })
}

/// Sessions that could not be closed because a probe never finished (see the module comment).
pub fn leaked_sessions() -> u64 {
    LEAKED.load(Ordering::Relaxed)
}

impl Drop for UserProbes {
    fn drop(&mut self) {
        // No new probe starts after this (SeqCst with the probes' count-then-check), then the ones in flight
        // finish. The wait serves TLB requests in case the caller runs with interrupts off.
        USER_SESSION.store(false, Ordering::SeqCst);
        let t0 = super::tlb::now();
        while IN_FLIGHT.load(Ordering::SeqCst) != 0 {
            if super::tlb::now().saturating_sub(t0) >= CLOSE_WAIT_NS {
                LEAKED.fetch_add(1, Ordering::Relaxed);
                return;
            }
            super::tlb::poll();
            core::hint::spin_loop();
        }
        let _irq = crate::cpu::IrqGuard::new();
        match self.prev {
            Some(h) => idt::set_exception_hook(idt::PAGE_FAULT, h),
            None => idt::clear_exception_hook(idt::PAGE_FAULT),
        }
        BUSY.store(false, Ordering::Release);
    }
}

/// Runs one user/vmap probe if a session is open: counted in `IN_FLIGHT` from before the session check until
/// the access is over, so closing the session waits for it.
fn in_session<R>(f: impl FnOnce() -> R) -> Result<R, Refused> {
    IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
    let r = if USER_SESSION.load(Ordering::SeqCst) { Ok(f()) } else { Err(Refused::NoSession) };
    IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    r
}

fn user_addr(va: u64) -> Result<(), Refused> {
    if va >= crate::USER_TOP || va & 7 != 0 {
        return Err(Refused::NoSession);
    }
    Ok(())
}

/// Loads 8 bytes from user address `va` (8-aligned) through this CPU's TLB and the loaded root: `Ok(Ok(v))`,
/// or `Ok(Err(error))` with the #PF error code. Any context; needs an open session.
pub fn user_read(va: u64) -> Result<Result<u64, u64>, Refused> {
    user_addr(va)?;
    let (v, st) = in_session(|| imp::user_read(va))?;
    Ok(if st & FAULT_MARK != 0 { Err(st & !FAULT_MARK) } else { Ok(v) })
}

/// Loads 8 bytes from a vmap address (8-aligned; kernel stacks, `reserve_kernel_va` ranges) the same way: the
/// kernel-half shootdown ktests. Needs an open session.
pub fn vmap_read(va: u64) -> Result<Result<u64, u64>, Refused> {
    if !(crate::VMAP_BASE..bootinfo::layout::VMAP_END).contains(&va) || va & 7 != 0 {
        return Err(Refused::NoSession);
    }
    let (v, st) = in_session(|| imp::user_read(va))?;
    Ok(if st & FAULT_MARK != 0 { Err(st & !FAULT_MARK) } else { Ok(v) })
}

/// Stores `v` at user address `va` (8-aligned) the same way: `Ok(Ok(()))` or the #PF error code.
pub fn user_write(va: u64, v: u64) -> Result<Result<(), u64>, Refused> {
    user_addr(va)?;
    let st = in_session(|| imp::user_write(va, v))?;
    Ok(if st & FAULT_MARK != 0 { Err(st & !FAULT_MARK) } else { Ok(()) })
}

#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .text
    .p2align 4
hal_mm_probe_read:
hal_mm_probe_read_fault:
    movq (%rdi), %rax
    xorl %eax, %eax
hal_mm_probe_read_resume:
    ret

hal_mm_probe_write:
hal_mm_probe_write_load:
    movb (%rdi), %al
hal_mm_probe_write_store:
    movb %al, (%rdi)
    xorl %eax, %eax
hal_mm_probe_write_resume:
    ret

hal_mm_probe_exec:
    xorl %eax, %eax
    call *%rdi
hal_mm_probe_exec_resume:
    ret

    .p2align 4
hal_mm_uprobe_read:
    xorl %edx, %edx
    xorl %eax, %eax
    pushfq
    orq $0x40000, (%rsp)
    popfq
hal_mm_uprobe_read_fault:
    movq (%rdi), %rax
hal_mm_uprobe_read_resume:
    pushfq
    andq $-262145, (%rsp)
    popfq
    movq %rdx, (%rsi)
    ret

hal_mm_uprobe_write:
    movq %rdx, %rax
    xorl %edx, %edx
    pushfq
    orq $0x40000, (%rsp)
    popfq
hal_mm_uprobe_write_fault:
    movq %rax, (%rdi)
hal_mm_uprobe_write_resume:
    pushfq
    andq $-262145, (%rsp)
    popfq
    movq %rdx, (%rsi)
    ret

    .global hal_mm_probe_labels
    .section .rodata.hal_mm_probe_labels, "a"
    .p2align 3
hal_mm_probe_labels:
    .quad hal_mm_probe_read, hal_mm_probe_read_fault, hal_mm_probe_read_resume
    .quad hal_mm_probe_write, hal_mm_probe_write_load, hal_mm_probe_write_store, hal_mm_probe_write_resume
    .quad hal_mm_probe_exec, hal_mm_probe_exec_resume
    .quad hal_mm_uprobe_read, hal_mm_uprobe_read_fault, hal_mm_uprobe_read_resume
    .quad hal_mm_uprobe_write, hal_mm_uprobe_write_fault, hal_mm_uprobe_write_resume
"#,
    options(att_syntax)
);

#[cfg(target_os = "none")]
mod imp {
    unsafe extern "C" {
        static hal_mm_probe_labels: [u64; 15];
    }
    fn labels() -> [u64; 15] {
        // SAFETY: a read-only table of fifteen code addresses defined in the global_asm above.
        unsafe { hal_mm_probe_labels }
    }
    type UserProbe = extern "C" fn(u64, *mut u64, u64) -> u64;
    fn ucall(at: u64, va: u64, v: u64) -> (u64, u64) {
        let mut st = 0u64;
        // SAFETY: `at` is one of the two user probe entry points above, `extern "C" fn(va, *status, value) -> u64`
        // clobbering rax and rdx only (both caller-saved) and writing the status through the pointer to `st`.
        // The access itself may fault only at the labelled instruction, which the session's hook resumes.
        let f: UserProbe = unsafe { core::mem::transmute::<u64, UserProbe>(at) };
        let r = f(va, &mut st, v);
        (r, st)
    }
    pub fn user_read(va: u64) -> (u64, u64) {
        ucall(labels()[9], va, 0)
    }
    pub fn user_write(va: u64, v: u64) -> u64 {
        ucall(labels()[12], va, v).1
    }
    /// The resume point for a fault at `rip` inside a user probe.
    pub fn user_resume_for(rip: u64) -> Option<u64> {
        let l = labels();
        if rip == l[10] {
            Some(l[11])
        } else if rip == l[13] {
            Some(l[14])
        } else {
            None
        }
    }
    type Probe = extern "C" fn(u64) -> u64;
    fn call(at: u64, arg: u64) -> u64 {
        // SAFETY: `at` is one of the three probe entry points above, each `extern "C" fn(u64) -> u64` that
        // clobbers only rax (and, for exec, whatever a lone `ret` clobbers: nothing).
        let f: Probe = unsafe { core::mem::transmute::<u64, Probe>(at) };
        f(arg)
    }
    pub fn read(addr: u64) -> Option<u64> {
        Some(call(labels()[0], addr))
    }
    pub fn write_same(addr: u64) -> Option<u64> {
        Some(call(labels()[3], addr))
    }
    pub fn exec(addr: u64) -> Option<u64> {
        Some(call(labels()[7], addr))
    }
    /// The resume point for a fault at `rip` inside the read or write probe.
    pub fn resume_for(rip: u64) -> Option<u64> {
        let l = labels();
        if rip == l[1] {
            Some(l[2])
        } else if rip == l[4] || rip == l[5] {
            Some(l[6])
        } else {
            None
        }
    }
    pub fn exec_resume() -> Option<u64> {
        Some(labels()[8])
    }
    pub fn byte(addr: u64) -> Option<u8> {
        // SAFETY: callers checked that `addr` is a present page of the kernel image or the direct map (RAM);
        // a one-byte volatile read of RAM has no side effect.
        Some(unsafe { (addr as *const u8).read_volatile() })
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    pub fn read(_addr: u64) -> Option<u64> {
        None
    }
    pub fn write_same(_addr: u64) -> Option<u64> {
        None
    }
    pub fn exec(_addr: u64) -> Option<u64> {
        None
    }
    pub fn resume_for(_rip: u64) -> Option<u64> {
        None
    }
    pub fn exec_resume() -> Option<u64> {
        None
    }
    pub fn byte(_addr: u64) -> Option<u8> {
        None
    }
    pub fn user_read(_va: u64) -> (u64, u64) {
        (0, 0)
    }
    pub fn user_write(_va: u64, _v: u64) -> u64 {
        0
    }
    pub fn user_resume_for(_rip: u64) -> Option<u64> {
        None
    }
}
