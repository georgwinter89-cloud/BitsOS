//! The kernel-virtual-space hook (MM-5a): `mm::vmap` installs its reserve/release pair here so crates below
//! `mm` (sched's kernel stacks) can take vmap ranges without depending on `mm`. A reservation is address
//! space only, with an unmapped guard page below it; the caller maps into it (`map_4k` into the kernel half)
//! and unmaps what it mapped before releasing it.

use core::sync::atomic::{AtomicUsize, Ordering};

/// `len` bytes (rounded up to pages) of vmap space with an unmapped guard page below; returns the first
/// usable address.
pub type KernelVaReserve = fn(len: u64) -> Option<u64>;
/// Gives back a range `reserve` returned (same `len`): removes whatever is still mapped in it, frees the page
/// tables that became empty, and makes the range reusable. False when it is kept out of use instead (another
/// CPU may still cache it until MM-6's shootdown, or it was not a live reservation).
pub type KernelVaRelease = fn(base: u64, len: u64) -> bool;

static RESERVE: AtomicUsize = AtomicUsize::new(0);
static RELEASE: AtomicUsize = AtomicUsize::new(0);

/// One-shot: the first call installs the pair, later calls change nothing and return false.
pub fn set_kernel_va(reserve: KernelVaReserve, release: KernelVaRelease) -> bool {
    // RELEASE claims the slot; RESERVE is stored after it and is what callers check first, so a reader that
    // sees RESERVE also sees the RELEASE of the same call.
    if RELEASE.compare_exchange(0, release as usize, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return false;
    }
    RESERVE.store(reserve as usize, Ordering::Release);
    true
}

/// A vmap reservation (`None` before MM installed its allocator or when no space is left).
pub fn reserve_kernel_va(len: u64) -> Option<u64> {
    let r = RESERVE.load(Ordering::Acquire);
    if r == 0 {
        return None;
    }
    // SAFETY: only `set_kernel_va` stores a non-zero value into RESERVE, once, always a valid `KernelVaReserve`.
    let f: KernelVaReserve = unsafe { core::mem::transmute::<usize, KernelVaReserve>(r) };
    f(len)
}

/// Releases a reservation (see `KernelVaRelease`); false before MM installed its allocator.
pub fn release_kernel_va(base: u64, len: u64) -> bool {
    if RESERVE.load(Ordering::Acquire) == 0 {
        return false;
    }
    let r = RELEASE.load(Ordering::Acquire);
    // SAFETY: RESERVE is set only after RELEASE (`set_kernel_va`), so RELEASE holds a valid `KernelVaRelease`.
    let f: KernelVaRelease = unsafe { core::mem::transmute::<usize, KernelVaRelease>(r) };
    f(base, len)
}

/// Whether MM installed its allocator.
pub fn kernel_va_ready() -> bool {
    RESERVE.load(Ordering::Acquire) != 0
}
