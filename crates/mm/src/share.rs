//! Share counts of program frames (MM-5b, copy-on-write for fork). A `Kind::User` frame (anonymous memory or a
//! private copy of a file page) that several address spaces map after a fork carries a count of its *other*
//! holders here: 0 = mapped by one space only (every fresh frame, no initialisation needed), n = n + 1 spaces
//! map it read-only and the first write in any of them copies it (or, once it is the last holder, takes it
//! back writable). Linux keeps this in `struct page` (`_mapcount`, `PageAnonExclusive`); V2's frame metadata
//! is 16 bits, so the counts live in a sparse table of chunks, each covering 4096 frames (16 MiB of RAM, 16 KiB
//! of counts), made the first time a frame in that range is shared. Memory: at most 1/1024 of RAM, only where
//! fork ever shared a frame; no fixed count of frames or sharers (R1): the chunk table is sized from the
//! memory map and a count that would overflow refuses the share, so the caller copies the frame instead.
//!
//! Rules for callers: `share` before a second space maps the frame; `release` once a space no longer maps it
//! (entry gone, shootdown confirmed) and free the frame only when it says so; `exclusive` decides whether a
//! write fault may take the frame back writable. All atomic, no lock: the counts are changed by different
//! address spaces under their own locks.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ksync::Once;

/// Frames per chunk of counts.
pub const CHUNK: u64 = 4096;
const PAGE: u64 = 4096;

pub struct Shares {
    /// `None` inside a made slot: sealed (see `seal_on_miss`), never gets counts.
    slots: Vec<Once<Option<Box<[AtomicU32]>>>>,
    /// A slot whose chunk could not be made is sealed for good instead of being tried again later. For counts
    /// that are only shown (`FILE_MAPS`): a chunk made after a missed `+1` would take that mapping's later `-1`
    /// from another mapping's count. Not for the copy-on-write counts, where a miss only makes fork copy.
    seal_on_miss: bool,
}

impl Shares {
    /// A table for physical addresses below `max_phys` (fallible: None without heap).
    pub fn new(max_phys: u64) -> Option<Shares> {
        let n = (max_phys / PAGE).div_ceil(CHUNK) as usize;
        let mut slots = Vec::new();
        slots.try_reserve_exact(n).ok()?;
        slots.resize_with(n, Once::new);
        Some(Shares { slots, seal_on_miss: false })
    }

    /// `new` for counts that seal a slot whose chunk found no room (see `seal_on_miss`).
    pub fn new_sealing(max_phys: u64) -> Option<Shares> {
        Shares::new(max_phys).map(|s| Shares { seal_on_miss: true, ..s })
    }

    fn chunk(&self, phys: u64) -> Option<&[AtomicU32]> {
        let pfn = phys / PAGE;
        self.slots.get((pfn / CHUNK) as usize)?.get()?.as_deref()
    }

    fn chunk_or_make(&self, phys: u64) -> Option<&[AtomicU32]> {
        let slot = self.slots.get(((phys / PAGE) / CHUNK) as usize)?;
        if slot.get().is_none() {
            let mut v: Vec<AtomicU32> = Vec::new();
            if v.try_reserve_exact(CHUNK as usize).is_err() {
                if self.seal_on_miss {
                    slot.get_or_init(|| None);
                }
                return slot.get()?.as_deref();
            }
            v.resize_with(CHUNK as usize, || AtomicU32::new(0));
            let b = v.into_boxed_slice();
            // Another CPU may have made (or sealed) it meanwhile: then ours is dropped unused.
            slot.get_or_init(move || Some(b));
        }
        slot.get()?.as_deref()
    }

    fn cell(c: &[AtomicU32], phys: u64) -> &AtomicU32 {
        &c[((phys / PAGE) % CHUNK) as usize]
    }

    /// One more space maps `phys`. False: no room for the count (no heap for a new chunk, a frame outside the
    /// table, or a count at its maximum); the caller copies the frame instead of sharing it.
    pub fn share(&self, phys: u64) -> bool {
        let Some(c) = self.chunk_or_make(phys) else { return false };
        Self::cell(c, phys).try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1)).is_ok()
    }

    /// One space no longer maps `phys`. True: it was the last one, free the frame.
    pub fn release(&self, phys: u64) -> bool {
        let Some(c) = self.chunk(phys) else { return true };
        Self::cell(c, phys).try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1)).is_err()
    }

    /// How many other spaces map `phys`.
    pub fn others(&self, phys: u64) -> u32 {
        self.chunk(phys).map_or(0, |c| Self::cell(c, phys).load(Ordering::Acquire))
    }

    /// Mapped by one space only: a write may take it back writable instead of copying.
    pub fn exclusive(&self, phys: u64) -> bool {
        self.others(phys) == 0
    }

    /// Chunks made so far (each 16 KiB).
    pub fn chunks(&self) -> usize {
        self.slots.iter().filter(|s| s.get().is_some_and(|c| c.is_some())).count()
    }

    /// Frames shared right now (count > 0) and the sum of their counts (diagnostics, ktests).
    pub fn census(&self) -> (u64, u64) {
        let (mut frames, mut sum) = (0u64, 0u64);
        for s in &self.slots {
            if let Some(Some(c)) = s.get() {
                for x in c.iter() {
                    let n = x.load(Ordering::Relaxed) as u64;
                    frames += (n > 0) as u64;
                    sum += n;
                }
            }
        }
        (frames, sum)
    }
}

static TABLE: Once<Option<Shares>> = Once::new();
static FILE_MAPS: Once<Option<Shares>> = Once::new();

/// A table for this machine, sized from the memory map (None without a frame allocator, or when its slot list
/// did not fit the heap).
fn make() -> Option<Shares> {
    make_with(Shares::new)
}

fn make_with(new: fn(u64) -> Option<Shares>) -> Option<Shares> {
    let max = mm_core::boot_buddy().map(|b| b.input_map().iter().map(|r| r.end).max().unwrap_or(0)).unwrap_or(0);
    if max == 0 { None } else { new(max) }
}

/// The table for this machine, made at first use (None: every share is refused and fork copies).
pub fn table() -> Option<&'static Shares> {
    TABLE.get_or_init(make).as_ref()
}

/// The table if a fork ever made it (the release paths never create it: no table, no shared frame).
fn existing() -> Option<&'static Shares> {
    TABLE.get().and_then(|t| t.as_ref())
}

pub fn share(phys: u64) -> bool {
    table().is_some_and(|t| t.share(phys))
}

pub fn release(phys: u64) -> bool {
    existing().is_none_or(|t| t.release(phys))
}

pub fn exclusive(phys: u64) -> bool {
    existing().is_none_or(|t| t.exclusive(phys))
}

pub fn others(phys: u64) -> u32 {
    existing().map_or(0, |t| t.others(phys))
}

// Map counts of page-cache frames (MM-ctr, for Pss and Shared_* of file pages in smaps; Linux reads
// `page_mapcount`). A second table of the same shape counts every mapping of the frame (not only the others):
// +1 when an address space maps it, -1 when the entry is gone. Only a number to show: a count that found no
// room (no heap for a chunk) seals that chunk's 16 MiB for good: its frames then show as mapped once (Pss too
// high, never corrupted by a later -1 of the missed +1); nothing is freed by these counts.

/// An address space now maps the page-cache frame `phys`.
pub fn map_file(phys: u64) {
    let Some(t) = FILE_MAPS.get_or_init(|| make_with(Shares::new_sealing)).as_ref() else { return };
    if !t.share(phys) {
        let first = FILE_MAP_MISSED.get() == 0;
        FILE_MAP_MISSED.inc();
        if first {
            klog::event!(Warn, "mm", "share.file_map_missed", phys = klog::Hex(phys);
                "no room for a page-cache frame's map count: its 16 MiB of frames show as mapped once in smaps (Pss)");
        }
    }
}

/// An address space no longer maps the page-cache frame `phys` (its entry is gone).
pub fn unmap_file(phys: u64) {
    if let Some(t) = FILE_MAPS.get().and_then(|t| t.as_ref()) {
        let _ = t.release(phys);
    }
}

/// How many address spaces map the page-cache frame `phys` (at least 1 for a mapped frame).
pub fn file_maps(phys: u64) -> u32 {
    FILE_MAPS.get().and_then(|t| t.as_ref()).map_or(0, |t| t.others(phys)).max(1)
}

klog::counter!(FILE_MAP_MISSED, "mm.share.file_map_missed");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_last_holder() {
        let t = Shares::new(64 << 20).unwrap();
        let f = 0x123_4000;
        assert!(t.exclusive(f) && t.chunks() == 0, "no chunk before the first share");
        assert!(t.release(f), "an unshared frame's only holder is the last");
        assert!(t.share(f) && t.share(f));
        assert_eq!(t.others(f), 2);
        assert_eq!(t.chunks(), 1);
        assert!(!t.release(f) && !t.release(f));
        assert!(t.exclusive(f));
        assert!(t.release(f), "the last holder frees");
        assert_eq!(t.census(), (0, 0));
    }

    #[test]
    fn outside_the_table_is_refused_not_lost() {
        let t = Shares::new(16 << 20).unwrap();
        assert!(!t.share(1 << 40), "a frame above the map cannot be shared: the caller copies");
        assert!(t.release(1 << 40));
    }

    #[test]
    fn saturation_refuses() {
        let t = Shares::new(16 << 20).unwrap();
        let f = 0x5000;
        assert!(t.share(f));
        Shares::cell(t.chunk(f).unwrap(), f).store(u32::MAX, Ordering::Relaxed);
        assert!(!t.share(f), "a full count refuses instead of wrapping");
        assert_eq!(t.others(f), u32::MAX);
    }

    #[test]
    fn a_sealed_slot_never_counts() {
        let t = Shares::new_sealing(16 << 20).unwrap();
        let f = 0x7000;
        t.slots[0].get_or_init(|| None);
        assert!(!t.share(f), "a sealed slot refuses");
        assert!(t.chunk_or_make(f).is_none() && t.chunks() == 0, "and is never made later");
        assert_eq!(t.others(f), 0);
        assert!(t.release(f));
    }

    #[test]
    fn chunks_are_per_16_mib() {
        let t = Shares::new(1 << 30).unwrap();
        for i in 0..8u64 {
            assert!(t.share(i * (CHUNK * PAGE) + 0x3000));
        }
        assert_eq!(t.chunks(), 8);
        assert_eq!(t.census(), (8, 8));
    }
}
