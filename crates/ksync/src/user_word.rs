//! Atomic access to one aligned 32-bit word of the **running thread's own** user address space without taking
//! a page fault (SC-5: futexes; Linux `futex_get_value_locked`, `futex_atomic_cmpxchg_inatomic` and
//! `arch_futex_atomic_op_inuser` under `pagefault_disable`).
//!
//! The futex code needs these while it holds a hash-bucket spin lock, where a page fault (which may sleep in
//! MM's lazy fill) is not allowed. Linux does the access through the user mapping with page faults disabled
//! and an exception-table fixup; V2 does it through the **direct map** instead:
//!
//! 1. interrupts off (an `IrqGuard`);
//! 2. `root` must be the address space loaded in CR3 on this CPU (the caller's own process): then this CPU is
//!    in the root's TLB-shootdown mask (MM-6, `hal::paging::tlb`). With interrupts off this CPU cannot answer a
//!    shootdown IPI, so the page-table walk below and the frame it finds stay valid until interrupts are on
//!    again (the "IPI as a read lock" argument of Linux's `get_user_pages_fast`). This rests on an **MM
//!    obligation** (CONTRACTS section 3, MM row, asked for by SC-5 change request 6): a frame or page table that
//!    was reachable from user root R is freed or reused only after a shootdown round for R was confirmed by
//!    every CPU that has R loaded, lazily too; an unconfirmed or given-up round keeps them until
//!    `AddressSpace::drop` after a confirmed leave (`as.drop_leaked` otherwise). A flush that does not need the
//!    target CPU's interrupts (AMD INVLPGB broadcast) must keep this guarantee by deferring the free (Linux needed
//!    RCU table freeing for `gup_fast` once it had such flushes). Any MM change to freeing, reclaim, COW or the
//!    flush mechanism must keep it, or this module becomes unsound;
//! 3. the leaf must be present, USER, write-back memory (and WRITABLE for anything that writes): a read-only
//!    page (copy-on-write after MM-5b, a clean page of a dirty-tracked file mapping) or a missing one is
//!    `Fault`, and the caller drops its locks, faults the page in through MM and retries (Linux's
//!    `fault_in_user_writeable`), so MM's rules for those pages are never bypassed;
//! 4. the frame's direct-map address must map that very frame as write-back RAM (no alias with another memory
//!    type, no device memory).
//!
//! The word is then changed with the machine instructions Linux uses (`arch/x86/include/asm/futex.h`: `mov`,
//! `xchg`, `lock xadd`, `lock cmpxchg` and a `lock cmpxchg` loop) in inline assembly, never through a Rust
//! reference: the frame is user memory that holds no Rust object, and other kernel paths (page-cache writes to
//! a shared tmpfs or /dev/shm file, the user-copy slow path, `mm_core::alloc_copy`) may write it with plain
//! stores at the same time. A locked instruction is atomic against any other access on x86, so the worst such a
//! race gives is a value mix the futex protocol already tolerates (Linux has the same race). Nothing is cached
//! from the word; the caller decides under its own lock what the value means.

use core::sync::atomic::{AtomicU32, Ordering};

/// Why an access did not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordError {
    /// Not mapped as needed right now (missing, not USER, read-only for a write, not write-back RAM): fault it
    /// in with locks dropped and retry, or answer EFAULT when MM refuses.
    Fault,
    /// `root` is not the address space loaded on this CPU: the direct-map rule does not hold here.
    NotLoaded,
    /// Misaligned or outside user space (EINVAL/EFAULT by the caller's rules).
    BadAddress,
    /// A read-modify-write lost `CONTENDED_LIMIT` races in a row (another CPU wrote the word every time): retry
    /// with locks dropped, as after a fault (a bound for interrupts-off code, B4; Linux loops without one).
    Contended,
}

/// How often the `lock cmpxchg` loop of Or/AndN/Xor may lose against other writers before it gives up.
pub const CONTENDED_LIMIT: u32 = 1 << 16;

/// The operation on the word. All return the value the word had before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordOp {
    /// Read only.
    Load,
    /// `lock cmpxchg`: stores `new` when the word equals `old`.
    CmpXchg { old: u32, new: u32 },
    /// FUTEX_OP_SET .. FUTEX_OP_XOR (Linux `arch_futex_atomic_op_inuser`).
    Set(u32),
    Add(u32),
    Or(u32),
    AndN(u32),
    Xor(u32),
}

impl WordOp {
    pub fn writes(self) -> bool {
        !matches!(self, WordOp::Load)
    }
}

/// Performs `op` on the u32 at user address `addr` of `root` (see the module comment for the rules).
pub fn access(root: u64, addr: u64, op: WordOp) -> Result<u32, WordError> {
    if addr % 4 != 0 || addr.checked_add(4).is_none_or(|e| e > hal::USER_TOP) {
        return Err(WordError::BadAddress);
    }
    imp::access(root, addr, op)
}

/// Applies `op` to an atomic in kernel memory (the same semantics as `access`; used by tests that model user
/// memory with kernel buffers).
pub fn apply(w: &AtomicU32, op: WordOp) -> u32 {
    match op {
        WordOp::Load => w.load(Ordering::SeqCst),
        WordOp::CmpXchg { old, new } => match w.compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(v) | Err(v) => v,
        },
        WordOp::Set(v) => w.swap(v, Ordering::SeqCst),
        WordOp::Add(v) => w.fetch_add(v, Ordering::SeqCst),
        WordOp::Or(v) => w.fetch_or(v, Ordering::SeqCst),
        WordOp::AndN(v) => w.fetch_and(!v, Ordering::SeqCst),
        WordOp::Xor(v) => w.fetch_xor(v, Ordering::SeqCst),
    }
}

#[cfg(target_os = "none")]
mod imp {
    use super::{CONTENDED_LIMIT, WordError, WordOp};
    use core::arch::asm;
    use hal::paging::{self, CacheType, PageFlags};

    pub fn access(root: u64, addr: u64, op: WordOp) -> Result<u32, WordError> {
        let _irq = hal::cpu::IrqGuard::new();
        if paging::current_root() != root {
            return Err(WordError::NotLoaded);
        }
        let (phys, fl) = paging::translate(root, addr).ok_or(WordError::Fault)?;
        let need = PageFlags::USER | if op.writes() { PageFlags::WRITABLE } else { 0 };
        if fl.0 & need != need || fl.cache() != Some(CacheType::WriteBack) {
            return Err(WordError::Fault);
        }
        let virt = hal::phys_to_virt(phys);
        // The direct map must hold this frame as write-back memory (RAM, not retyped for a device).
        match paging::translate(root, virt) {
            Some((p, f)) if p == phys && f.cache() == Some(CacheType::WriteBack) && f.0 & PageFlags::WRITABLE != 0 => {}
            _ => return Err(WordError::Fault),
        }
        let p = virt as *mut u32;
        let rmw = |f: fn(u32, u32) -> u32, arg: u32| -> Result<u32, WordError> {
            let mut old = load(p);
            for _ in 0..CONTENDED_LIMIT {
                let cur = cmpxchg(p, old, f(old, arg));
                if cur == old {
                    return Ok(old);
                }
                old = cur;
            }
            Err(WordError::Contended)
        };
        match op {
            WordOp::Load => Ok(load(p)),
            WordOp::CmpXchg { old, new } => Ok(cmpxchg(p, old, new)),
            WordOp::Set(v) => Ok(xchg(p, v)),
            WordOp::Add(v) => Ok(xadd(p, v)),
            WordOp::Or(v) => rmw(|o, a| o | a, v),
            WordOp::AndN(v) => rmw(|o, a| o & !a, v),
            WordOp::Xor(v) => rmw(|o, a| o ^ a, v),
        }
    }

    // The four instructions below are only called by `access` above, with `p` checked there. One SAFETY argument
    // covers all of them (SAFETY-W):
    // `p` is 4-byte aligned (addr is, and the page offset is kept by the translation) and is the direct-map
    // address of the frame behind the program's own present, USER, write-back leaf, mapped writable and
    // write-back in the kernel half (both checked in `access`). The frame and the tables walked stay allocated
    // while interrupts are off, because `root` is loaded on this CPU and MM frees a user frame or table only after
    // every CPU with the root loaded confirmed the shootdown (the MM obligation in the module comment, CONTRACTS
    // section 3 MM row). The instruction touches exactly these 4 bytes; no Rust reference to them exists, so
    // plain kernel stores to the same frame elsewhere (page cache, user-copy slow path) are no aliasing
    // violation, only a value race the futex protocol tolerates. No stack use; the default memory clobber keeps
    // the compiler from moving other memory accesses across it.

    fn load(p: *mut u32) -> u32 {
        let v: u32;
        // SAFETY: SAFETY-W above (a 4-byte read of the checked user frame through the direct map).
        unsafe { asm!("mov {v:e}, dword ptr [{p}]", p = in(reg) p, v = out(reg) v, options(nostack, preserves_flags, readonly)) };
        v
    }

    fn cmpxchg(p: *mut u32, old: u32, new: u32) -> u32 {
        let prev: u32;
        // SAFETY: SAFETY-W above (`lock cmpxchg` on the checked user frame through the direct map).
        unsafe {
            asm!("lock cmpxchg dword ptr [{p}], {new:e}", p = in(reg) p, new = in(reg) new, inout("eax") old => prev, options(nostack))
        };
        prev
    }

    fn xchg(p: *mut u32, v: u32) -> u32 {
        let mut v = v;
        // SAFETY: SAFETY-W above (`xchg` with memory is always locked).
        unsafe { asm!("xchg dword ptr [{p}], {v:e}", p = in(reg) p, v = inout(reg) v, options(nostack, preserves_flags)) };
        v
    }

    fn xadd(p: *mut u32, v: u32) -> u32 {
        let mut v = v;
        // SAFETY: SAFETY-W above (`lock xadd`, wrapping as Linux's FUTEX_OP_ADD).
        unsafe { asm!("lock xadd dword ptr [{p}], {v:e}", p = in(reg) p, v = inout(reg) v, options(nostack)) };
        v
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    use super::{WordError, WordOp};

    /// Host builds have no user address spaces.
    pub fn access(_root: u64, _addr: u64, _op: WordOp) -> Result<u32, WordError> {
        Err(WordError::NotLoaded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_return_the_old_value() {
        let w = AtomicU32::new(5);
        assert_eq!(apply(&w, WordOp::Load), 5);
        assert_eq!(apply(&w, WordOp::Add(3)), 5);
        assert_eq!(apply(&w, WordOp::Or(0x10)), 8);
        assert_eq!(apply(&w, WordOp::AndN(0x8)), 0x18);
        assert_eq!(apply(&w, WordOp::Xor(0xff)), 0x10);
        assert_eq!(apply(&w, WordOp::Set(7)), 0xef);
        assert_eq!(apply(&w, WordOp::CmpXchg { old: 1, new: 2 }), 7);
        assert_eq!(w.load(Ordering::SeqCst), 7);
        assert_eq!(apply(&w, WordOp::CmpXchg { old: 7, new: 2 }), 7);
        assert_eq!(w.load(Ordering::SeqCst), 2);
        // Wrapping add as the hardware's `lock xadd`.
        let w = AtomicU32::new(u32::MAX);
        assert_eq!(apply(&w, WordOp::Add(2)), u32::MAX);
        assert_eq!(w.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bad_addresses_are_refused_before_any_access() {
        assert_eq!(access(0, 2, WordOp::Load), Err(WordError::BadAddress));
        assert_eq!(access(0, hal::USER_TOP - 2, WordOp::Load), Err(WordError::BadAddress));
        assert_eq!(access(0, u64::MAX - 3, WordOp::Load), Err(WordError::BadAddress));
        assert_eq!(access(0, 0x1000, WordOp::Load), Err(WordError::NotLoaded));
    }
}
