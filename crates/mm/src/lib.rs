//! Address spaces, VMAs, faults and DMA (owner: MM; contracts `C: AddressSpace / Vma`, `C: DmaPool`).
//!
//! MM-5a: per-process address spaces (`aspace`) with a VMA tree (`vma`), Linux's anonymous-memory semantics
//! (`linux`), pages filled on the first touch through `AddressSpace::handle_user_fault` (`fault`) or at map
//! time for callers that do not route user faults yet, and user copies that fill what they touch. `abi` runs
//! the memory-call scripts that are compared with CachyOS (`abi/`). MM-5b: copy-on-write for fork (`share`
//! counts), strict commit (`commit`, R2), shared anonymous memory, device VMAs with a fault hook and WC/UC pages
//! (`device`, GX-D7), `madvise`/`mremap`/`mincore`, per-process numbers (`MemStats`). MM-6: the TLB shootdown (`hal::paging::tlb`, set up and
//! reported by `tlb`): unmaps and protection changes reach every CPU that has the root loaded before frames
//! and tables are freed, PCID per CPU, lazy TLB; the leak paths of MM-5a (`as.drop_leaked`, vmap's
//! quarantine of dropped windows) remain only as the fallback for a CPU that never answers.
//!
//! MM-2: `kernel_tables` switches to the kernel's own page tables (plan in `kmap`, MTRR rules in `mtrr`),
//! `vmap` hands out typed device windows with guard pages and plain kernel-virtual reservations.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod abi;
mod as_ktest;
mod aspace;
pub mod commit;
mod commit_ktest;
mod counters_ktest;
mod cow_ktest;
pub mod device;
mod device_ktest;
pub mod dma;
mod dma_ktest;
pub mod fault;
mod frames_ktest;
mod heap_ktest;
pub mod kernel_tables;
pub mod kmap;
pub mod linux;
pub mod mtrr;
mod paging_ktest;
pub mod reclaim;
mod reclaim_ktest;
mod rmap_ktest;
mod report;
pub mod share;
mod spurious_ktest;
pub mod tlb;
mod tlb_grant_ktest;
mod tlb_idle_ktest;
mod tlb_ktest;
mod tlb_lazy_ktest;
mod tlb_root_ktest;
pub mod vma;
mod vaspace;
pub mod vmap;

pub use aspace::{AddressSpace, AsCore, Fill, MemStats, PageUsage, PSS_SHIFT};
pub use fault::FaultReason;
pub use vma::{FileRef, Prot, Vma, VmaKind};

use hal::paging;

pub const PAGE: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmError {
    /// No memory for the mapping (ENOMEM to the asker; never kill a running program, R2).
    NoMemory,
    /// Address range invalid or outside user space (EINVAL).
    Invalid,
    /// Overlaps an existing mapping where that is not allowed (EEXIST for MAP_FIXED_NOREPLACE).
    Exists,
    /// Bad user pointer (EFAULT).
    Fault,
}

/// Where `mmap` without a usable hint places mappings (top-down below this, like Linux's `mmap_base`).
pub const MMAP_TOP: u64 = 0x0000_7F00_0000_0000;

/// The kernel root as recorded at the first init level (the loader's PML4 if the switch had not happened
/// yet). `hal::paging::kernel_root()`, set by the switch to the kernel's own tables (`kernel_tables`), wins
/// over it. User roots copy their kernel half from the kernel root, and a dying root that is still loaded is
/// swapped for it (only a recorded value: without one `drop` leaks instead of freeing a loaded root).
static KERNEL_ROOT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn record_kernel_root() -> Result<(), &'static str> {
    KERNEL_ROOT.store(paging::current_root(), core::sync::atomic::Ordering::Relaxed);
    Ok(())
}
kreg::initcall!(kreg::Level::Core, "mm.kernel_root", record_kernel_root);

pub(crate) fn recorded_kernel_root() -> u64 {
    paging::kernel_root().unwrap_or_else(|| KERNEL_ROOT.load(core::sync::atomic::Ordering::Relaxed))
}

pub(crate) fn kernel_root() -> u64 {
    match recorded_kernel_root() {
        0 => paging::current_root(),
        r => r,
    }
}
