//! Who takes the reserve: takes per order, frames per kind, the takes of callers on their last try, and the call
//! chains (`callers`), for the boot allocator as `metrics` counters and `frames.highatomic_taker` events.
//!
//! Mini-PC 12.11.2026: `mm.frames.highatomic_allocs` went from 1 to 286 in a minute and 25 s later nothing was left
//! for a 5-page block under a spin lock; no number said which orders, kinds or callers had it.

use super::super::{Buddy, PB, ZONES};
use crate::callers::{site_event, RESERVE_TAKERS as TAKERS};
use crate::{Kind, KINDS};
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

klog::counter!(pub(super) HIGHATOMIC_ALLOCS, "mm.frames.highatomic_allocs");
// Frames ever taken (`highatomic_free` goes down and up).
klog::counter!(TAKEN_PAGES, "mm.frames.highatomic_pages");
// Takes of heap callers that may wait, on the try after which null is next.
klog::counter!(LAST_TRIES, "mm.frames.highatomic_last_try");
klog::counter!(O0, "mm.frames.highatomic_order0");
klog::counter!(O1, "mm.frames.highatomic_order1");
klog::counter!(O2, "mm.frames.highatomic_order2");
klog::counter!(O3, "mm.frames.highatomic_order3");
klog::counter!(O4, "mm.frames.highatomic_order4");
klog::counter!(O5, "mm.frames.highatomic_order5");
klog::counter!(O6, "mm.frames.highatomic_order6");
klog::counter!(O7, "mm.frames.highatomic_order7");
klog::counter!(O8, "mm.frames.highatomic_order8");
klog::counter!(K_HEAP, "mm.frames.highatomic_pages_heap");
klog::counter!(K_PAGETABLE, "mm.frames.highatomic_pages_pagetable");
klog::counter!(K_DMA, "mm.frames.highatomic_pages_dma");
klog::counter!(K_STACK, "mm.frames.highatomic_pages_stack");
klog::counter!(K_DRIVER, "mm.frames.highatomic_pages_driver");
klog::counter!(K_OTHER, "mm.frames.highatomic_pages_other");
// Takes whose call chain found the takers' table full: counted above, not named by an event.
klog::counter!(UNLISTED, "mm.frames.highatomic_taker_unlisted");
// Pageblocks made part of the reserve after boot, and reserve pageblocks handed to the type living in them.
klog::counter!(pub(super) REFILLS, "mm.frames.highatomic_refills");
klog::counter!(pub(super) HANDED_OVER, "mm.frames.highatomic_handed_over");
// Refill passes that found no free pageblock and could not make one.
klog::counter!(pub(super) REFILL_SHORT, "mm.frames.highatomic_refill_short");

const _: () = assert!(PB == 9, "one counter per order below the pageblock order");
static BY_ORDER: [&klog::metrics::Counter; PB] = [&O0, &O1, &O2, &O3, &O4, &O5, &O6, &O7, &O8];

/// The counter of the frames `kind` took (program and cache memory never take: they share `other`, which then
/// shows a broken rule).
fn kind_counter(kind: Kind) -> &'static klog::metrics::Counter {
    match kind {
        Kind::Heap => &K_HEAP,
        Kind::PageTable => &K_PAGETABLE,
        Kind::Dma => &K_DMA,
        Kind::Stack => &K_STACK,
        Kind::Driver => &K_DRIVER,
        Kind::User | Kind::PageCache | Kind::Other => &K_OTHER,
    }
}

/// The reserve's numbers of one allocator.
pub(in super::super) struct Reserve {
    /// Pageblocks each zone's reserve should have (what it was made with).
    pub(super) target: [AtomicU64; ZONES],
    takes: [AtomicU64; PB],
    pages: [AtomicU64; KINDS],
    last_tries: AtomicU64,
    pub(super) refilled: AtomicU64,
    pub(super) handed: AtomicU64,
}

/// A copy of an allocator's reserve numbers (`Buddy::reserve_stats`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReserveStats {
    /// Pageblocks per zone: wanted, there, and the free frames in them.
    pub target: [u64; ZONES],
    pub pageblocks: [u64; ZONES],
    pub free: [u64; ZONES],
    /// Takes per block order.
    pub takes: [u64; PB],
    /// Frames taken per kind (`Kind as usize`).
    pub pages: [u64; KINDS],
    /// Of the takes, those of heap callers that may wait, on their last try.
    pub last_tries: u64,
    /// Pageblocks made part of the reserve after it was first made, and handed over to another type.
    pub refilled: u64,
    pub handed: u64,
}

impl ReserveStats {
    pub fn total_takes(&self) -> u64 {
        self.takes.iter().sum()
    }
    pub fn total_pages(&self) -> u64 {
        self.pages.iter().sum()
    }
}

impl Reserve {
    pub(in super::super) const fn new() -> Reserve {
        Reserve {
            target: [const { AtomicU64::new(0) }; ZONES],
            takes: [const { AtomicU64::new(0) }; PB],
            pages: [const { AtomicU64::new(0) }; KINDS],
            last_tries: AtomicU64::new(0),
            refilled: AtomicU64::new(0),
            handed: AtomicU64::new(0),
        }
    }
}

impl Buddy {
    /// A block of `order` left the reserve for `kind` (no allocator lock held; the heap's may be).
    #[cold]
    pub(super) fn count_take(&self, order: u32, kind: Kind, last_try: bool) {
        let r = &self.ha;
        r.takes[order as usize].fetch_add(1, Relaxed);
        r.pages[kind as usize].fetch_add(1 << order, Relaxed);
        if last_try {
            r.last_tries.fetch_add(1, Relaxed);
        }
        if self.global_reports {
            HIGHATOMIC_ALLOCS.inc();
            TAKEN_PAGES.add(1 << order);
            BY_ORDER[order as usize].inc();
            kind_counter(kind).add(1 << order);
            if last_try {
                LAST_TRIES.inc();
            }
            TAKERS.note(u64::from(order) | (kind as u64) << 8 | u64::from(last_try) << 16, 1 << order);
        }
    }

    /// The reserve's numbers now.
    pub fn reserve_stats(&self) -> ReserveStats {
        let r = &self.ha;
        let zones: [super::super::ZoneInfo; ZONES] = core::array::from_fn(|z| self.zone_info(z));
        ReserveStats {
            target: core::array::from_fn(|z| r.target[z].load(Relaxed)),
            pageblocks: core::array::from_fn(|z| zones[z].pageblocks[super::MigrateType::HighAtomic as usize]),
            free: core::array::from_fn(|z| zones[z].free_highatomic),
            takes: core::array::from_fn(|o| r.takes[o].load(Relaxed)),
            pages: core::array::from_fn(|k| r.pages[k].load(Relaxed)),
            last_tries: r.last_tries.load(Relaxed),
            refilled: r.refilled.load(Relaxed),
            handed: r.handed.load(Relaxed),
        }
    }
}

/// Events for the takers that are new or doubled (`report::flush_reports`: no lock held).
pub(crate) fn flush() {
    UNLISTED.add(TAKERS.dropped_news());
    TAKERS.each_news(&mut |s| {
        site_event!(Warn, "frames.highatomic_taker", s, pages = s.amount, order = s.info & 0xFF, kind = (s.info >> 8) & 0xFF, last_try = (s.info >> 16) & 1;
            "took from the HighAtomic reserve (c0..: return addresses, innermost first)");
    });
}
