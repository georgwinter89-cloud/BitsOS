//! Deferred events of the frame allocator (B6) and its counters.
//!
//! The allocator may be called with the heap lock or klog's ring lock held (both allocate), so it must never
//! call `klog::event!` itself (R5: never call into klog while holding frames/heap). It counts what happened
//! here, lock-free, and `flush` turns the counts into events from a context that holds nothing: the
//! `mm.report` thread (crate `mm`), `stats` users and the ktests.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

klog::counter!(BAD_FREES, "mm.frames.bad_free");
klog::counter!(ALLOC_FAILS, "mm.frames.alloc_fail");
klog::counter!(DMA32_FALLBACKS, "mm.frames.dma32_fallback");
klog::counter!(PCP_DRAINS, "mm.frames.pcp_drain");
klog::counter!(HEAP_BAD_FREES, "mm.heap.bad_free");
klog::counter!(KIND_MISMATCHES, "mm.frames.kind_mismatch");
klog::counter!(CORRUPT, "mm.frames.corrupt");
klog::counter!(HEAP_ALLOC_FAILS, "mm.heap.alloc_fail");
klog::counter!(HEAP_CORRUPT, "mm.heap.corrupt");
klog::counter!(HEAP_RECLAIMS, "mm.heap.reclaim");

/// Why a `free` was refused. (A free with another kind than the allocation is accepted and counted apart,
/// `Reports::kind_mismatch`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BadFree {
    /// Not inside any pool region (KEEP, KERNEL, MODULE, a hole, the heap seed, beyond RAM).
    OutOfRange = 1,
    /// Address not aligned to the block size of `order`.
    Misaligned = 2,
    /// `order` above MAX_ORDER.
    BadOrder = 3,
    /// Inside a block, not its first frame.
    NotHead = 4,
    /// The block is already free (or parked in a per-CPU list).
    DoubleFree = 5,
    /// The block was handed out with a different order.
    OrderMismatch = 6,
    /// The block belongs to the heap (a slab, or a piece of a large heap run) and is not freed this way.
    HeapOwned = 7,
}

impl BadFree {
    pub fn name(self) -> &'static str {
        match self {
            BadFree::OutOfRange => "out_of_range",
            BadFree::Misaligned => "misaligned",
            BadFree::BadOrder => "bad_order",
            BadFree::NotHead => "not_block_head",
            BadFree::DoubleFree => "double_free",
            BadFree::OrderMismatch => "order_mismatch",
            BadFree::HeapOwned => "heap_owned",
        }
    }
    fn from_u8(v: u8) -> BadFree {
        match v {
            2 => BadFree::Misaligned,
            3 => BadFree::BadOrder,
            4 => BadFree::NotHead,
            5 => BadFree::DoubleFree,
            6 => BadFree::OrderMismatch,
            7 => BadFree::HeapOwned,
            _ => BadFree::OutOfRange,
        }
    }
}

/// Number of counters `flush` tracks (one `seen` slot each).
const SEEN: usize = 10;

/// Counts and the last detail of each kind of trouble; one per allocator instance.
#[derive(Default)]
pub struct Reports {
    /// The boot allocator: also bumps the global klog counters.
    global: bool,
    pub bad_frees: AtomicU64,
    bad_last_phys: AtomicU64,
    /// order | reason << 8 | kind << 16
    bad_last_info: AtomicU64,
    pub alloc_fails: AtomicU64,
    /// order | zone << 8
    fail_last: AtomicU64,
    pub fallbacks: AtomicU64,
    pub drains: AtomicU64,
    pub heap_bad_frees: AtomicU64,
    heap_last: AtomicU64,
    /// Frees accepted with another kind than the allocation (charged to the recorded one).
    pub kind_mismatches: AtomicU64,
    kind_last_phys: AtomicU64,
    /// order | given kind << 8 | recorded kind << 16
    kind_last_info: AtomicU64,
    /// Frames taken out of circulation after an inconsistent per-CPU list entry (a bug elsewhere).
    pub corrupt: AtomicU64,
    corrupt_last: AtomicU64,
    /// Heap allocations that got null (ENOMEM to the asker) after the caches gave back what they held.
    pub heap_alloc_fails: AtomicU64,
    heap_fail_size: AtomicU64,
    /// Heap objects found handed out twice (a bug elsewhere); kept out of use.
    pub heap_corrupt: AtomicU64,
    heap_corrupt_last: AtomicU64,
    /// Times an allocation emptied the magazines and returned empty slabs before retrying.
    pub heap_reclaims: AtomicU64,
    seen: [AtomicU64; SEEN],
    fallback_told: AtomicBool,
}

impl Reports {
    pub const fn new(global: bool) -> Reports {
        Reports {
            global,
            bad_frees: AtomicU64::new(0),
            bad_last_phys: AtomicU64::new(0),
            bad_last_info: AtomicU64::new(0),
            alloc_fails: AtomicU64::new(0),
            fail_last: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
            drains: AtomicU64::new(0),
            heap_bad_frees: AtomicU64::new(0),
            heap_last: AtomicU64::new(0),
            kind_mismatches: AtomicU64::new(0),
            kind_last_phys: AtomicU64::new(0),
            kind_last_info: AtomicU64::new(0),
            corrupt: AtomicU64::new(0),
            corrupt_last: AtomicU64::new(0),
            heap_alloc_fails: AtomicU64::new(0),
            heap_fail_size: AtomicU64::new(0),
            heap_corrupt: AtomicU64::new(0),
            heap_corrupt_last: AtomicU64::new(0),
            heap_reclaims: AtomicU64::new(0),
            seen: [const { AtomicU64::new(0) }; SEEN],
            fallback_told: AtomicBool::new(false),
        }
    }

    pub fn bad_free(&self, phys: u64, order: u32, kind: usize, why: BadFree) {
        self.bad_last_phys.store(phys, Relaxed);
        self.bad_last_info.store(order as u64 & 0xFF | (why as u64) << 8 | (kind as u64) << 16, Relaxed);
        self.bad_frees.fetch_add(1, Relaxed);
        if self.global {
            BAD_FREES.inc();
        }
    }
    pub fn alloc_fail(&self, order: u32, zone: usize) {
        self.fail_last.store(order as u64 & 0xFF | (zone as u64) << 8, Relaxed);
        self.alloc_fails.fetch_add(1, Relaxed);
        if self.global {
            ALLOC_FAILS.inc();
        }
    }
    pub fn fallback(&self) {
        self.fallbacks.fetch_add(1, Relaxed);
        if self.global {
            DMA32_FALLBACKS.inc();
        }
    }
    pub fn drain(&self) {
        self.drains.fetch_add(1, Relaxed);
        if self.global {
            PCP_DRAINS.inc();
        }
    }
    pub fn kind_mismatch(&self, phys: u64, order: u32, given: usize, recorded: usize) {
        self.kind_last_phys.store(phys, Relaxed);
        self.kind_last_info.store(order as u64 & 0xFF | (given as u64) << 8 | (recorded as u64) << 16, Relaxed);
        self.kind_mismatches.fetch_add(1, Relaxed);
        if self.global {
            KIND_MISMATCHES.inc();
        }
    }
    pub fn corrupt(&self, phys: u64) {
        self.corrupt_last.store(phys, Relaxed);
        self.corrupt.fetch_add(1, Relaxed);
        if self.global {
            CORRUPT.inc();
        }
    }
    pub fn heap_bad_free(&self, virt: u64) {
        self.heap_last.store(virt, Relaxed);
        self.heap_bad_frees.fetch_add(1, Relaxed);
        if self.global {
            HEAP_BAD_FREES.inc();
        }
    }

    pub fn heap_alloc_fail(&self, size: usize) {
        self.heap_fail_size.store(size as u64, Relaxed);
        self.heap_alloc_fails.fetch_add(1, Relaxed);
        if self.global {
            HEAP_ALLOC_FAILS.inc();
        }
    }
    pub fn heap_corrupt(&self, virt: usize) {
        self.heap_corrupt_last.store(virt as u64, Relaxed);
        self.heap_corrupt.fetch_add(1, Relaxed);
        if self.global {
            HEAP_CORRUPT.inc();
        }
    }
    pub fn heap_reclaim(&self) {
        self.heap_reclaims.fetch_add(1, Relaxed);
        if self.global {
            HEAP_RECLAIMS.inc();
        }
    }

    /// The reason of the last refused free (tests).
    pub fn last_bad_free(&self) -> Option<(u64, BadFree)> {
        (self.bad_frees.load(Relaxed) > 0)
            .then(|| (self.bad_last_phys.load(Relaxed), BadFree::from_u8((self.bad_last_info.load(Relaxed) >> 8) as u8)))
    }

    /// The last accepted free with a wrong kind: (phys, given kind, recorded kind) (tests).
    pub fn last_kind_mismatch(&self) -> Option<(u64, usize, usize)> {
        (self.kind_mismatches.load(Relaxed) > 0).then(|| {
            let i = self.kind_last_info.load(Relaxed);
            (self.kind_last_phys.load(Relaxed), ((i >> 8) & 0xFF) as usize, (i >> 16) as usize)
        })
    }

    fn delta(&self, i: usize, now: u64) -> u64 {
        let old = self.seen[i].swap(now, Relaxed);
        now.saturating_sub(old)
    }

    /// Emits an event for everything counted since the last flush. Call only while holding no frames/heap
    /// lock and not from inside klog. `who` names the instance ("frames" for the boot allocator).
    pub fn flush(&self, who: &'static str) {
        let n = self.delta(0, self.bad_frees.load(Relaxed));
        if n > 0 {
            let info = self.bad_last_info.load(Relaxed);
            klog::event!(Error, "mm", "frames.bad_free", allocator = who, count = n, last_phys = klog::Hex(self.bad_last_phys.load(Relaxed)),
                order = info & 0xFF, kind = info >> 16, reason = BadFree::from_u8((info >> 8) as u8).name(); "refused, nothing changed");
        }
        let n = self.delta(1, self.alloc_fails.load(Relaxed));
        if n > 0 {
            let l = self.fail_last.load(Relaxed);
            klog::event!(Warn, "mm", "frames.alloc_fail", allocator = who, count = n, last_order = l & 0xFF,
                last_zone = if l >> 8 == 0 { "dma32" } else { "normal" }; "ENOMEM to the asker");
        }
        let n = self.delta(2, self.fallbacks.load(Relaxed));
        if n > 0 && !self.fallback_told.swap(true, Relaxed) {
            klog::event!(Info, "mm", "frames.dma32_fallback", allocator = who, count = n; "NORMAL empty, served from DMA32 (counter mm.frames.dma32_fallback has the rest)");
        }
        let n = self.delta(3, self.drains.load(Relaxed));
        if n > 0 {
            klog::event!(Info, "mm", "frames.pcp_drain", allocator = who, count = n; "per-CPU lists drained to satisfy an allocation");
        }
        let n = self.delta(4, self.heap_bad_frees.load(Relaxed));
        if n > 0 {
            klog::event!(Error, "mm", "heap.bad_free", count = n, last_virt = klog::Hex(self.heap_last.load(Relaxed)); "refused, nothing changed");
        }
        let n = self.delta(5, self.kind_mismatches.load(Relaxed));
        if n > 0 {
            let info = self.kind_last_info.load(Relaxed);
            klog::event!(Warn, "mm", "frames.kind_mismatch", allocator = who, count = n, last_phys = klog::Hex(self.kind_last_phys.load(Relaxed)),
                order = info & 0xFF, given_kind = (info >> 8) & 0xFF, recorded_kind = info >> 16; "accepted, charged to the recorded owner");
        }
        let n = self.delta(6, self.corrupt.load(Relaxed));
        if n > 0 {
            klog::event!(Error, "mm", "frames.corrupt", allocator = who, count = n, last_phys = klog::Hex(self.corrupt_last.load(Relaxed));
                "per-CPU list held a frame in another state; frame kept out of use (leaked_bytes)");
        }
        let n = self.delta(7, self.heap_alloc_fails.load(Relaxed));
        if n > 0 {
            klog::event!(Warn, "mm", "heap.alloc_fail", count = n, last_size = self.heap_fail_size.load(Relaxed); "null to the asker after the caches gave back what they held");
        }
        let n = self.delta(8, self.heap_corrupt.load(Relaxed));
        if n > 0 {
            klog::event!(Error, "mm", "heap.corrupt", count = n, last_virt = klog::Hex(self.heap_corrupt_last.load(Relaxed));
                "an object was about to be handed out twice; kept out of use");
        }
        let n = self.delta(9, self.heap_reclaims.load(Relaxed));
        if n > 0 {
            klog::event!(Info, "mm", "heap.reclaim", count = n; "magazines emptied and empty slabs returned to satisfy an allocation");
        }
    }
}

/// Reports of the boot allocator, the kernel heap and the named slab caches.
pub static GLOBAL: Reports = Reports::new(true);

/// Emits the pending events of the boot allocator and the heap (see `Reports::flush`).
pub fn flush_reports() {
    GLOBAL.flush("frames");
}
