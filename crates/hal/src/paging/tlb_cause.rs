//! MM-11: shootdowns counted by what asked for them, so a storm of shootdown IPIs (YT-4a/YT-5: 50-78k per
//! second on single CPUs while Brave and Cobalt play video) names its source. Every initiator (`tlb::flush_user`,
//! `flush_kernel`, `leave`) passes its cause; the counters are atomics only (initiators run with interrupts
//! off), read through `tlb::stats().by_cause`.

use super::tlb::Outcome;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Why a shootdown ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// munmap, and what a failed populate takes back.
    Munmap,
    /// mmap(MAP_FIXED) over existing pages.
    MapFixed,
    /// mremap: the moved range's old entries, a shrink.
    Mremap,
    Mprotect,
    /// madvise(DONTNEED, DONTNEED_LOCKED, FREE).
    Madvise,
    /// A copy-on-write break: the shared frame replaced by the copy.
    Cow,
    /// Fault paths that change a present entry (a write grant that must keep its round).
    Fault,
    /// Page-cache frames taken out of mappings: truncate, hole punch, reclaim.
    Reclaim,
    /// fork write-protecting the parent's private pages.
    Fork,
    /// Device mappings: write-protect for dirty tracking, zap of a device's pages.
    Device,
    /// An address space ends (`leave`).
    Exit,
    /// Kernel vmap windows (`unmap_kernel`).
    Vmap,
    /// Other kernel-half changes: stacks, the CPU area, the direct map's memory types.
    Kernel,
    /// User-half changes of a single page outside the paths above.
    Other,
}

pub const CAUSES: usize = 14;

impl Cause {
    pub const ALL: [Cause; CAUSES] = [
        Cause::Munmap,
        Cause::MapFixed,
        Cause::Mremap,
        Cause::Mprotect,
        Cause::Madvise,
        Cause::Cow,
        Cause::Fault,
        Cause::Reclaim,
        Cause::Fork,
        Cause::Device,
        Cause::Exit,
        Cause::Vmap,
        Cause::Kernel,
        Cause::Other,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Cause::Munmap => "munmap",
            Cause::MapFixed => "map_fixed",
            Cause::Mremap => "mremap",
            Cause::Mprotect => "mprotect",
            Cause::Madvise => "madvise",
            Cause::Cow => "cow",
            Cause::Fault => "fault",
            Cause::Reclaim => "reclaim",
            Cause::Fork => "fork",
            Cause::Device => "device",
            Cause::Exit => "exit",
            Cause::Vmap => "vmap",
            Cause::Kernel => "kernel",
            Cause::Other => "other",
        }
    }
}

/// Totals since boot for one cause.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CauseStats {
    /// Shootdowns started (each raised a generation).
    pub flushes: u64,
    /// Those that had other CPUs to ask (a round).
    pub rounds: u64,
    /// Shootdown IPIs sent for them, resends included.
    pub ipis: u64,
    /// Flushes that freed page tables, so CPUs holding the root lazily were asked too (kernel causes: full
    /// flushes, every CPU is asked anyway).
    pub tables: u64,
}

struct Slot {
    flushes: AtomicU64,
    rounds: AtomicU64,
    ipis: AtomicU64,
    tables: AtomicU64,
}

static BY: [Slot; CAUSES] =
    [const { Slot { flushes: AtomicU64::new(0), rounds: AtomicU64::new(0), ipis: AtomicU64::new(0), tables: AtomicU64::new(0) } }; CAUSES];

pub(super) fn count(cause: Cause, out: &Outcome, tables: bool) {
    let s = &BY[cause as usize];
    s.flushes.fetch_add(1, Relaxed);
    if out.targets > 0 {
        s.rounds.fetch_add(1, Relaxed);
    }
    if out.ipis > 0 {
        s.ipis.fetch_add(out.ipis as u64, Relaxed);
    }
    if tables {
        s.tables.fetch_add(1, Relaxed);
    }
}

pub(super) fn by_cause() -> [CauseStats; CAUSES] {
    core::array::from_fn(|i| {
        let s = &BY[i];
        CauseStats { flushes: s.flushes.load(Relaxed), rounds: s.rounds.load(Relaxed), ipis: s.ipis.load(Relaxed), tables: s.tables.load(Relaxed) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_in_order_with_unique_names() {
        for (i, c) in Cause::ALL.iter().enumerate() {
            assert_eq!(*c as usize, i, "{c:?} out of place");
            assert!(Cause::ALL[i + 1..].iter().all(|d| d.name() != c.name()), "{} twice", c.name());
        }
    }

    #[test]
    fn rounds_and_ipis_only_when_others_were_asked() {
        let before = by_cause()[Cause::Other as usize];
        count(Cause::Other, &Outcome::default(), false);
        count(Cause::Other, &Outcome { targets: 3, ipis: 4, ..Outcome::default() }, true);
        let d = by_cause()[Cause::Other as usize];
        assert_eq!((d.flushes - before.flushes, d.rounds - before.rounds, d.ipis - before.ipis, d.tables - before.tables), (2, 1, 4, 1));
    }
}
