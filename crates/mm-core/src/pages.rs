//! Where the heap's pages come from: the seed chunk and the buddy allocator.
//!
//! The kernel hands the heap a seed chunk (8 MiB of direct-mapped RAM) before the frame allocator exists;
//! until then every slab and large block comes from it. Afterwards slabs still use what is left of the seed
//! first (its free blocks, then its unused tail), so it is not wasted, and large blocks come from the buddy
//! allocator as exact page runs. A large block from the seed (only before the frame allocator exists) has a
//! header page in front so its free is checked exactly, and is not reused (counted as `lost`). Seed memory
//! is never given to the buddy allocator (the buddy allocator never owned it).
//!
//! What each seed page is lives out of band, in a table of one byte per page at the head of the seed (the
//! seed's own `meta`): head of a free block of order k, head of a slab of order k, header page of a large
//! run. Nothing is ever written into a block that is free or handed out, so a stray write into freed memory
//! cannot steer the seed, and the free path can ask "is this a slab of order k" without any lock.
//!
//! Who owns a slab (`owns_slab`): in the seed, the table; elsewhere, the buddy allocator's own frame state
//! (`TAG_SLAB`). Only after that check does the heap read a slab header, so a freed pointer into a hole,
//! MMIO, or memory the heap does not own is refused without touching it.
//!
//! Locks: `heap.seed` (an `IrqSpinLock`), taken under the cache lock, before the frame allocator's locks.

use crate::buddy::{boot_buddy, Buddy, ORDERS};
use crate::meta::TAG_SLAB;
use crate::report::{Reports, GLOBAL};
use crate::{Kind, Zone, KINDS, PAGE};
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering::{Acquire, Relaxed, Release}};
use ksync::IrqSpinLock;

/// Seed page table entries (one byte per data page; the order sits in the low 6 bits).
const T_NONE: u8 = 0;
const T_FREE: u8 = 0x40;
const T_SLAB: u8 = 0x80;
const T_RUN: u8 = 0xC0;
const _: () = assert!(ORDERS <= 0x40);

struct Seed {
    /// Start of the never-used tail (0 = no seed).
    cur: usize,
    /// Free seed blocks per order (their heads are marked `T_FREE | order` in the table).
    free_count: [u64; ORDERS],
    free_bytes: u64,
    lost_bytes: u64,
}

#[derive(Clone, Copy)]
enum BuddySrc {
    /// The boot allocator, once the kernel installed it.
    Boot,
    /// A given allocator (host tests).
    #[cfg_attr(not(test), allow(dead_code))]
    Fixed(Option<&'static Buddy>),
}

pub struct PageSrc {
    seed: IrqSpinLock<Seed>,
    /// The seed's bounds and page table, set once by `set_seed` (`seed_end` last, with Release) and read
    /// without the lock: the table covers the data pages [seed_data, seed_end); the table itself sits in
    /// [seed_start, seed_data).
    seed_start: AtomicUsize,
    seed_data: AtomicUsize,
    seed_end: AtomicUsize,
    seed_tab: AtomicPtr<AtomicU8>,
    buddy: BuddySrc,
    rep: &'static Reports,
    /// Bytes held from the buddy allocator per kind (slabs and large runs).
    held: [AtomicU64; KINDS],
}

/// Seed numbers for the statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeedStats {
    pub total: u64,
    /// Bytes of the seed's page table (at its head).
    pub table: u64,
    /// Bytes handed out from the tail so far (in use or free again).
    pub carved: u64,
    /// Bytes of free seed blocks.
    pub free: u64,
    /// Bytes of freed large seed blocks and their header pages (never reused).
    pub lost: u64,
}

#[cfg(target_os = "none")]
fn p2v(p: u64) -> usize {
    hal::phys_to_virt(p) as usize
}
#[cfg(target_os = "none")]
fn v2p(v: usize) -> Option<u64> {
    bootinfo::layout::virt_to_phys(v as u64)
}
// Host tests: the "physical" addresses of a staged buddy are host memory, mapped one to one.
#[cfg(not(target_os = "none"))]
fn p2v(p: u64) -> usize {
    p as usize
}
#[cfg(not(target_os = "none"))]
fn v2p(v: usize) -> Option<u64> {
    Some(v as u64)
}

const RUN_MAGIC: usize = 0x5345_4544_5255_4E21;
const P: usize = PAGE as usize;

/// The seed's data pages and their table, as seen under the seed lock (or after `seed_end` was read).
#[derive(Clone, Copy)]
struct Tab {
    data: usize,
    end: usize,
    tab: &'static [AtomicU8],
}

impl Tab {
    fn holds(&self, v: usize, len: usize) -> bool {
        v >= self.data && v.checked_add(len).is_some_and(|e| e <= self.end)
    }
    /// The entry of the data page at `v` (page-aligned, inside the data pages).
    fn get(&self, v: usize) -> u8 {
        self.tab[(v - self.data) / P].load(Acquire)
    }
    fn set(&self, v: usize, t: u8) {
        self.tab[(v - self.data) / P].store(t, Release);
    }
}

impl Seed {
    fn push(&mut self, t: &Tab, v: usize, order: u32) {
        t.set(v, T_FREE | order as u8);
        self.free_count[order as usize] += 1;
        self.free_bytes += PAGE << order;
    }

    /// A free block of `order` (its entry becomes `T_NONE`). A scan of the table, only when one exists.
    fn pop(&mut self, t: &Tab, order: u32) -> Option<usize> {
        if self.free_count[order as usize] == 0 {
            return None;
        }
        let want = T_FREE | order as u8;
        let Some(i) = t.tab.iter().position(|e| e.load(Relaxed) == want) else {
            // Only after a stray write into the table: forget the count rather than trust it.
            self.free_bytes -= self.free_count[order as usize] * (PAGE << order);
            self.free_count[order as usize] = 0;
            return None;
        };
        let v = t.data + i * P;
        t.set(v, T_NONE);
        self.free_count[order as usize] -= 1;
        self.free_bytes -= PAGE << order;
        Some(v)
    }

    /// Puts [s, e) (page-aligned) on the free lists in maximal aligned blocks.
    fn push_range(&mut self, t: &Tab, mut s: usize, e: usize) {
        while s < e {
            let mut k = ((s / P).trailing_zeros()).min(ORDERS as u32 - 1);
            while s + (P << k) > e {
                k -= 1;
            }
            self.push(t, s, k);
            s += P << k;
        }
    }

    /// A block of `PAGE << order` bytes aligned to its size: a free one (splitting a bigger block), else the
    /// tail.
    fn block(&mut self, t: &Tab, order: u32) -> Option<usize> {
        for o in order as usize..ORDERS {
            if let Some(v) = self.pop(t, o as u32) {
                for k in (order..o as u32).rev() {
                    self.push(t, v + (P << k), k);
                }
                return Some(v);
            }
        }
        self.carve(t, P << order, P << order)
    }

    /// A large run of `pages` aligned to `align` from the tail, with a header page in front (marked `T_RUN`,
    /// holding the magic and the page count) so its free can be checked exactly; the alignment gap goes to
    /// the free blocks.
    fn carve_run(&mut self, t: &Tab, pages: usize, align: usize) -> Option<usize> {
        let len = pages.checked_mul(P)?;
        let run = self.cur.checked_add(P)?.checked_next_multiple_of(align)?;
        if self.cur == 0 || run.checked_add(len).is_none_or(|e| e > t.end) {
            return None;
        }
        let gap = self.cur;
        self.cur = run + len;
        self.push_range(t, gap, run - P);
        // SAFETY: the header page [run - PAGE, run) was just carved from the seed tail: heap-only memory that
        // nothing else references; written through a raw pointer, no reference is formed.
        unsafe { core::ptr::write((run - P) as *mut [usize; 2], [RUN_MAGIC, pages]) };
        t.set(run - P, T_RUN);
        Some(run)
    }

    /// Ends a run from `carve_run`: true if `v` is one that is live with exactly `pages` pages. The run and
    /// its header page are not reused (only runs from before the frame allocator existed are seed runs).
    fn release_run(&mut self, t: &Tab, v: usize, pages: u64) -> bool {
        let Some(h) = v.checked_sub(P) else { return false };
        let Some(len) = usize::try_from(pages).ok().and_then(|p| p.checked_mul(P)) else { return false };
        if !v.is_multiple_of(P) || !t.holds(h, P) || !t.holds(v, len) || t.get(h) != T_RUN {
            return false;
        }
        // SAFETY: the table says [h, h + PAGE) is the header page `carve_run` wrote (heap-only seed memory no
        // user is given); read through a raw pointer, no reference is formed.
        if unsafe { core::ptr::read(h as *const [usize; 2]) } != [RUN_MAGIC, pages as usize] {
            return false;
        }
        t.set(h, T_NONE);
        self.lost_bytes += (pages + 1) * PAGE;
        true
    }

    /// `len` bytes aligned to `align` from the tail; the alignment gap goes to the free blocks.
    fn carve(&mut self, t: &Tab, len: usize, align: usize) -> Option<usize> {
        if self.cur == 0 {
            return None;
        }
        let at = self.cur.checked_next_multiple_of(align)?;
        if at.checked_add(len).is_none_or(|e| e > t.end) {
            return None;
        }
        let gap = self.cur;
        self.cur = at + len;
        self.push_range(t, gap, at);
        Some(at)
    }
}

impl PageSrc {
    pub const fn new() -> PageSrc {
        PageSrc::with(BuddySrc::Boot)
    }

    const fn with(buddy: BuddySrc) -> PageSrc {
        let seed = Seed { cur: 0, free_count: [0; ORDERS], free_bytes: 0, lost_bytes: 0 };
        PageSrc {
            seed: IrqSpinLock::named("heap.seed", seed),
            seed_start: AtomicUsize::new(0),
            seed_data: AtomicUsize::new(0),
            seed_end: AtomicUsize::new(0),
            seed_tab: AtomicPtr::new(core::ptr::null_mut()),
            buddy,
            rep: &GLOBAL,
            held: [const { AtomicU64::new(0) }; KINDS],
        }
    }

    /// A source over a given buddy allocator and/or a seed of real memory (host tests).
    #[cfg(test)]
    pub fn for_test(buddy: Option<&'static Buddy>, seed: Option<(usize, usize)>) -> PageSrc {
        let me = PageSrc::with(BuddySrc::Fixed(buddy));
        if let Some((s, len)) = seed {
            me.set_seed(s, len);
        }
        me
    }

    /// Where troubles are counted (`KernelHeap::reports`).
    pub fn reports(&self) -> &'static Reports {
        self.rep
    }

    #[cfg(test)]
    pub(crate) fn set_reports(&mut self, rep: &'static Reports) {
        self.rep = rep;
    }

    fn buddy(&self) -> Option<&'static Buddy> {
        match self.buddy {
            BuddySrc::Boot => boot_buddy(),
            BuddySrc::Fixed(b) => b,
        }
    }

    /// The seed's data pages and table, once the table exists (`tab_init`).
    fn tab(&self) -> Option<Tab> {
        let tab = self.seed_tab.load(Acquire);
        if tab.is_null() {
            return None;
        }
        let (data, end) = (self.seed_data.load(Relaxed), self.seed_end.load(Relaxed));
        // SAFETY: `tab_init` initialised `(end - data) / PAGE` entries at `seed_tab` (the seed's head: memory
        // only the heap uses, never handed out, never freed) before it published the pointer with Release.
        let tab = unsafe { core::slice::from_raw_parts(tab, (end - data) / P) };
        Some(Tab { data, end, tab })
    }

    /// The table, written on the first use of the seed (seed lock held, proven by `_s`), so that recording
    /// the seed touches no memory.
    fn tab_init(&self, _s: &mut Seed) -> Option<Tab> {
        if let Some(t) = self.tab() {
            return Some(t);
        }
        let (start, data, end) = (self.seed_start.load(Relaxed), self.seed_data.load(Relaxed), self.seed_end.load(Acquire));
        if end == 0 {
            return None;
        }
        // SAFETY: [start, data) is the head of the seed the caller of `set_seed` gave the heap (direct-mapped
        // RAM nothing else uses, `KernelHeap::seed`'s contract), one byte per data page fits in it.
        unsafe { core::ptr::write_bytes(start as *mut u8, T_NONE, (end - data) / P) };
        self.seed_tab.store(start as *mut AtomicU8, Release);
        self.tab()
    }

    /// Records the seed (one-shot, page-aligned start; checked by `KernelHeap::seed`). False if a seed exists
    /// or it is too small for its table and one page. Its head becomes the page table (written on first use).
    pub fn set_seed(&self, virt: usize, len: usize) -> bool {
        let mut s = self.seed.lock();
        if self.seed_end.load(Relaxed) != 0 {
            return false;
        }
        let Some(start) = virt.checked_next_multiple_of(P) else { return false };
        let end = virt.saturating_add(len) & !(P - 1);
        if end <= start {
            return false;
        }
        let data = start + ((end - start) / P).div_ceil(P) * P;
        if end <= data {
            return false;
        }
        self.seed_start.store(start, Relaxed);
        self.seed_data.store(data, Relaxed);
        self.seed_end.store(end, Release);
        s.cur = data;
        true
    }

    /// The seed as virtual addresses, table included ((0, 0) if none).
    pub fn seed_virt(&self) -> (usize, usize) {
        match self.seed_end.load(Acquire) {
            0 => (0, 0),
            e => (self.seed_start.load(Relaxed), e),
        }
    }

    pub fn in_seed(&self, v: usize) -> bool {
        let (s, e) = self.seed_virt();
        v >= s && v < e
    }

    /// Is `v` the start of a slab block of `order` for `kind` that this source handed out and not taken back?
    /// Reads only the seed table or the buddy allocator's frame state, never the block, so any address is
    /// safe to ask about (lock-free: the free path of every object asks).
    pub fn owns_slab(&self, v: usize, order: u32, kind: Kind) -> bool {
        if self.in_seed(v) {
            let Some(t) = self.tab() else { return false };
            return v >= t.data && v.is_multiple_of(P) && v + (P << order) <= t.end && t.get(v) == T_SLAB | order as u8;
        }
        match (self.buddy(), v2p(v)) {
            (Some(b), Some(p)) => b.is_tagged(p, order, kind, TAG_SLAB),
            _ => false,
        }
    }

    /// A block of `PAGE << order` bytes aligned to its size for a slab of `kind` (virtual address).
    pub fn alloc_block(&self, order: u32, kind: Kind) -> Option<usize> {
        if self.seed_end.load(Relaxed) != 0 {
            let mut s = self.seed.lock();
            if let Some(t) = self.tab_init(&mut s)
                && let Some(v) = s.block(&t, order)
            {
                t.set(v, T_SLAB | order as u8);
                return Some(v);
            }
        }
        let p = self.buddy()?.alloc_tagged(order, Zone::Normal, kind, TAG_SLAB)?;
        self.held[kind as usize].fetch_add(PAGE << order, Relaxed);
        Some(p2v(p))
    }

    /// Gives back a block from `alloc_block`; false (nothing changed) if `v` is not such a block.
    pub fn free_block(&self, v: usize, order: u32, kind: Kind) -> bool {
        if self.in_seed(v) {
            let Some(t) = self.tab() else { return false };
            let mut s = self.seed.lock();
            if !v.is_multiple_of(P) || !t.holds(v, P << order) || t.get(v) != T_SLAB | order as u8 {
                return false;
            }
            s.push(&t, v, order);
            return true;
        }
        match (self.buddy(), v2p(v)) {
            (Some(b), Some(p)) if b.free_tagged(p, order, kind, TAG_SLAB) => {
                self.held[kind as usize].fetch_sub(PAGE << order, Relaxed);
                true
            }
            _ => false,
        }
    }

    /// `pages` contiguous pages aligned to `align_pages` (a power of two) for a large block: an exact run
    /// from the buddy allocator, or from the seed's tail before the frame allocator exists.
    pub fn alloc_run(&self, pages: u64, align_pages: u64, kind: Kind) -> Option<usize> {
        let Some(b) = self.buddy() else {
            let align = usize::try_from(align_pages.checked_mul(PAGE)?).ok()?;
            let mut s = self.seed.lock();
            let t = self.tab_init(&mut s)?;
            return s.carve_run(&t, usize::try_from(pages).ok()?, align);
        };
        let p = b.alloc_exact(pages, align_pages, Zone::Normal, kind)?;
        self.held[kind as usize].fetch_add(pages * PAGE, Relaxed);
        Some(p2v(p))
    }

    /// Gives back a run from `alloc_run`. False (refused, nothing changed) if `v` is not a live run of exactly
    /// these pages for `kind` (double free, wrong size, a slab, a later piece, or another CPU freeing it now).
    pub fn free_run(&self, v: usize, pages: u64, kind: Kind) -> bool {
        if self.in_seed(v) {
            let Some(t) = self.tab() else { return false };
            return self.seed.lock().release_run(&t, v, pages);
        }
        match (self.buddy(), v2p(v)) {
            (Some(b), Some(p)) if b.free_exact(p, pages, kind) => {
                self.held[kind as usize].fetch_sub(pages * PAGE, Relaxed);
                true
            }
            _ => false,
        }
    }

    /// Bytes held from the buddy allocator per kind.
    pub fn held(&self) -> [u64; KINDS] {
        core::array::from_fn(|k| self.held[k].load(Relaxed))
    }

    pub fn seed_stats(&self) -> SeedStats {
        let s = self.seed.lock();
        let (start, data, end) = (self.seed_start.load(Relaxed), self.seed_data.load(Relaxed), self.seed_end.load(Relaxed));
        if end == 0 {
            return SeedStats::default();
        }
        SeedStats {
            total: (end - start) as u64,
            table: (data - start) as u64,
            carved: (s.cur - data) as u64,
            free: s.free_bytes,
            lost: s.lost_bytes,
        }
    }
}

impl Default for PageSrc {
    fn default() -> Self {
        Self::new()
    }
}
