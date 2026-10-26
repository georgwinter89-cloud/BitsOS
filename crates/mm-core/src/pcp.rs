//! Per-CPU lists of single frames (Linux `per_cpu_pages`): most allocations and frees of one page touch only
//! this CPU's list, and the zone lock is taken once per `BATCH` frames.
//!
//! The list's storage is reserved when the allocator is built and never grows, so allocating or freeing a
//! frame never calls into the heap (the heap calls the frame allocator under its own lock). `HIGH` and
//! `BATCH` are tuning values for the cache size, not limits on anything that exists (R1): a frame that does
//! not fit simply goes to the buddy lists.
//!
//! CPUs that come online after the allocator was built (CP-4) have no list yet; `PerCpu::get_for` returns
//! `None` for them and they use the buddy lists directly.

use crate::buddy::ZONES;
use alloc::vec::Vec;
use ksync::IrqSpinLock;

/// Frames moved between a list and the buddy lists at once (Linux uses 63 for large zones).
pub const BATCH: usize = 32;
/// A list longer than this gives `BATCH` frames back.
pub const HIGH: usize = 6 * BATCH;

pub struct PcpCpu {
    /// One list per zone, so a frame always goes back to the zone it came from.
    pub lists: [IrqSpinLock<Vec<u64>>; ZONES],
}

pub fn new_cpu(_cpu: usize) -> PcpCpu {
    PcpCpu { lists: core::array::from_fn(|_| IrqSpinLock::named("frames.pcp", Vec::with_capacity(HIGH + BATCH))) }
}
