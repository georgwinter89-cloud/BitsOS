//! Copies between kernel buffers and user memory (owner: AB; contract part of `C: AddressSpace`).
//!
//! Two methods, one API:
//! - **Fast path** (AB-1), when `root` is a process's address space loaded in CR3 (a process copying its own
//!   memory, the normal syscall case): one `rep movsb`/`rep stosb` through the user mapping, bracketed by `stac`/`clac`
//!   when SMAP is on. A fault on the user side is caught by the #PF fixup (`install_fixup`), which resumes after
//!   the instruction with the count of bytes not copied; the copy then asks the fault resolver (MM-5a's lazy
//!   fill, in thread context) once per address and retries, or returns `Efault`. No page walk, and an unmap
//!   racing with the copy faults instead of reading a freed frame.
//!   The copy runs in ring 0, where the USER bit does not restrict anything (SMAP covers user pages only), so
//!   the fast path is correct only for roots that map nothing supervisor-only below USER_TOP: never for the
//!   kernel's own roots (bitsboot's or MM-2's tables, with the identity-mapped kexec stub page), which walk.
//!   Known gap (MM change request): MM maps PROT_NONE pages present without USER, so a fast read from one
//!   succeeds where the walk says EFAULT (only the program's own frame; writes fault as the page is read-only).
//!   It also needs the fixup at the head of the #PF hooks: when another hook replaced it, copies walk (counted
//!   in `Stats::hook_replaced`) instead of turning a bad pointer into a kernel page fault.
//! - **Page walk** (G0, kept as fallback): for roots that are not loaded (MCP reading another process, the
//!   loader filling a new process) and before the fixup is installed: walk `root` page by page, check present +
//!   USER (+ WRITABLE for writes), copy through the direct map. `copy_to_user_force` (the loader writing
//!   read-only text) always walks.
//!
//! A fault on the kernel side of a copy is a kernel bug and is not fixed up (the #PF fixup only accepts a fault
//! address below `USER_TOP` at the copy instruction).

use crate::paging::{self, PageFlags};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Efault;

/// Makes a user page present for a kernel access that faulted at `addr` (`write`: the kernel wanted to write).
/// Returns true when the copy should retry. Registered by MM (MM-5a's lazy fill); runs in thread context.
pub type FaultResolver = fn(addr: u64, write: bool) -> bool;

static FIXUP_ON: AtomicBool = AtomicBool::new(false);
static PREV_PF: AtomicUsize = AtomicUsize::new(0);
static RESOLVER: AtomicUsize = AtomicUsize::new(0);

static FAST_FAULTS: AtomicU64 = AtomicU64::new(0);
static RESOLVED: AtomicU64 = AtomicU64::new(0);
static WALKS: AtomicU64 = AtomicU64::new(0);
static HOOK_REPLACED: AtomicU64 = AtomicU64::new(0);

/// Counters (rare events only; the fast path itself counts nothing).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Faults the fixup turned into a short copy (bad pointers, or pages the resolver then filled).
    pub fast_faults: u64,
    /// Of those, retried after the resolver made the page present.
    pub resolved: u64,
    /// Copies that walked the page tables (root not loaded, or the fixup not yet installed).
    pub walks: u64,
    /// Whether the fast path is available.
    pub fast_path: bool,
    /// Copies that walked because another #PF hook had replaced the fixup (should stay 0; CP change request
    /// to call the fixup from `hal_kernel_trap` directly).
    pub hook_replaced: u64,
}

pub fn stats() -> Stats {
    Stats {
        fast_faults: FAST_FAULTS.load(Ordering::Relaxed),
        resolved: RESOLVED.load(Ordering::Relaxed),
        walks: WALKS.load(Ordering::Relaxed),
        fast_path: FIXUP_ON.load(Ordering::Acquire),
        hook_replaced: HOOK_REPLACED.load(Ordering::Relaxed),
    }
}

/// Installs the #PF fixup for the fast path (chaining to the hook installed before it) and turns the fast path
/// on. Idempotent. Call once interrupts and the IDT are set up (`syscall::init`).
pub fn install_fixup() {
    if FIXUP_ON.load(Ordering::Acquire) || !crate::BARE_METAL {
        return;
    }
    let prev = crate::idt::exception_hook(crate::idt::PAGE_FAULT).filter(|h| *h as usize != pf_hook as crate::idt::ExceptionHook as usize);
    PREV_PF.store(prev.map_or(0, |h| h as usize), Ordering::Release);
    crate::idt::set_exception_hook(crate::idt::PAGE_FAULT, pf_hook);
    FIXUP_ON.store(true, Ordering::Release);
}

/// Registers MM's resolver for faults on user pages during a fast copy (`None` removes it); returns the one
/// installed before, so a test that swaps in its own can put it back (AB-8: file-backed program pages are only
/// present once touched, so every syscall reading a program's constants depends on the resolver).
pub fn set_fault_resolver(f: Option<FaultResolver>) -> Option<FaultResolver> {
    let old = RESOLVER.swap(f.map_or(0, |f| f as usize), Ordering::AcqRel);
    // SAFETY: only this function stores non-zero values, always a valid FaultResolver.
    (old != 0).then(|| unsafe { core::mem::transmute::<usize, FaultResolver>(old) })
}

fn pf_hook(f: &mut crate::idt::TrapFrame) -> bool {
    if fixup(f) {
        return true;
    }
    let prev = PREV_PF.load(Ordering::Acquire);
    // SAFETY: PREV_PF holds 0 or a value taken from `idt::exception_hook`, always a valid ExceptionHook.
    prev != 0 && unsafe { core::mem::transmute::<usize, crate::idt::ExceptionHook>(prev) }(f)
}

/// The exception-table check: a #PF at one of the copy instructions on a user address resumes after the
/// instruction (rcx = bytes left, see the assembly). Also usable by idt.rs directly (CP change request).
pub(crate) fn fixup(f: &mut crate::idt::TrapFrame) -> bool {
    if f.vector != crate::idt::PAGE_FAULT as u64 || crate::cpu::read_cr2() >= crate::USER_TOP {
        return false;
    }
    match imp::resume_for(f.rip) {
        Some(r) => {
            f.rip = r;
            true
        }
        None => false,
    }
}

fn check(addr: u64, len: usize) -> Result<(), Efault> {
    let end = addr.checked_add(len as u64).ok_or(Efault)?;
    if end > crate::USER_TOP { Err(Efault) } else { Ok(()) }
}

/// Whether `root` may take the fast path: the fixup is installed and still first in line for #PF, the SMAP
/// decision is made (`entry::smap_all`), and `root` is loaded, not held only lazily (MM-tlb3: its TLB entries may be
/// stale) and not one of the kernel's own roots.
fn fast_ok(root: u64) -> bool {
    if !FIXUP_ON.load(Ordering::Acquire) || root != paging::current_root() || paging::tlb::lazy_here() || crate::entry::smap_all().is_none() {
        return false;
    }
    if root == crate::switch::kernel_root() || Some(root) == paging::kernel_root() {
        return false;
    }
    let ours = pf_hook as crate::idt::ExceptionHook as usize;
    if crate::idt::exception_hook(crate::idt::PAGE_FAULT).map(|h| h as usize) != Some(ours) {
        HOOK_REPLACED.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    true
}

#[derive(Clone, Copy)]
enum Op {
    FromUser(*mut u8),
    ToUser(*const u8),
    Zero,
}

/// The fast copy with retries after the resolver. `user..user+len` is checked to lie below USER_TOP.
fn fast(user: u64, len: usize, op: Op) -> Result<(), Efault> {
    // The same gate as the entry's clac (true only when every CPU that ran init_syscall has CR4.SMAP, so
    // stac/clac cannot #UD here); `fast_ok` made sure the decision exists. Not `cpu::smap_enabled()`, which is
    // the last CPU's CR4.
    let smap = crate::entry::smap_all().unwrap_or(false);
    let mut done = 0usize;
    let mut last_fault = u64::MAX;
    while done < len {
        let n = len - done;
        let u = user + done as u64;
        let left = match op {
            // SAFETY: dst is `n` bytes of the caller's kernel buffer from `done`; u..u+n is user memory below
            // USER_TOP in the loaded address space; a fault there is fixed up (pf_hook) and returns the rest.
            Op::FromUser(dst) => unsafe { imp::copy(dst.add(done), u as *const u8, n, smap) },
            // SAFETY: as above with source and destination swapped.
            Op::ToUser(src) => unsafe { imp::copy(u as *mut u8, src.add(done), n, smap) },
            // SAFETY: u..u+n is user memory below USER_TOP in the loaded address space; faults are fixed up.
            Op::Zero => unsafe { imp::zero(u as *mut u8, n, smap) },
        };
        if left == 0 {
            return Ok(());
        }
        done = len - left.min(len);
        FAST_FAULTS.fetch_add(1, Ordering::Relaxed);
        let at = user + done as u64;
        if at == last_fault {
            return Err(Efault);
        }
        last_fault = at;
        let r = RESOLVER.load(Ordering::Acquire);
        if r == 0 {
            return Err(Efault);
        }
        // SAFETY: only `set_fault_resolver` stores non-zero values, always a valid FaultResolver.
        let resolve: FaultResolver = unsafe { core::mem::transmute::<usize, FaultResolver>(r) };
        if !resolve(at, !matches!(op, Op::FromUser(_))) {
            return Err(Efault);
        }
        RESOLVED.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

fn for_each_page(root: u64, addr: u64, len: usize, write: bool, mut f: impl FnMut(u64, usize, usize)) -> Result<(), Efault> {
    check(addr, len)?;
    WALKS.fetch_add(1, Ordering::Relaxed);
    let mut done = 0usize;
    while done < len {
        let va = addr + done as u64;
        let (pa, flags) = paging::translate(root, va).ok_or(Efault)?;
        if !flags.contains(PageFlags::USER) || (write && !flags.contains(PageFlags::WRITABLE)) {
            return Err(Efault);
        }
        let in_page = (4096 - (va & 0xFFF)) as usize;
        let n = in_page.min(len - done);
        f(pa, done, n);
        done += n;
    }
    Ok(())
}

pub fn copy_from_user(root: u64, dst: &mut [u8], src: u64) -> Result<(), Efault> {
    let len = dst.len();
    check(src, len)?;
    if len == 0 {
        return Ok(());
    }
    if fast_ok(root) {
        return fast(src, len, Op::FromUser(dst.as_mut_ptr()));
    }
    for_each_page(root, src, len, false, |pa, off, n| {
        // SAFETY: pa..pa+n is a present user frame (checked), reachable through the direct map; dst has n
        // bytes at off.
        unsafe { core::ptr::copy_nonoverlapping(crate::phys_to_virt(pa) as *const u8, dst[off..].as_mut_ptr(), n) }
    })
}

pub fn copy_to_user(root: u64, dst: u64, src: &[u8]) -> Result<(), Efault> {
    check(dst, src.len())?;
    if src.is_empty() {
        return Ok(());
    }
    if fast_ok(root) {
        return fast(dst, src.len(), Op::ToUser(src.as_ptr()));
    }
    for_each_page(root, dst, src.len(), true, |pa, off, n| {
        // SAFETY: as copy_from_user, writable user frame.
        unsafe { core::ptr::copy_nonoverlapping(src[off..].as_ptr(), crate::phys_to_virt(pa) as *mut u8, n) }
    })
}

/// As `copy_to_user` but also into read-only user pages (program loading); still only USER pages. Always
/// walks (the fast path would fault on read-only pages, as CR0.WP is set).
pub fn copy_to_user_force(root: u64, dst: u64, src: &[u8]) -> Result<(), Efault> {
    for_each_page(root, dst, src.len(), false, |pa, off, n| {
        // SAFETY: as copy_to_user; the frame is a user frame of `root` (USER bit checked).
        unsafe { core::ptr::copy_nonoverlapping(src[off..].as_ptr(), crate::phys_to_virt(pa) as *mut u8, n) }
    })
}

/// Fills user memory with zeros.
pub fn zero_user(root: u64, dst: u64, len: usize) -> Result<(), Efault> {
    check(dst, len)?;
    if len == 0 {
        return Ok(());
    }
    if fast_ok(root) {
        return fast(dst, len, Op::Zero);
    }
    for_each_page(root, dst, len, true, |pa, _off, n| {
        // SAFETY: as copy_to_user.
        unsafe { core::ptr::write_bytes(crate::phys_to_virt(pa) as *mut u8, 0, n) }
    })
}

/// Why `strncpy_from_user` gave no string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrError {
    /// The memory is not readable user memory (EFAULT).
    Fault,
    /// No NUL within `max` bytes (paths: ENAMETOOLONG).
    TooLong,
}

impl From<Efault> for StrError {
    fn from(_: Efault) -> Self {
        StrError::Fault
    }
}

/// Reads a NUL-terminated string of fewer than `max` bytes (without the NUL) into `out`; returns its length.
/// Reads in pieces that never cross a page past the one holding the NUL, so a string ending right before an
/// unmapped page is fine. `max` bytes without a NUL is `TooLong` (Linux: PATH_MAX counts the NUL).
pub fn strncpy_from_user(root: u64, src: u64, out: &mut alloc::vec::Vec<u8>, max: usize) -> Result<usize, StrError> {
    out.clear();
    let mut buf = [0u8; 256];
    let mut a = src;
    while out.len() < max {
        let in_page = (4096 - (a & 0xFFF)) as usize;
        let n = in_page.min(buf.len()).min(max - out.len());
        copy_from_user(root, &mut buf[..n], a)?;
        let end = buf[..n].iter().position(|&b| b == 0);
        push_bytes(out, &buf[..end.unwrap_or(n)]);
        if end.is_some() {
            return Ok(out.len());
        }
        a += n as u64;
    }
    Err(StrError::TooLong)
}

/// Appends without `Vec<u8>::extend_from_slice`: hal must not instantiate that generic. With shared generics
/// the crates above hal (gopfb's per-pixel row building) then call hal's out-of-line copy instead of inlining
/// their own, which made every console present ~18x slower in QEMU (found in AB-1; DP open point in the notes).
fn push_bytes(out: &mut alloc::vec::Vec<u8>, s: &[u8]) {
    out.reserve(s.len());
    for &b in s {
        out.push(b);
    }
}

#[cfg(target_os = "none")]
mod imp {
    unsafe extern "C" {
        fn hal_uc_copy(dst: *mut u8, src: *const u8, len: usize, smap: u64) -> usize;
        fn hal_uc_zero(dst: *mut u8, len: usize, smap: u64) -> usize;
        static hal_uc_copy_insn: u8;
        static hal_uc_copy_resume: u8;
        static hal_uc_zero_insn: u8;
        static hal_uc_zero_resume: u8;
    }

    /// Copies `len` bytes; returns how many were not copied (0 = all).
    /// SAFETY (caller): dst and src valid for `len` bytes except for user-side addresses below USER_TOP whose
    /// faults the #PF fixup catches.
    pub unsafe fn copy(dst: *mut u8, src: *const u8, len: usize, smap: bool) -> usize {
        // SAFETY: the caller's contract; the assembly touches only dst/src for len bytes and rcx/rsi/rdi/r8/rax.
        unsafe { hal_uc_copy(dst, src, len, smap as u64) }
    }

    /// Zeroes `len` bytes; returns how many were not zeroed. SAFETY (caller): as `copy`.
    pub unsafe fn zero(dst: *mut u8, len: usize, smap: bool) -> usize {
        // SAFETY: as `copy`.
        unsafe { hal_uc_zero(dst, len, smap as u64) }
    }

    pub fn resume_for(rip: u64) -> Option<u64> {
        let (ci, cr, zi, zr) =
            (&raw const hal_uc_copy_insn as u64, &raw const hal_uc_copy_resume as u64, &raw const hal_uc_zero_insn as u64, &raw const hal_uc_zero_resume as u64);
        if rip == ci {
            Some(cr)
        } else if rip == zi {
            Some(zr)
        } else {
            None
        }
    }
}

// The fault on `rep movsb`/`rep stosb` is precise: rcx holds the bytes left at the faulting iteration, so the
// fixup resumes at the label after the instruction, which drops AC again and returns rcx. DF is clear (ABI).
#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .text
    # hal_uc_copy(dst: rdi, src: rsi, len: rdx, smap: rcx) -> bytes not copied
    .global hal_uc_copy
hal_uc_copy:
    movq %rcx, %r8
    movq %rdx, %rcx
    testq %r8, %r8
    jz 1f
    stac
1:
    .global hal_uc_copy_insn
hal_uc_copy_insn:
    rep movsb
    .global hal_uc_copy_resume
hal_uc_copy_resume:
    testq %r8, %r8
    jz 2f
    clac
2:
    movq %rcx, %rax
    ret

    # hal_uc_zero(dst: rdi, len: rsi, smap: rdx) -> bytes not zeroed
    .global hal_uc_zero
hal_uc_zero:
    movq %rdx, %r8
    movq %rsi, %rcx
    xorl %eax, %eax
    testq %r8, %r8
    jz 1f
    stac
1:
    .global hal_uc_zero_insn
hal_uc_zero_insn:
    rep stosb
    .global hal_uc_zero_resume
hal_uc_zero_resume:
    testq %r8, %r8
    jz 2f
    clac
2:
    movq %rcx, %rax
    ret
"#,
    options(att_syntax)
);

#[cfg(not(target_os = "none"))]
mod imp {
    pub unsafe fn copy(_dst: *mut u8, _src: *const u8, len: usize, _smap: bool) -> usize {
        len
    }
    pub unsafe fn zero(_dst: *mut u8, len: usize, _smap: bool) -> usize {
        len
    }
    pub fn resume_for(_rip: u64) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_check_refuses_kernel_and_wrapping_ranges() {
        assert!(check(0x40_0000, 16).is_ok());
        assert!(check(crate::USER_TOP - 8, 8).is_ok());
        assert!(check(crate::USER_TOP - 8, 9).is_err());
        assert!(check(crate::USER_TOP, 0).is_ok());
        assert!(check(0xFFFF_8880_0000_0000, 1).is_err());
        assert!(check(u64::MAX - 3, 8).is_err());
    }

    #[test]
    fn host_build_has_no_fast_path() {
        install_fixup();
        assert!(!stats().fast_path);
    }
}
