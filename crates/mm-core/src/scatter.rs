//! Large heap blocks that are only virtually contiguous (Linux 7.x `kvmalloc`, mm/util.c, over `vmalloc` and
//! `vfree_atomic`, mm/vmalloc.c). A block above `PAGE_ALLOC_COSTLY_ORDER` whose exact page run is not free is
//! built from single frames mapped side by side in the vmap area: after heavy file
//! traffic memory is scattered page cache, reclaim frees scattered frames and nothing compacts, so a 2 MiB run
//! can be missing while GBs are free. Every round of the allocation tries the run first, then single frames. mm-core sits below the vmap allocator: crate mm implements `Scatter` and
//! registers it once (`KernelHeap::set_scatter`).
//!
//! Only where reclaim may run (`reclaim::context_allows`: no spin lock, interrupts on, no handler, Linux's
//! sleepable GFP_KERNEL): the vmap bookkeeping takes small heap blocks under its spin locks, and a heap
//! allocation made there therefore never comes back here.
//!
//! Frees: where a TLB shootdown may be waited for (the same test) the block goes back at once. Elsewhere it is
//! pushed onto a lock-free list kept in the block's own first bytes (`vfree_atomic`'s llist) and drained by the
//! `mm.report` thread (woken through `Scatter::kick` where a wake-up is allowed), by `regain` (memory pressure)
//! and by the next scattered allocation or free in a good context. The entry starts with a tag that only a
//! block on the list carries, claimed atomically, so a second free of such a block is refused like any other
//! double free (an exact copy of the tag in a block's first word, 1 in 2^64, makes its free refused: a leak).

use crate::heap::KernelHeap;
use crate::PAGE;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};
use ksync::OnceRef;

/// Linux's `PAGE_ALLOC_COSTLY_ORDER`: up to 2^3 pages a contiguous run is cheap enough to insist on.
pub const COSTLY_ORDER: u32 = 3;

/// Crate mm's side: vmap ranges with single frames behind them.
pub trait Scatter: Sync {
    /// The addresses every block lies in (the vmap area; never the direct map).
    fn span(&self) -> (usize, usize);
    /// `pages` single `Kind::Heap` frames mapped writable and no-execute at a fresh kernel range with an unmapped
    /// guard page below; returns its first address. Never calls klog or the scheduler. None: nothing changed.
    fn alloc(&self, pages: u64) -> Option<usize>;
    /// Unmaps a block of `pages` from `alloc` and frees its frames and range; only where `context_allows`. False:
    /// not such a block, nothing changed.
    fn free(&self, v: usize, pages: u64) -> bool;
    /// Lock-free, from any context: whether `v` looks like a live block of `pages` (its pages are mapped heap
    /// frames, the guard below is not), before the heap writes its deferred-list entry into it.
    fn looks_live(&self, v: usize, pages: u64) -> bool;
    /// Wakes the thread that drains deferred frees; called outside spin locks and interrupt handlers only.
    fn kick(&self);
}

klog::counter!(SCATTER_ALLOCS, "mm.heap.scatter_allocs");
klog::counter!(SCATTER_FREES, "mm.heap.scatter_frees");
klog::counter!(SCATTER_DEFERRED, "mm.heap.scatter_deferred");
klog::counter!(SCATTER_PAGES, "mm.heap.scatter_pages");

/// The deferred-list entry, written into the first bytes of the freed block.
#[repr(C)]
struct Node {
    /// `tag_of(block)` while the block is on the list.
    tag: usize,
    next: usize,
    pages: u64,
}

fn tag_of(v: usize) -> usize {
    v ^ 0x5CA7_7E9D_DEFE_44ED
}

/// The first word of a block that `Scatter::alloc` just returned or `looks_live` confirmed.
fn tag_word(v: usize) -> &'static AtomicUsize {
    // SAFETY: `v` is the page-aligned first address of a mapped, writable scattered block (fresh from the hook
    // or checked by `looks_live`); only `AtomicUsize` accesses touch this word while the heap looks at it.
    unsafe { AtomicUsize::from_ptr(v as *mut usize) }
}

/// One heap's scattered blocks.
pub(crate) struct ScatterState {
    hook: OnceRef<dyn Scatter>,
    /// The deferred list's first block (0: empty).
    head: AtomicUsize,
    pub(crate) allocs: AtomicU64,
    pub(crate) frees: AtomicU64,
    pub(crate) deferred: AtomicU64,
    /// Pages of scattered blocks not given back yet (those on the deferred list included).
    pub(crate) held: AtomicU64,
    /// Of `held`, the pages on the deferred list.
    pub(crate) waiting: AtomicU64,
}

impl ScatterState {
    pub(crate) const fn new() -> Self {
        ScatterState {
            hook: OnceRef::new(),
            head: AtomicUsize::new(0),
            allocs: AtomicU64::new(0),
            frees: AtomicU64::new(0),
            deferred: AtomicU64::new(0),
            held: AtomicU64::new(0),
            waiting: AtomicU64::new(0),
        }
    }
}

fn wake_allowed() -> bool {
    ksync::preempt_count() == 0 && !hal::percpu::in_hard_irq()
}

impl KernelHeap {
    /// Installs crate mm's scattered blocks (once; false if one is installed already).
    pub fn set_scatter(&self, h: &'static dyn Scatter) -> bool {
        self.scatter.hook.set(h)
    }

    /// Whether `v` lies where scattered blocks lie.
    pub(crate) fn scattered(&self, v: usize) -> bool {
        self.scatter.hook.get().is_some_and(|h| {
            let (lo, hi) = h.span();
            v >= lo && v < hi
        })
    }

    /// Whether a large block of this shape may fall back to single frames here and now.
    pub(crate) fn may_scatter(&self, pages: u64, align_pages: u64) -> bool {
        align_pages == 1 && pages > 1 << COSTLY_ORDER && self.scatter.hook.get().is_some() && crate::reclaim::context_allows()
    }

    /// Linux 7.x `kvmalloc_node` falls back to vmalloc for anything above one page. Up to the costly order that is
    /// the late step here (`large_alloc`: after reclaim and compaction), where a missing run of a few frames
    /// would otherwise end in null and a kernel panic (Mini-PC 11.11.2026, 17296 bytes).
    pub(crate) fn may_scatter_late(&self, pages: u64, align_pages: u64) -> bool {
        align_pages == 1 && pages > 1 && pages <= 1 << COSTLY_ORDER && self.scatter.hook.get().is_some() && crate::reclaim::context_allows()
    }

    pub(crate) fn scatter_alloc(&self, pages: u64) -> Option<usize> {
        let h = self.scatter.hook.get()?;
        self.drain_scattered();
        let v = h.alloc(pages)?;
        // Frames are not cleared: the first one may still carry the tag of an earlier block at this address.
        tag_word(v).store(0, Relaxed);
        let s = &self.scatter;
        s.allocs.fetch_add(1, Relaxed);
        s.held.fetch_add(pages, Relaxed);
        if self.global {
            SCATTER_ALLOCS.inc();
            SCATTER_PAGES.add(pages);
        }
        self.large_bytes.fetch_add(pages * PAGE, Relaxed);
        self.large_blocks.fetch_add(1, Relaxed);
        Some(v)
    }

    /// Frees a scattered block (`large_free` routed it here by its address); false, nothing changed, if it is not one.
    pub(crate) fn scatter_free(&self, v: usize, pages: u64) -> bool {
        let Some(h) = self.scatter.hook.get() else { return false };
        // `looks_live` before any read of the block: only mapped heap frames of exactly this shape (not a kernel
        // stack's or a window's reservation, not a block freed already).
        if v == 0 || !(v as u64).is_multiple_of(PAGE) || !h.looks_live(v, pages) {
            return false;
        }
        if crate::reclaim::context_allows() {
            // On the deferred list already: freeing it here would unmap the drain's next entry.
            if tag_word(v).load(Acquire) == tag_of(v) || !h.free(v, pages) {
                return false;
            }
            self.scatter_gone(pages);
            self.drain_scattered();
        } else {
            if !self.push_deferred(v, pages) {
                return false;
            }
            if wake_allowed() {
                h.kick();
            }
        }
        self.large_bytes.fetch_sub(pages * PAGE, Relaxed);
        self.large_blocks.fetch_sub(1, Relaxed);
        true
    }

    fn scatter_gone(&self, pages: u64) {
        self.scatter.frees.fetch_add(1, Relaxed);
        self.scatter.held.fetch_sub(pages, Relaxed);
        if self.global {
            SCATTER_FREES.inc();
        }
    }

    /// False: the block is on the list already (a double free), nothing changed.
    fn push_deferred(&self, v: usize, pages: u64) -> bool {
        let (word, tag) = (tag_word(v), tag_of(v));
        let w = word.load(Relaxed);
        if w == tag || word.compare_exchange(w, tag, Relaxed, Relaxed).is_err() {
            return false;
        }
        let s = &self.scatter;
        s.deferred.fetch_add(1, Relaxed);
        s.waiting.fetch_add(pages, Relaxed);
        if self.global {
            SCATTER_DEFERRED.inc();
        }
        let node = v as *mut Node;
        let mut head = s.head.load(Relaxed);
        // SAFETY: `v` is a live scattered block of `pages` (at least 9) mapped writable pages that its owner
        // gives up here (dealloc's contract; `looks_live` confirmed the mapping), and this call won its tag: nobody
        // else writes it until `drain_scattered` takes it off the list.
        unsafe { core::ptr::addr_of_mut!((*node).pages).write(pages) };
        loop {
            // SAFETY: as above.
            unsafe { core::ptr::addr_of_mut!((*node).next).write(head) };
            match s.head.compare_exchange_weak(head, v, Release, Relaxed) {
                Ok(_) => return true,
                Err(h) => head = h,
            }
        }
    }

    /// Gives back every block on the deferred list where that is allowed now; returns how many went. Holds no
    /// heap lock, so any thread may call it (`mm.report`, kswapd, tests).
    pub fn drain_scattered(&self) -> u64 {
        let s = &self.scatter;
        if s.head.load(Relaxed) == 0 || self.in_emergency() || !crate::reclaim::context_allows() {
            return 0;
        }
        let Some(h) = s.hook.get() else { return 0 };
        let mut n = s.head.swap(0, Acquire);
        let mut freed = 0;
        while n != 0 {
            // SAFETY: `n` was pushed by `push_deferred` (Release, paired with the Acquire swap) and is still
            // mapped: only this loop, which took the whole list, frees it, after this read (only the copy is used
            // afterwards; the tag keeps a second free from freeing it anywhere else).
            let node = unsafe { (n as *const Node).read() };
            if node.tag != tag_of(n) {
                // Overwritten while on the list: its `next` cannot be trusted.
                self.reports().heap_bad_free(n as u64);
                break;
            }
            s.waiting.fetch_sub(node.pages, Relaxed);
            if h.free(n, node.pages) {
                self.scatter_gone(node.pages);
                freed += 1;
            } else {
                self.reports().heap_bad_free(n as u64);
            }
            if node.next == n {
                break;
            }
            n = node.next;
        }
        freed
    }

    pub(crate) fn scatter_stats(&self) -> ScatterStats {
        let s = &self.scatter;
        ScatterStats {
            allocs: s.allocs.load(Relaxed),
            frees: s.frees.load(Relaxed),
            deferred: s.deferred.load(Relaxed),
            held_bytes: s.held.load(Relaxed) * PAGE,
            waiting_bytes: s.waiting.load(Relaxed) * PAGE,
        }
    }
}

/// One heap's scattered blocks (`HeapStats::scatter`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScatterStats {
    pub allocs: u64,
    pub frees: u64,
    /// Frees that went onto the deferred list.
    pub deferred: u64,
    /// Bytes of scattered blocks not given back yet, those on the deferred list included.
    pub held_bytes: u64,
    /// Of `held_bytes`, the blocks on the deferred list.
    pub waiting_bytes: u64,
}
