//! MM-rmap (FS-14 change request 1): which address spaces map a file, so a hole punch or a truncate can take
//! the file's pages out of every mapping (Linux `unmap_mapping_range`, linux-6.6/mm/memory.c:3565, over the
//! file's `i_mmap` tree; here at space granularity: the file's entry lists the spaces, each space's own VMA
//! tree gives the ranges). Then the page cache can free the pages and give their charge back, and the next
//! touch faults a fresh zero page inside the file or SIGBUS past its end.
//!
//! The registry is one sorted list of (file, space) pairs: a space is added when it maps a file (mmap, fork)
//! and removed when its last VMA of that file goes (munmap, MAP_FIXED, mremap's cut, the space's drop). Keys
//! are the file's `Inode` address: a mapping keeps its inode alive, so a live entry never names another file
//! (a stale one only costs a look at that space).
//!
//! Locking: `mm.rmap` (spin, short, allocates) is taken under `mm.as` and never the other way round: an unmap
//! copies the spaces out first and then takes each space's `mm.as` alone, one at a time. vfs calls the
//! unmapper without any of its own locks (before `vfs.inode.data`), so the order stays `vfs.file.pos` > `mm.as`
//! > `mm.rmap`, `vfs.inode.data`, `vfs.map.pins` (R5).

use super::{AsCore, Inner, Owner};
use crate::vma::{FileRef, VmaKind};
use crate::PAGE;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use hal::paging;
use ksync::SpinLock;
use vfs::mmap::Unmapper;
use vfs::Inode;

struct Entry {
    key: usize,
    space: Weak<AsCore>,
}

impl Entry {
    fn order(&self) -> (usize, usize) {
        (self.key, self.space.as_ptr() as usize)
    }
}

static FILES: SpinLock<Vec<Entry>> = SpinLock::named("mm.rmap", Vec::new());

klog::counter!(RMAP_MISSED, "mm.rmap.missed");
klog::counter!(RMAP_UNMAPS, "mm.rmap.unmaps");
klog::counter!(RMAP_SPACES, "mm.rmap.spaces_visited");

/// The registry key of a file VMA (None: a device's VMA, or a backing without a file).
pub(crate) fn key_of(f: &FileRef) -> Option<usize> {
    if f.device.is_some() {
        return None;
    }
    f.backing.file().map(|m| Arc::as_ptr(m.inode()) as usize)
}

/// The key of the owner of an unmapped segment.
fn key_of_owner(o: &Owner) -> Option<usize> {
    match o {
        Owner::File(b, _) => b.file().map(|m| Arc::as_ptr(m.inode()) as usize),
        _ => None,
    }
}

/// Remembers that `space` maps file `key` (once). False: no room (the caller refuses the mapping).
pub(crate) fn add(key: usize, space: &Weak<AsCore>) -> bool {
    let want = (key, space.as_ptr() as usize);
    let mut g = FILES.lock();
    match g.binary_search_by(|e| e.order().cmp(&want)) {
        Ok(_) => true,
        Err(i) => {
            if g.try_reserve(1).is_err() {
                drop(g);
                RMAP_MISSED.inc();
                klog::event!(Warn, "mm", "rmap.no_room"; "no room in the reverse map: the mapping is refused (ENOMEM)");
                return false;
            }
            g.insert(i, Entry { key, space: space.clone() });
            true
        }
    }
}

/// Forgets that the space at `space` maps file `key`.
fn remove(key: usize, space: *const AsCore) {
    let want = (key, space as usize);
    let mut g = FILES.lock();
    if let Ok(i) = g.binary_search_by(|e| e.order().cmp(&want)) {
        // Dropping a Weak never frees the space's memory under this lock in a way that calls back here: the
        // AsCore's own drop has run already when only weak references are left.
        g.remove(i);
    }
}

/// The next space mapping file `key` after the one at address `after` (0: the first), without allocating:
/// (its address, the space if it still lives). Found by order, so entries added or removed meanwhile (a space
/// that ends during the walk) never make the walk skip one.
fn next_space(key: usize, after: usize) -> Option<(usize, Option<Arc<AsCore>>)> {
    let g = FILES.lock();
    let i = g.partition_point(|e| e.order() <= (key, after));
    let e = g.get(i).filter(|e| e.key == key)?;
    Some((e.space.as_ptr() as usize, e.space.upgrade()))
}

/// How many spaces the registry lists for file `inode` (ktests).
pub fn spaces_of(inode: &Arc<Inode>) -> usize {
    let key = Arc::as_ptr(inode) as usize;
    let g = FILES.lock();
    g.iter().filter(|e| e.key == key).count()
}

impl AsCore {
    /// After VMAs of the files in `segs` left the tree: forgets the files this space no longer maps.
    /// The tree is looked at once per file of `segs` (a range cut out of one file VMA gives runs of the same
    /// file), so the cost is the space's VMAs per unmapped file, beside the unmap's own page-table work.
    pub(crate) fn rmap_forget(&self, g: &Inner, segs: &super::Segs) {
        let mut last = None;
        for (_, _, o) in segs {
            if let Some(k) = key_of_owner(o)
                && last != Some(k)
            {
                last = Some(k);
                self.rmap_forget_key(g, k);
            }
        }
    }

    /// Forgets file `key` for this space when no VMA of it is left.
    pub(crate) fn rmap_forget_key(&self, g: &Inner, key: usize) {
        if !g.space.tree.iter().any(|v| v.file.as_ref().and_then(key_of) == Some(key)) {
            remove(key, self);
        }
    }

    /// The space's drop (`me` is the dying space): every entry of it goes, also one a failed fork made for a
    /// VMA its tree never got. One pass over the registry: removing the space's k entries one by one from the
    /// sorted vector would move its tail k times, so a single `retain` is the cheaper walk.
    pub(crate) fn rmap_drop_of(me: *const AsCore) {
        FILES.lock().retain(|e| e.space.as_ptr() != me);
    }

    /// Takes file pages `first..=last` of file `key` out of this space (see `Unmapper::unmap`). A stale entry (no
    /// VMA of the file is left) goes.
    fn rmap_unmap(&self, key: usize, first: u64, last: u64, even_cows: bool) {
        let mut g = self.inner.lock();
        let mut found = false;
        let mut at = 0u64;
        // One VMA at a time by address (a clone holds only Arcs: no allocation, and the tree may be read again
        // after each unmap, which changes page tables only).
        while let Some(v) = g.space.tree.next_from(at).cloned() {
            at = v.end;
            let Some(f) = v.file.as_ref().filter(|f| key_of(f) == Some(key)) else { continue };
            found = true;
            let pages = v.pages();
            let Some(top) = f.pgoff.checked_add(pages - 1) else { continue };
            let (lo, hi) = (first.max(f.pgoff), last.min(top));
            if lo > hi {
                continue;
            }
            let (s, e) = (v.start + (lo - f.pgoff) * PAGE, v.start + (hi - f.pgoff + 1) * PAGE);
            let owner = Owner::File(f.backing.clone(), v.kind == VmaKind::Anonymous);
            if even_cows || !v.private() {
                self.unmap_one(&mut g, s, e, &owner);
            } else {
                self.unmap_cache_frames(&mut g, s, e, &owner);
            }
        }
        if !found {
            remove(key, self);
        }
    }

    /// A private VMA's hole punch: only the entries that map page-cache frames go (runs of them, one shootdown
    /// per run); the program's private copies keep their data (Linux `zap_details.even_cows = false`).
    fn unmap_cache_frames(&self, g: &mut Inner, s: u64, e: u64, owner: &Owner) {
        let mut run: Option<u64> = None;
        let mut va = s;
        while va < e {
            let cached = paging::translate(self.root, va).is_some_and(|(phys, _)| !mm_core::is_user_frame(phys & !0xFFF));
            match (cached, run) {
                (true, None) => run = Some(va),
                (false, Some(r)) => {
                    self.unmap_one(g, r, va, owner);
                    run = None;
                }
                _ => {}
            }
            va += PAGE;
        }
        if let Some(r) = run {
            self.unmap_one(g, r, e, owner);
        }
    }
}

/// The reverse map vfs calls on hole punch and truncate (`vfs::mmap::set_unmapper`).
struct Rmap;

impl Unmapper for Rmap {
    fn unmap(&self, inode: &Inode, first: u64, last: u64, even_cows: bool) {
        let key = inode as *const Inode as usize;
        let (mut after, mut spaces) = (0usize, 0u64);
        // One space at a time, copied out of the registry before its lock is taken (`mm.as` > `mm.rmap`).
        while let Some((at, s)) = next_space(key, after) {
            after = at;
            let Some(s) = s else { continue };
            spaces += 1;
            s.rmap_unmap(key, first, last, even_cows);
        }
        if spaces > 0 {
            RMAP_UNMAPS.inc();
            RMAP_SPACES.add(spaces);
            klog::event!(Debug, "mm", "rmap.unmap", ino = inode.meta.ino, first = first, to_end = last == u64::MAX, last = last, spaces = spaces, even_cows = even_cows;
                "file pages taken out of every mapping (hole punch or truncate)");
        }
    }
}

static RMAP: Rmap = Rmap;

/// Core initcall: vfs's punch and truncate call the reverse map from now on.
fn install() -> Result<(), &'static str> {
    if !vfs::mmap::set_unmapper(&RMAP) {
        return Err("an unmapper was installed already");
    }
    Ok(())
}
kreg::initcall!(kreg::Level::Core, "mm.rmap", install);
