//! Heap statistics, the consistency walk and the bad-free probe (`hw_info memory`, ktests, a later MCP view).
//!
//! Neither allocates while it holds a heap lock (that would wait for itself): numbers go into fixed arrays,
//! and `check` reports plain values (`CheckError`) that the caller formats afterwards.

use crate::heap::KernelHeap;
use crate::pages::SeedStats;
use crate::slab::geo::NCLASSES;
use crate::slab::{CacheStats, CheckError};
use crate::{Kind, KINDS, PAGE};
use core::alloc::Layout;
use core::sync::atomic::Ordering::Relaxed;

#[derive(Clone, Copy, Debug, Default)]
pub struct HeapStats {
    pub classes: [CacheStats; NCLASSES],
    /// Objects parked in per-CPU magazines, per class.
    pub magazine_objects: [u64; NCLASSES],
    /// CPUs with magazines.
    pub magazine_cpus: usize,
    /// Bytes of all class slabs (seed and buddy).
    pub slab_bytes: u64,
    /// Bytes of class objects users hold (at class size).
    pub live_bytes: u64,
    /// Bytes users asked for and hold (layout sizes, small and large).
    pub requested_bytes: u64,
    pub large_bytes: u64,
    pub large_blocks: u64,
    /// Bytes the heap and the named caches hold from the buddy allocator, per kind (`FrameStats`'s
    /// `used_by_kind[Heap]` is exactly `held_by_kind[Heap]` when nothing else allocates `Kind::Heap` frames).
    pub held_by_kind: [u64; KINDS],
    /// Bytes users hold per kind: heap classes and large blocks under `Heap`, named caches under theirs.
    pub live_by_kind: [u64; KINDS],
    /// Bytes of named-cache slabs per kind.
    pub named_slab_by_kind: [u64; KINDS],
    pub seed: SeedStats,
}

impl KernelHeap {
    pub fn stats(&self) -> HeapStats {
        let mut s = HeapStats { classes: core::array::from_fn(|c| self.classes[c].stats()), ..Default::default() };
        self.mags.for_each(|m| {
            s.magazine_cpus += 1;
            let g = m.lock();
            for (c, mag) in g.iter().enumerate() {
                s.magazine_objects[c] += mag.len() as u64;
            }
        });
        for c in &s.classes {
            s.slab_bytes += c.slabs * c.slab_bytes as u64;
            s.live_bytes += c.live * c.object_size as u64;
        }
        s.requested_bytes = self.requested.load(Relaxed);
        s.large_bytes = self.large_bytes.load(Relaxed);
        s.large_blocks = self.large_blocks.load(Relaxed);
        s.held_by_kind = self.src().held();
        s.live_by_kind[Kind::Heap as usize] = s.live_bytes + s.large_bytes;
        if core::ptr::eq(self, &crate::heap::HEAP) {
            crate::slab::for_each_cache(|c| {
                let cs = c.stats();
                s.live_by_kind[cs.kind] += cs.live * cs.object_size as u64;
                s.named_slab_by_kind[cs.kind] += cs.slabs * cs.slab_bytes as u64;
            });
        }
        s.seed = self.src().seed_stats();
        s
    }

    /// Walks every magazine and slab: each parked object is taken and not live, each slab's bitmaps match its
    /// header, list and the cache counters, taken = live + parked per class, and the bytes held from the
    /// buddy allocator = class slabs outside the seed + large blocks + named-cache slabs outside the seed.
    /// Exact while no other CPU allocates (the ktests run on one CPU).
    pub fn check(&self) -> Result<(), CheckError> {
        let mut parked = [0u64; NCLASSES];
        let mut bad = None;
        self.mags.for_each(|m| {
            let g = m.lock();
            for (c, mag) in g.iter().enumerate() {
                for &o in mag.objects() {
                    if !self.classes[c].parked_ok(self.src(), o) {
                        bad = Some(o);
                    }
                }
                parked[c] += mag.len() as u64;
            }
        });
        if let Some(addr) = bad {
            return Err(CheckError::MagazineObject { addr });
        }
        let mut buddy_slab_bytes = [0u64; KINDS];
        for (c, cache) in self.classes.iter().enumerate() {
            let sums = cache.check(self.src())?;
            let st = cache.stats();
            if st.taken != st.live + parked[c] {
                return Err(CheckError::Counter { what: "taken = live + parked", counted: st.taken, walked: st.live + parked[c] });
            }
            buddy_slab_bytes[Kind::Heap as usize] += (sums.slabs - sums.seed_slabs) * st.slab_bytes as u64;
        }
        let mut named = Ok(());
        if core::ptr::eq(self, &crate::heap::HEAP) {
            crate::slab::for_each_cache(|c| match c.check_sums() {
                Ok((sums, st)) => buddy_slab_bytes[st.kind] += (sums.slabs - sums.seed_slabs) * st.slab_bytes as u64,
                Err(e) => named = Err(e),
            });
        }
        named?;
        let held = self.src().held();
        let mut want = buddy_slab_bytes;
        want[Kind::Heap as usize] += self.large_bytes.load(Relaxed) - self.seed_large_bytes();
        for k in 0..KINDS {
            if held[k] != want[k] {
                return Err(CheckError::Counter { what: "bytes held from the buddy allocator", counted: held[k], walked: want[k] });
            }
        }
        Ok(())
    }

    /// Large bytes that came from the seed (before the frame allocator existed) and are still held: the
    /// seed tail carved minus slabs and free lists cannot tell them apart, so they are tracked by address.
    fn seed_large_bytes(&self) -> u64 {
        self.seed_large.load(Relaxed) * PAGE
    }
}

impl KernelHeap {
    /// Hands the heap the frees a buggy caller could make, on blocks it allocates itself: a double free of
    /// an object parked in a magazine, an interior pointer, a free with another class's layout, a double
    /// free of a large block, a large block freed with the wrong size, a pointer outside the direct map, a
    /// pointer into a hole of the physical map (small and large layout; nothing may be read there), a slab
    /// freed as a page run, and a later piece of a page run freed on its own. Each must be refused and
    /// counted, and nothing may change. Returns the number refused (`PROBE_REFUSALS`).
    pub fn probe_bad_frees(&self) -> Result<u64, &'static str> {
        let l64 = Layout::from_size_align(64, 8).map_err(|_| "layout")?;
        let l512 = Layout::from_size_align(512, 8).map_err(|_| "layout")?;
        let page = Layout::from_size_align(PAGE as usize, 8).map_err(|_| "layout")?;
        let big = Layout::from_size_align(3 * PAGE as usize, 8).map_err(|_| "layout")?;
        let bigger = Layout::from_size_align(5 * PAGE as usize, 8).map_err(|_| "layout")?;
        let (a, b, l, r) = (self.alloc(l64), self.alloc(l64), self.alloc(big), self.alloc(bigger));
        if a.is_null() || b.is_null() || l.is_null() || r.is_null() {
            return Err("no memory for the probe");
        }
        let before = self.reports().heap_bad_frees.load(Relaxed);
        let (large, requested) = (self.large_bytes.load(Relaxed), self.requested.load(Relaxed));
        // An address in a hole: kernel, the local APIC page through the direct map (MMIO, never RAM, not
        // mapped as RAM); host, an address no allocation of the test gets (low canonical, unmapped).
        #[cfg(target_os = "none")]
        let hole = (bootinfo::layout::DIRECT_MAP_BASE + 0xFEE0_0000) as usize;
        #[cfg(not(target_os = "none"))]
        let hole = 0x7FF0_0000_0000usize;
        // SAFETY: a, b, l and r are blocks this function just allocated with these layouts; every call below
        // either frees one of them once with its own layout or hands the heap a pointer it must refuse
        // without reading or writing it (inside `b`, `b` under another class, `a`/`l` again after their free,
        // `l` with the wrong size, the slab under `b` as a page, the last page of `r`, unmapped addresses):
        // nothing else holds them.
        unsafe {
            self.dealloc(a, l64);
            self.dealloc(a, l64);
            self.dealloc(b.add(8), l64);
            self.dealloc(b, l512);
            self.dealloc(l, bigger);
            self.dealloc(l, big);
            self.dealloc(l, big);
            self.dealloc(0x1000 as *mut u8, l64);
            self.dealloc((hole + 64) as *mut u8, l64);
            self.dealloc(hole as *mut u8, page);
            self.dealloc(((b as usize) & !(PAGE as usize - 1)) as *mut u8, page);
            self.dealloc(r.add(4 * PAGE as usize), page);
            self.dealloc(r, bigger);
            self.dealloc(b, l64);
        }
        let refused = self.reports().heap_bad_frees.load(Relaxed) - before;
        if refused != PROBE_REFUSALS {
            return Err("not every bad free was refused");
        }
        if self.large_bytes.load(Relaxed) + 8 * PAGE != large || self.requested.load(Relaxed) + 2 * 64 + 8 * PAGE != requested {
            return Err("a refused free changed the heap's numbers");
        }
        Ok(refused)
    }
}

/// Bad frees `probe_bad_frees` makes.
pub const PROBE_REFUSALS: u64 = 10;
