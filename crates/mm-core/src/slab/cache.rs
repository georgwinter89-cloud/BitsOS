//! One slab cache: objects of one geometry, slabs on three intrusive lists (partial, full, empty).
//!
//! `take` hands objects out of slabs (marking them `taken`), creating a slab from the page source when no
//! partial or empty one is left; `put` takes them back, and a slab whose last object comes back goes to the
//! empty list; beyond `KEEP_EMPTY` empty slabs it goes straight back to the page source (the buddy
//! allocator, or the seed's free list). `mark_live` / `clear_live` track which objects a user holds, without
//! the lock; `clear_live` is the gate of every free: a pointer that is not an object of this cache, or not
//! live, is refused before anything changes.
//!
//! Lock order: per-CPU magazine -> `heap.cache` (this lock) -> `heap.seed` -> frames.

use super::geo::Geometry;
use super::raw::{Slab, SLAB_MAGIC};
use crate::pages::PageSrc;
use crate::Kind;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use ksync::IrqSpinLock;

pub const PARTIAL: u32 = 0;
pub const FULL: u32 = 1;
pub const EMPTY: u32 = 2;
/// Empty slabs a cache keeps for the next allocation (hysteresis against create/return churn); a tuning
/// value, `shrink` gives these back too.
pub const KEEP_EMPTY: u64 = 1;

pub(crate) struct Lists {
    head: [usize; 3],
    len: [u64; 3],
}

/// Why a pointer was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not at an object boundary of a slab of this geometry, or not in a slab block of this order and kind.
    NotAnObject,
    /// A slab block, but its header names another cache.
    ForeignSlab,
    /// The object is not held by a user (double free, or never handed out).
    NotLive,
}

/// What a consistency walk found (no allocation while the walk holds locks, so plain numbers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckError {
    BadMagic { base: usize },
    WrongList { base: usize, list: u32, expected: u32 },
    TakenCount { base: usize, header: u32, bits: usize },
    LiveNotTaken { base: usize },
    Counter { what: &'static str, counted: u64, walked: u64 },
    MagazineObject { addr: usize },
}

/// What `Cache::check` walked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckSums {
    pub slabs: u64,
    /// Slabs carved from the seed (not held from the buddy allocator).
    pub seed_slabs: u64,
}

/// Numbers of one cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub name: &'static str,
    pub kind: usize,
    pub object_size: usize,
    pub slab_bytes: usize,
    pub objects_per_slab: usize,
    pub slabs: u64,
    pub empty_slabs: u64,
    pub full_slabs: u64,
    /// Objects out of slabs (held by users or parked in magazines).
    pub taken: u64,
    /// Objects held by users.
    pub live: u64,
}

pub struct Cache {
    pub(crate) name: &'static str,
    pub(crate) kind: Kind,
    pub(crate) geo: Geometry,
    lists: IrqSpinLock<Lists>,
    slabs: AtomicU64,
    taken: AtomicU64,
    live: AtomicU64,
}

impl Cache {
    pub const fn new(name: &'static str, geo: Geometry, kind: Kind) -> Cache {
        Cache {
            name,
            kind,
            geo,
            lists: IrqSpinLock::named("heap.cache", Lists { head: [0; 3], len: [0; 3] }),
            slabs: AtomicU64::new(0),
            taken: AtomicU64::new(0),
            live: AtomicU64::new(0),
        }
    }

    /// Ties slabs to this cache. A cache never moves once it has slabs: it lives in a static or a leaked box.
    pub(crate) fn magic(&self) -> u64 {
        SLAB_MAGIC ^ self as *const Cache as u64
    }

    fn slab(&self, base: usize) -> Slab {
        // SAFETY: only called with bases from this cache's list heads (which only ever hold checked slabs),
        // from objects this cache handed out (`take`, magazines), or checked by `owned` / `locate`: slabs of this
        // geometry the cache owns until `trim_empty` gives them back.
        unsafe { Slab::at(base, &self.geo) }
    }

    /// `base` as a slab of this cache, or None: slab-aligned, a slab block of this order and kind the page
    /// source handed out and still counts as given (`owns_slab`, which reads no slab memory), with this
    /// cache's magic in the header (at the same place in every slab of the order, so never object data).
    fn owned(&self, src: &PageSrc, base: usize) -> Option<Slab> {
        if base == 0 || !base.is_multiple_of(self.geo.slab_bytes()) || !src.owns_slab(base, self.geo.order, self.kind) {
            return None;
        }
        self.named_here(base)
    }

    /// The slab at `base` if its header names this cache. Only for a `base` that `owns_slab` confirmed as a
    /// slab block of this order and kind.
    fn named_here(&self, base: usize) -> Option<Slab> {
        // SAFETY: the caller checked `owns_slab`: a slab block of this order, its last word is a magic.
        let magic = unsafe { Slab::magic_at(base, self.geo.order) };
        // The magic names the cache, and with it the geometry the header and bitmaps are read with.
        (magic.load(Relaxed) == self.magic()).then(|| self.slab(base))
    }

    /// A link read from a slab header: Ok(None) for "no neighbour", Ok(slab) for a slab of this cache on
    /// `list`, Err for a damaged link (counted; the caller treats it as the end and never writes through it).
    fn neighbour(&self, src: &PageSrc, link: usize, list: usize) -> Result<Option<Slab>, ()> {
        if link == 0 {
            return Ok(None);
        }
        match self.owned(src, link) {
            Some(n) if n.hdr.list.load(Relaxed) as usize == list => Ok(Some(n)),
            _ => {
                src.reports().heap_corrupt(link);
                Err(())
            }
        }
    }

    /// The list a slab says it is on, if that is a list.
    fn list_of(&self, src: &PageSrc, s: Slab) -> Option<usize> {
        let list = s.hdr.list.load(Relaxed) as usize;
        if list > EMPTY as usize {
            src.reports().heap_corrupt(s.base);
            return None;
        }
        Some(list)
    }

    // ---- lists (lock held) ----

    fn link(&self, l: &mut Lists, s: Slab, list: u32) {
        let old = l.head[list as usize];
        s.hdr.prev.store(0, Relaxed);
        s.hdr.next.store(old, Relaxed);
        if old != 0 {
            self.slab(old).hdr.prev.store(s.base, Relaxed);
        }
        s.hdr.list.store(list, Relaxed);
        l.head[list as usize] = s.base;
        l.len[list as usize] += 1;
    }

    /// Takes `s` off the list it is on. Only checked slabs are written; a damaged neighbour link ends the
    /// list there (the slabs behind it leak, counted).
    fn unlink(&self, src: &PageSrc, l: &mut Lists, s: Slab) {
        let Some(list) = self.list_of(src, s) else { return };
        let prev = self.neighbour(src, s.hdr.prev.load(Relaxed), list);
        let next = self.neighbour(src, s.hdr.next.load(Relaxed), list).unwrap_or(None);
        let next_base = next.map_or(0, |n| n.base);
        match prev {
            Ok(Some(p)) => p.hdr.next.store(next_base, Relaxed),
            Ok(None) | Err(()) if l.head[list] == s.base => l.head[list] = next_base,
            // Not the head and no usable predecessor: it is not reachable on this list anyway.
            _ => {}
        }
        if let Some(n) = next {
            n.hdr.prev.store(prev.unwrap_or(None).map_or(0, |p| p.base), Relaxed);
        }
        l.len[list] = l.len[list].saturating_sub(1);
    }

    fn relist(&self, src: &PageSrc, l: &mut Lists, s: Slab, list: u32) {
        if s.hdr.list.load(Relaxed) != list {
            self.unlink(src, l, s);
            self.link(l, s, list);
        }
    }

    /// A slab with a free object: partial, else empty, else a new one from `src`.
    fn source_slab(&self, l: &mut Lists, src: &PageSrc) -> Option<Slab> {
        if l.head[PARTIAL as usize] != 0 {
            return Some(self.slab(l.head[PARTIAL as usize]));
        }
        if l.head[EMPTY as usize] != 0 {
            let s = self.slab(l.head[EMPTY as usize]);
            self.relist(src, l, s, PARTIAL);
            return Some(s);
        }
        let base = src.alloc_block(self.geo.order, self.kind)?;
        // SAFETY: a fresh block of `slab_bytes` aligned to its size, from the page source, owned by nobody else.
        let s = unsafe { Slab::init(base, &self.geo, self.magic(), PARTIAL) };
        self.slabs.fetch_add(1, Relaxed);
        self.link(l, s, PARTIAL);
        Some(s)
    }

    // ---- objects ----

    /// Hands up to `n` objects out of slabs (taken, not yet live) to `out`; fewer only when the page source
    /// is empty. Returns how many.
    pub fn take(&self, src: &PageSrc, n: usize, out: &mut dyn FnMut(usize)) -> usize {
        let mut got = 0;
        let mut l = self.lists.lock();
        while got < n {
            let Some(s) = self.source_slab(&mut l, src) else { break };
            let mut full = false;
            while got < n && !full {
                match s.first_free() {
                    Some(i) if i < self.geo.count => {
                        Slab::flip(s.taken, i, true);
                        s.hdr.taken.fetch_add(1, Relaxed);
                        out(s.base + i * self.geo.size);
                        got += 1;
                        full = s.hdr.taken.load(Relaxed) as usize >= self.geo.count;
                    }
                    // Only after a stray write into the bitmap: never pick this slab again.
                    Some(_) => {
                        src.reports().heap_corrupt(s.base);
                        full = true;
                    }
                    None => full = true,
                }
            }
            if full {
                self.relist(src, &mut l, s, FULL);
            }
        }
        drop(l);
        self.taken.fetch_add(got as u64, Relaxed);
        got
    }

    /// Takes objects back into their slabs (they must be taken and not live: magazine contents and objects
    /// `clear_live` accepted). Empty slabs beyond `keep` go back to `src`.
    pub fn put(&self, src: &PageSrc, objs: &[usize], keep: u64) {
        let mut l = self.lists.lock();
        let mut back = 0;
        for &v in objs {
            let base = v & !(self.geo.slab_bytes() - 1);
            let s = self.slab(base);
            let i = (v - base) / self.geo.size;
            if !Slab::flip(s.taken, i, false) {
                continue; // cannot happen: the callers only pass objects taken from this cache
            }
            back += 1;
            let left = s.hdr.taken.fetch_sub(1, Relaxed) - 1;
            if s.hdr.list.load(Relaxed) == FULL {
                self.relist(src, &mut l, s, PARTIAL);
            }
            if left == 0 {
                self.relist(src, &mut l, s, EMPTY);
                self.trim_empty(&mut l, src, keep);
            }
        }
        drop(l);
        self.taken.fetch_sub(back, Relaxed);
    }

    /// Gives empty slabs beyond `keep` back to the page source (lock held). Only a slab whose header and
    /// bitmap both say "nothing taken" goes back; anything else on the empty list is a damaged list (counted,
    /// the list is cut there and the slab stays where it is).
    fn trim_empty(&self, l: &mut Lists, src: &PageSrc, keep: u64) {
        while l.len[EMPTY as usize] > keep && l.head[EMPTY as usize] != 0 {
            let s = self.slab(l.head[EMPTY as usize]);
            if s.hdr.taken.load(Relaxed) != 0 || Slab::count_bits(s.taken, self.geo.count) != 0 {
                src.reports().heap_corrupt(s.base);
                l.head[EMPTY as usize] = 0;
                l.len[EMPTY as usize] = 0;
                return;
            }
            self.unlink(src, l, s);
            s.magic.store(0, Relaxed);
            self.slabs.fetch_sub(1, Relaxed);
            if !src.free_block(s.base, self.geo.order, self.kind) {
                src.reports().heap_corrupt(s.base);
            }
        }
    }

    /// Gives every empty slab beyond `keep` back.
    pub fn release_empty(&self, src: &PageSrc, keep: u64) {
        let mut l = self.lists.lock();
        self.trim_empty(&mut l, src, keep);
    }

    /// The slab and index of an object of this cache, or why `v` is not one. Any value of `v` is safe: the
    /// slab is confirmed with the page source before its header is read.
    pub fn locate(&self, src: &PageSrc, v: usize) -> Result<(Slab, usize), Refused> {
        if v == 0 {
            return Err(Refused::NotAnObject);
        }
        let base = v & !(self.geo.slab_bytes() - 1);
        let off = v - base;
        if off >= self.geo.count * self.geo.size || !off.is_multiple_of(self.geo.size) {
            return Err(Refused::NotAnObject);
        }
        if !src.owns_slab(base, self.geo.order, self.kind) {
            return Err(Refused::NotAnObject);
        }
        let s = self.named_here(base).ok_or(Refused::ForeignSlab)?;
        Ok((s, off / self.geo.size))
    }

    /// Marks an object just taken (from a slab or a magazine) as held by a user. False if it already was:
    /// handed out twice, a bug elsewhere (the caller keeps it out of use).
    pub fn mark_live(&self, v: usize) -> bool {
        let base = v & !(self.geo.slab_bytes() - 1);
        let s = self.slab(base);
        if Slab::flip(s.live, (v - base) / self.geo.size, true) {
            return false;
        }
        self.live.fetch_add(1, Relaxed);
        true
    }

    /// The gate of every free: `v` must be a live object of this cache; it is no longer live afterwards.
    pub fn clear_live(&self, src: &PageSrc, v: usize) -> Result<(), Refused> {
        let (s, i) = self.locate(src, v)?;
        if !Slab::flip(s.live, i, false) {
            return Err(Refused::NotLive);
        }
        self.live.fetch_sub(1, Relaxed);
        Ok(())
    }

    pub fn stats(&self) -> CacheStats {
        let l = self.lists.lock();
        let (empty, full) = (l.len[EMPTY as usize], l.len[FULL as usize]);
        drop(l);
        CacheStats {
            name: self.name,
            kind: self.kind as usize,
            object_size: self.geo.size,
            slab_bytes: self.geo.slab_bytes(),
            objects_per_slab: self.geo.count,
            slabs: self.slabs.load(Relaxed),
            empty_slabs: empty,
            full_slabs: full,
            taken: self.taken.load(Relaxed),
            live: self.live.load(Relaxed),
        }
    }

    /// Walks every slab under the lock: headers, list membership, bitmaps against the counters. Exact when
    /// no other CPU allocates from this cache meanwhile (the live counter changes without the lock).
    pub fn check(&self, src: &PageSrc) -> Result<CheckSums, CheckError> {
        let l = self.lists.lock();
        let (mut slabs, mut taken, mut live, mut seed) = (0u64, 0u64, 0u64, 0u64);
        let bound = self.slabs.load(Relaxed);
        for list in [PARTIAL, FULL, EMPTY] {
            let mut base = l.head[list as usize];
            let mut n = 0;
            while base != 0 {
                let Some(s) = self.owned(src, base) else { return Err(CheckError::BadMagic { base }) };
                if n >= bound {
                    return Err(CheckError::Counter { what: "list walk (a loop)", counted: bound, walked: n + 1 });
                }
                let t = Slab::count_bits(s.taken, self.geo.count);
                if s.hdr.taken.load(Relaxed) as usize != t {
                    return Err(CheckError::TakenCount { base, header: s.hdr.taken.load(Relaxed), bits: t });
                }
                let lv = Slab::count_bits(s.live, self.geo.count);
                let expected = match t {
                    0 => EMPTY,
                    t if t == self.geo.count => FULL,
                    _ => PARTIAL,
                };
                if s.hdr.list.load(Relaxed) != list || expected != list {
                    return Err(CheckError::WrongList { base, list, expected });
                }
                if s.live.iter().zip(s.taken).any(|(lv, tk)| lv.load(Relaxed) & !tk.load(Relaxed) != 0) {
                    return Err(CheckError::LiveNotTaken { base });
                }
                taken += t as u64;
                live += lv as u64;
                seed += src.in_seed(base) as u64;
                n += 1;
                base = s.hdr.next.load(Relaxed);
            }
            if n != l.len[list as usize] {
                return Err(CheckError::Counter { what: "list length", counted: l.len[list as usize], walked: n });
            }
            slabs += n;
        }
        drop(l);
        for (what, counted, walked) in [("slabs", self.slabs.load(Relaxed), slabs), ("taken", self.taken.load(Relaxed), taken), ("live", self.live.load(Relaxed), live)] {
            if counted != walked {
                return Err(CheckError::Counter { what, counted, walked });
            }
        }
        Ok(CheckSums { slabs, seed_slabs: seed })
    }

    /// Is `v` an object of this cache that is taken but not live (what a magazine may hold)?
    pub fn parked_ok(&self, src: &PageSrc, v: usize) -> bool {
        self.locate(src, v).is_ok_and(|(s, i)| Slab::test(s.taken, i) && !Slab::test(s.live, i))
    }
}
