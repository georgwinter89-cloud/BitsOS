//! Slab caches (MM-3; Linux `kmem_cache`, Bonwick's slab allocator with magazines).
//!
//! A cache serves objects of one size from slabs (buddy blocks of 4 KiB..128 KiB, header and bitmaps at the
//! end, `geo`/`raw`). The heap's size classes are caches with per-CPU magazines in front (`crate::heap`,
//! `crate::mags`); `SlabCache` is a named cache other crates create as a `static` for their own objects,
//! charged to their own `Kind`, used through the safe `SlabBox`. Every free is checked against the slab's
//! `live` bitmap, so a double free or a foreign pointer is refused and counted (`mm.heap.bad_free`), never
//! turned into corruption.

mod boxed;
pub(crate) mod cache;
pub mod geo;
pub(crate) mod raw;

pub use boxed::SlabBox;
pub use cache::{CacheStats, CheckError, Refused};

use crate::Kind;
use cache::Cache;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// A named cache of kernel objects of one size and alignment (up to 4 KiB), charged to `kind`. Create it as
/// a `static`; it uses the kernel heap's page source (seed, then buddy) and has no magazines.
pub struct SlabCache {
    inner: Cache,
    next: AtomicPtr<SlabCache>,
    registered: AtomicBool,
}

static NAMED: AtomicPtr<SlabCache> = AtomicPtr::new(core::ptr::null_mut());

impl SlabCache {
    /// Panics at compile time (in a `static`) if the size and alignment do not fit a slab.
    pub const fn new(name: &'static str, size: usize, align: usize, kind: Kind) -> SlabCache {
        let geo = match geo::geometry(size, align) {
            Some(g) => g,
            None => panic!("SlabCache: size/alignment do not fit a slab (alignment up to 4 KiB, size up to ~128 KiB)"),
        };
        SlabCache { inner: Cache::new(name, geo, kind), next: AtomicPtr::new(core::ptr::null_mut()), registered: AtomicBool::new(false) }
    }

    pub fn name(&self) -> &'static str {
        self.inner.name
    }

    /// Bytes per object (the size rounded up to the alignment, at least 8).
    pub fn object_size(&self) -> usize {
        self.inner.geo.size
    }

    /// Objects are aligned to this (the lowest set bit of the object size; slabs are page-aligned).
    pub fn object_align(&self) -> usize {
        1 << self.inner.geo.size.trailing_zeros().min(12)
    }

    fn register(&'static self) {
        if self.registered.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self as *const SlabCache as *mut SlabCache;
        let mut head = NAMED.load(Ordering::Acquire);
        loop {
            self.next.store(head, Ordering::Relaxed);
            match NAMED.compare_exchange(head, me, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(h) => head = h,
            }
        }
    }

    /// One object (uninitialised memory of `object_size` bytes), or None when memory is out (after the heap
    /// gave back what its caches hold).
    pub fn alloc(&'static self) -> Option<NonNull<u8>> {
        let heap = &crate::heap::HEAP;
        if heap.in_emergency() {
            return None;
        }
        self.register();
        for attempt in 0..2 {
            let mut got = 0;
            self.inner.take(heap.src(), 1, &mut |o| got = o);
            if got != 0 && self.inner.mark_live(got) {
                return NonNull::new(got as *mut u8);
            }
            if got != 0 {
                heap.reports().heap_corrupt(got);
            }
            if attempt == 0 {
                heap.shrink();
            }
        }
        heap.reports().heap_alloc_fail(self.inner.geo.size);
        None
    }

    /// Gives an object back. Refused (counted, nothing changed) if `p` is not a live object of this cache.
    ///
    /// Any pointer value is safe to hand in: the slab is confirmed with the page source before its header is
    /// read.
    ///
    /// # Safety
    /// If `p` is a live object of this cache, the caller owns it (this cache's `alloc` returned it to the
    /// caller) and does not use it after a successful free.
    pub unsafe fn free(&'static self, p: NonNull<u8>) -> Result<(), Refused> {
        let v = p.as_ptr() as usize;
        if crate::heap::HEAP.in_emergency() {
            return Ok(()); // leaked: no lock is taken any more (`KernelHeap::enter_emergency`)
        }
        if let Err(e) = self.inner.clear_live(crate::heap::HEAP.src(), v) {
            crate::heap::HEAP.reports().heap_bad_free(v as u64);
            return Err(e);
        }
        self.inner.put(crate::heap::HEAP.src(), &[v], cache::KEEP_EMPTY);
        Ok(())
    }

    pub fn stats(&self) -> CacheStats {
        self.inner.stats()
    }

    /// Walks the slabs (see `KernelHeap::check`).
    pub fn check(&self) -> Result<(), CheckError> {
        self.inner.check(crate::heap::HEAP.src()).map(|_| ())
    }

    pub(crate) fn check_sums(&self) -> Result<(cache::CheckSums, CacheStats), CheckError> {
        Ok((self.inner.check(crate::heap::HEAP.src())?, self.inner.stats()))
    }

    /// Gives every empty slab back.
    pub fn shrink(&self) {
        self.inner.release_empty(crate::heap::HEAP.src(), 0);
    }
}

/// Every named cache that has been used, newest first.
pub fn for_each_cache(mut f: impl FnMut(&'static SlabCache)) {
    let mut p = NAMED.load(Ordering::Acquire);
    while !p.is_null() {
        // SAFETY: only `register` stores into NAMED and `next`, and only `&'static SlabCache` pointers, never
        // removed; a static lives forever.
        let c: &'static SlabCache = unsafe { &*p };
        f(c);
        p = c.next.load(Ordering::Acquire);
    }
}
