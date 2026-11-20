//! L7-SYNC: the kernel half of the vDSO's robust-futex try-unlock (linux-7.3.1/include/linux/futex.h:125
//! `futex_fixup_robust_unlock`, kernel/futex/core.c:1452 `__futex_fixup_robust_unlock`, :1212
//! `robust_list_clear_pending`, arch/x86/include/asm/futex_robust.h). A thread that was interrupted between the
//! try-unlock's successful cmpxchg (ZF set) and its store of 0 into `list_op_pending` (the pointer in rdx) and
//! now gets a signal has that store done here first: a fatal signal must not leave its exit walk a pending
//! pointer to a lock it already gave away.
//!
//! Linux records the two ranges per address space when it maps the vDSO; V2 takes the image's address from the
//! program's AT_SYSINFO_EHDR and confirms with the VMA at rip, so a moved or unmapped vDSO is never mistaken.
//! Called on the way back to user mode before signals are delivered; a syscall return can never be inside a
//! range (no syscall instruction there), so it is cheap to call there too.

use crate::Process;
use crate::task::Thread;
use core::sync::atomic::Ordering;
use hal::entry::UserContext;

include!(concat!(env!("OUT_DIR"), "/vdso_syms.rs"));

/// RFLAGS.ZF.
const ZF: u64 = 1 << 6;
const AT_SYSINFO_EHDR: u64 = 33;
/// `list_op_pending`'s offset in a 64-bit `struct robust_list_head`.
const PENDING_OFF: u64 = 16;

/// Which critical section an offset into the image is in: Some(true) for the 32-bit pointer one. Pure.
pub fn section_of(off: u64) -> Option<bool> {
    if (CS64.0..CS64.1).contains(&off) {
        Some(false)
    } else if (CS32.0..CS32.1).contains(&off) {
        Some(true)
    } else {
        None
    }
}

/// The image offset of `rip` when `p`'s vDSO is mapped there.
fn image_offset(p: &Process, rip: u64) -> Option<u64> {
    let base = {
        let a = p.saved_auxv.lock();
        a.chunks(2).find(|kv| kv.first() == Some(&AT_SYSINFO_EHDR)).and_then(|kv| kv.get(1).copied())?
    };
    let off = rip.checked_sub(base)?;
    section_of(off)?;
    let v = p.mm.vma_at(rip)?;
    let f = v.file.as_ref().filter(|f| super::is_vdso(f) && f.pgoff >= 1)?;
    (v.start - (f.pgoff - 1) * 4096 == base).then_some(off)
}

/// `futex_fixup_robust_unlock` for thread `t` of `p` about to take a signal with the user state `ctx`: true when
/// it was inside a critical section with the cmpxchg done and the pending pointer was cleared (or could not be).
pub fn fixup_robust_unlock(p: &Process, t: &Thread, ctx: &UserContext) -> bool {
    let Some(off) = image_offset(p, ctx.regs.rip) else { return false };
    if ctx.regs.rflags & ZF == 0 {
        return false;
    }
    let list32 = section_of(off) == Some(true);
    let pop = ctx.regs.rdx;
    let n = if list32 { 4 } else { 8 };
    if p.mm.copy_to_user(pop, &[0u8; 8][..n]).is_err() && !list32 {
        // As `robust_list_clear_pending`: better a few stale futexes than a walk through a pointer that lies.
        let head = t.robust_list.load(Ordering::Acquire);
        if head != 0 && pop == head.wrapping_add(PENDING_OFF) {
            t.robust_list.store(0, Ordering::Release);
        }
    }
    true
}

#[path = "unlock_ktests.rs"]
mod unlock_ktests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_are_the_store_and_its_branch() {
        // Each section is the jnz (2 bytes) and the store: 3 bytes for movq through rdx, 2 for movl.
        assert_eq!(CS64.1 - CS64.0, 5, "{CS64:x?}");
        assert_eq!(CS32.1 - CS32.0, 4, "{CS32:x?}");
        assert_eq!(section_of(CS64.0), Some(false));
        assert_eq!(section_of(CS64.1 - 1), Some(false));
        assert_eq!(section_of(CS64.1), None, "the ret is outside");
        assert_eq!(section_of(CS64.0 - 1), None, "the cmpxchg is outside");
        assert_eq!(section_of(CS32.0), Some(true));
    }
}
