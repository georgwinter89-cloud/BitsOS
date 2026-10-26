//! The kernel heap behind `GlobalAlloc` (MM-3): slab caches for the size classes 8..2048 bytes with per-CPU
//! magazines in front, exact page runs from the buddy allocator for everything bigger or aligned above 2 KiB.
//!
//! Small: `class_of(size, align)` picks the class; the CPU's magazine serves it, refilled with half a
//! magazine from the class cache when empty, and a full magazine gives its oldest half back on free
//! (`mags`). Empty slabs go back to the page source beyond one per class. Large: `pages = size / 4 KiB`
//! rounded up, aligned to `align` (up to the buddy maximum), from `PageSrc::alloc_run`. When memory is out
//! the heap empties every magazine and returns every empty slab, then tries once more; only then the caller
//! gets null (R2: ENOMEM to the asker, nobody else is touched). Before the frame allocator exists the seed
//! chunk serves slabs and large blocks (`pages`).
//!
//! Every allocation is charged to an owner kind: the heap's frames to `Kind::Heap`, named caches to theirs
//! (`slab::SlabCache`). Lock order (never klog or the scheduler while held, R5): `heap.mags` ->
//! `heap.cache` -> `heap.seed` -> frames (per-CPU list -> zone). The heap never calls klog; troubles are
//! counted in `report` and turned into events by the `mm.report` thread. A panic first calls
//! `enter_emergency`, after which the heap takes no lock at all (core.md 10.3).

use crate::mags::{MagSlots, CAP};
use crate::pages::PageSrc;
use crate::report::Reports;
use crate::slab::cache::{Cache, KEEP_EMPTY};
use crate::slab::geo::{class_of, geometry, CLASSES, NCLASSES};
use crate::{Kind, PAGE};
use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

const CLASS_NAMES: [&str; NCLASSES] =
    ["heap-8", "heap-16", "heap-32", "heap-64", "heap-96", "heap-128", "heap-192", "heap-256", "heap-512", "heap-1k", "heap-2k"];

const fn class_cache(i: usize) -> Cache {
    match geometry(CLASSES[i], 8) {
        Some(g) => Cache::new(CLASS_NAMES[i], g, Kind::Heap),
        None => panic!("size class does not fit a slab"),
    }
}

pub struct KernelHeap {
    src: PageSrc,
    pub(crate) classes: [Cache; NCLASSES],
    pub(crate) mags: MagSlots,
    /// The heap's own named caches are only shrunk with the kernel heap (host-test heaps leave them alone).
    global: bool,
    /// Set by the panic path: from then on the heap takes no lock (`enter_emergency`).
    emergency: AtomicBool,
    /// Bytes users asked for and still hold (layout sizes).
    pub(crate) requested: AtomicU64,
    pub(crate) large_bytes: AtomicU64,
    pub(crate) large_blocks: AtomicU64,
    /// Pages of large blocks carved from the seed (before the frame allocator existed) still held.
    pub(crate) seed_large: AtomicU64,
}

/// Pages and page alignment of a large block.
fn large_shape(size: usize, align: usize) -> (u64, u64) {
    ((size as u64).div_ceil(PAGE), (align as u64 / PAGE).max(1))
}

impl KernelHeap {
    pub const fn new() -> Self {
        KernelHeap {
            src: PageSrc::new(),
            classes: [
                class_cache(0),
                class_cache(1),
                class_cache(2),
                class_cache(3),
                class_cache(4),
                class_cache(5),
                class_cache(6),
                class_cache(7),
                class_cache(8),
                class_cache(9),
                class_cache(10),
            ],
            mags: MagSlots::kernel(),
            global: true,
            emergency: AtomicBool::new(false),
            requested: AtomicU64::new(0),
            large_bytes: AtomicU64::new(0),
            large_blocks: AtomicU64::new(0),
            seed_large: AtomicU64::new(0),
        }
    }

    /// A heap over its own page source, magazines and reports (host tests). Leaked: slabs are tied to the
    /// address of their cache, so a heap must not move once used.
    #[cfg(test)]
    pub fn for_test(src: PageSrc, mags: MagSlots) -> &'static KernelHeap {
        let mut h = KernelHeap::new();
        h.src = src;
        h.src.set_reports(alloc::boxed::Box::leak(alloc::boxed::Box::new(Reports::new(false))));
        h.mags = mags;
        h.global = false;
        alloc::boxed::Box::leak(alloc::boxed::Box::new(h))
    }

    pub(crate) fn src(&self) -> &PageSrc {
        &self.src
    }

    pub fn reports(&self) -> &Reports {
        self.src.reports()
    }

    /// For the panic path (and anything else that may run while this CPU holds a heap lock, CONTRACTS R5):
    /// from now on `alloc` returns null and `dealloc` leaks, without taking any lock, so a panic that
    /// formats or allocates cannot deadlock on a lock the stopped code holds. One-way; the system is going
    /// down. NMI handlers never allocate at all (core.md 10.2) and do not need this.
    pub fn enter_emergency(&self) {
        self.emergency.store(true, Relaxed);
    }

    pub fn in_emergency(&self) -> bool {
        self.emergency.load(Relaxed)
    }

    /// Gives the heap its first memory before the frame allocator exists. `virt` must be direct-mapped RAM
    /// that no frame allocator hands out (the kernel carves it from the memory map; the frame allocator then
    /// reserves `seed_range`). One-shot: a second seed, or one outside the direct map, is refused and counted
    /// (`mm.heap.bad_free`), so it can never point the heap at memory something else uses afterwards.
    pub fn seed(&self, virt: u64, len: u64) {
        let mapped = virt.checked_add(len).is_some_and(|e| {
            len > 0 && bootinfo::layout::virt_to_phys(virt).is_some() && bootinfo::layout::virt_to_phys(e - 1).is_some()
        });
        if !mapped || !self.src.set_seed(virt as usize, len as usize) {
            self.reports().heap_bad_free(virt);
        }
    }

    /// The seed as a physical range ((0, 0) if none): the frame allocator never hands it out.
    pub fn seed_range(&self) -> (u64, u64) {
        let (s, e) = self.src.seed_virt();
        match (bootinfo::layout::virt_to_phys(s as u64), bootinfo::layout::virt_to_phys(e as u64)) {
            (Some(s), Some(e)) if e > s => (s, e),
            _ => (0, 0),
        }
    }

    /// Bytes users asked for and still hold.
    pub fn allocated_bytes(&self) -> u64 {
        self.requested.load(Relaxed)
    }

    pub fn alloc(&self, layout: Layout) -> *mut u8 {
        if self.in_emergency() {
            return core::ptr::null_mut();
        }
        let size = layout.size().max(1);
        let v = match class_of(size, layout.align()) {
            Some(c) => self.small_alloc(c),
            None => self.large_alloc(size, layout.align()),
        };
        if v == 0 {
            self.reports().heap_alloc_fail(size);
        } else {
            self.requested.fetch_add(size as u64, Relaxed);
        }
        v as *mut u8
    }

    /// Frees a block. A pointer that is not a live block of this layout's class or page run is refused and
    /// counted (`mm.heap.bad_free`) without changing anything: before any slab header is read, the page source
    /// confirms the slab (seed table or buddy frame state), so any pointer value is safe to hand in.
    ///
    /// # Safety
    /// If `ptr` is a live block of this heap with `layout`'s class or page run, the caller owns it (it came
    /// from `alloc`/`realloc` with `layout`, the GlobalAlloc rule) and does not use it after the call: the
    /// heap cannot tell the owner of a live block from someone else holding a copy of the pointer.
    pub unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if self.in_emergency() {
            return;
        }
        let (v, size) = (ptr as usize, layout.size().max(1));
        let ok = match class_of(size, layout.align()) {
            Some(c) => self.small_free(c, v),
            None => self.large_free(v, size, layout.align()),
        };
        if ok {
            self.requested.fetch_sub(size as u64, Relaxed);
        } else {
            self.reports().heap_bad_free(v as u64);
        }
    }

    /// In place when the new size needs the same class or the same pages; otherwise allocate, copy, free.
    ///
    /// # Safety
    /// As `dealloc`; `new_size` is not zero and rounded up to `layout.align()` does not overflow `isize`.
    pub unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let (old, new, align) = (layout.size().max(1), new_size.max(1), layout.align());
        let same = match (class_of(old, align), class_of(new, align)) {
            (Some(a), Some(b)) => a == b,
            (None, None) => large_shape(old, align) == large_shape(new, align),
            _ => false,
        };
        if same {
            self.requested.fetch_add(new as u64, Relaxed);
            self.requested.fetch_sub(old as u64, Relaxed);
            return ptr;
        }
        let Ok(nl) = Layout::from_size_align(new_size, align) else { return core::ptr::null_mut() };
        let n = self.alloc(nl);
        if !n.is_null() {
            // SAFETY: `ptr` is the caller's live block with `layout` (this fn's contract) and `n` a block just
            // handed out, both at least min(old, new) bytes and not overlapping; `ptr` is freed once, after.
            unsafe {
                core::ptr::copy_nonoverlapping(ptr, n, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
        }
        n
    }

    fn small_alloc(&self, c: usize) -> usize {
        let cache = &self.classes[c];
        for attempt in 0..2 {
            if let Some(m) = self.mags.this() {
                let mut g = m.lock();
                let mag = &mut g[c];
                if mag.is_empty() {
                    cache.take(&self.src, CAP[c] / 2, &mut |o| {
                        mag.push(o);
                    });
                }
                while let Some(o) = mag.pop() {
                    if cache.mark_live(o) {
                        return o;
                    }
                    self.reports().heap_corrupt(o);
                }
            } else {
                let mut got = 0;
                cache.take(&self.src, 1, &mut |o| got = o);
                if got != 0 {
                    if cache.mark_live(got) {
                        return got;
                    }
                    self.reports().heap_corrupt(got);
                }
            }
            if attempt == 0 {
                self.shrink();
            }
        }
        0
    }

    fn small_free(&self, c: usize, v: usize) -> bool {
        let cache = &self.classes[c];
        if cache.clear_live(&self.src, v).is_err() {
            return false;
        }
        if let Some(m) = self.mags.this() {
            let mut g = m.lock();
            let mag = &mut g[c];
            if mag.len() >= CAP[c] {
                mag.drain_oldest(CAP[c] / 2, |objs| cache.put(&self.src, objs, KEEP_EMPTY));
            }
            if mag.push(v) {
                return true;
            }
        }
        cache.put(&self.src, &[v], KEEP_EMPTY);
        true
    }

    fn large_alloc(&self, size: usize, align: usize) -> usize {
        let (pages, align_pages) = large_shape(size, align);
        for attempt in 0..2 {
            if let Some(v) = self.src.alloc_run(pages, align_pages, Kind::Heap) {
                self.large_bytes.fetch_add(pages * PAGE, Relaxed);
                self.large_blocks.fetch_add(1, Relaxed);
                if self.src.in_seed(v) {
                    self.seed_large.fetch_add(pages, Relaxed);
                }
                return v;
            }
            if attempt == 0 {
                self.shrink();
            }
        }
        0
    }

    fn large_free(&self, v: usize, size: usize, align: usize) -> bool {
        let (pages, align_pages) = large_shape(size, align);
        let seed = self.src.in_seed(v);
        if v == 0 || !(v as u64).is_multiple_of(align_pages * PAGE) || !self.src.free_run(v, pages, Kind::Heap) {
            return false;
        }
        if seed {
            self.seed_large.fetch_sub(pages, Relaxed);
        }
        self.large_bytes.fetch_sub(pages * PAGE, Relaxed);
        self.large_blocks.fetch_sub(1, Relaxed);
        true
    }

    /// Creates the per-CPU magazines of every possible CPU (allocates; BSP, before the APs start).
    pub fn prepare_cpus(&self) {
        self.mags.prepare_cpus();
    }

    /// Empties every CPU's magazines into the caches and gives every empty slab back (the heap's own and the
    /// named caches'). Called when memory is out, and by tests; hold no heap lock.
    pub fn shrink(&self) {
        self.mags.for_each(|m| {
            let mut g = m.lock();
            for (c, mag) in g.iter_mut().enumerate() {
                let n = mag.len();
                mag.drain_oldest(n, |objs| self.classes[c].put(&self.src, objs, KEEP_EMPTY));
            }
        });
        for c in &self.classes {
            c.release_empty(&self.src, 0);
        }
        if self.global {
            crate::slab::for_each_cache(|c| c.shrink());
        }
        self.reports().heap_reclaim();
    }
}

impl Default for KernelHeap {
    fn default() -> Self {
        Self::new()
    }
}

pub static HEAP: KernelHeap = KernelHeap::new();

/// The type the kernel registers as `#[global_allocator]`.
pub struct GlobalHeap;

// SAFETY: alloc returns null or a block of at least `layout.size()` bytes aligned to `layout.align()` that no
// live block overlaps: a slab object is handed out only while its `live` bit was clear and stays out until
// its free clears it (class objects are aligned to their class, `class_of` picks a class aligned enough);
// a large block is an exact page run owned until its dealloc. dealloc gets the layout alloc was called with
// (GlobalAlloc rule), so `class_of` sends it down the same path; anything else is refused and counted.
// realloc keeps the block only when the new layout maps to the same class or page run.
unsafe impl GlobalAlloc for GlobalHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        HEAP.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: GlobalAlloc's contract is KernelHeap::dealloc's.
        unsafe { HEAP.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc's contract is KernelHeap::realloc's.
        unsafe { HEAP.realloc(ptr, layout, new_size) }
    }
}
