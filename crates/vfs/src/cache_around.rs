//! FS-W2: a write that goes to the backend's storage beside the cache (`PageCache::write_direct`): ext4 sends a
//! file's data from the file's own pages straight to the disk, while the disk's cache (`block::bdev::DiskFile`)
//! holds the volume's metadata and, now and then, a page of the very blocks that are written. Linux has the same
//! two views of one disk (a file's folios and the block device's mapping) and cleans the second for blocks a file
//! takes over (`clean_bdev_aliases`, linux-7/include/linux/buffer_head.h:213; ext4's `__ext4_forget` without a
//! journal does the same for one buffer: clear its dirty bit, wait for its write, forget it,
//! linux-7/fs/ext4/ext4_jbd2.c:292).
//!
//! What the call keeps true for the pages `[first, last]` of this cache:
//!
//! 1. **Nothing older lands after the direct write.** A write-back of these pages that is at the device when the
//!    call begins is waited for (the write-back lock). A page at the device stays in the index (the batch holds
//!    it and reclaim takes only pages nobody holds), so "no page of the range cached" means "none at the device":
//!    that is the fast path, and it takes no lock but `vfs.inode.data` for a look. Dirty pages found are written
//!    out first, never thrown away (they may be the only copy of bytes a committed change relies on: ext4 zeroes
//!    the rest of a truncated file's edge block through this cache). Then every page of the range leaves the cache.
//!    While the direct write runs the range is listed (`Direct::active`) and `collect_dirty` passes over it, so no
//!    write-back can begin on a page that appears there meanwhile.
//! 2. **Nothing older is read afterwards.** When the direct write has ended, whatever is cached of the range goes
//!    (a fill may have inserted bytes it read before or during the write), and a fill or read-ahead job still
//!    reading pages of the range inserts nothing and its reader reads again. Only those: every fill remembers how
//!    many direct writes had ended when it began (`Mark`), an ended write whose range a running fill reads is kept
//!    in `Direct::log` with its number, and a fill asks at its end whether a later one touched its pages
//!    (`Direct::fresh`). A fill elsewhere in the cache keeps its read (a download's data writes must not make the
//!    volume's metadata reads start over). A fill that could not note itself in `Inner::reading` is counted
//!    (`Direct::blind`); while one runs every ended write is logged, so it too reads again only for its own pages.
//!    Only when the log itself cannot grow do all running fills read again (`Direct::lost`).
//!
//! The caller's part: nobody else writes these blocks through the cache while the call runs (ext4: the blocks
//! belong to the step's file, and what could change them waits for the step, `fs_ext4::flight`). A page that is
//! dirty at the end broke that rule; it is dropped and reported (`cache.direct_dirty`), the direct bytes stand.
//!
//! Locks: `vfs.cache.sync` (this cache's) before `vfs.inode.data`, as every write-back; neither is held while
//! `write` runs, unless the list of ranges cannot grow (then the write-back lock stays held across the write: slower,
//! as correct).

use super::{Inner, PageCache, SyncKind};
use crate::Errno;
use alloc::vec::Vec;
use ksync::{Mutex, MutexGuard};
use uapi::errno::*;

klog::counter!(DIRECT_WRITES, "vfs.cache.direct_writes");
// Calls that met pages of their range in the cache (the slow path: the write-back lock, a write-back, the drop).
klog::counter!(DIRECT_MET_PAGES, "vfs.cache.direct_met_pages");
klog::counter!(DIRECT_DROPPED, "vfs.cache.direct_dropped_pages");
// Calls whose end made a running fill of their pages read again.
klog::counter!(DIRECT_SPOILED, "vfs.cache.direct_spoiled_fills");
// Calls the log could not take (no memory): every running fill reads again.
klog::counter!(DIRECT_LOG_LOST, "vfs.cache.direct_log_lost");

/// Rounds of "write the dirty pages out, look again" before the call gives up (EBUSY): only a writer that keeps
/// dirtying the range through the cache gets there, and it must not be written beside.
const WAY_ROUNDS: u32 = 4;

/// The direct writes of one cache, behind `vfs.inode.data`.
#[derive(Default)]
pub(crate) struct Direct {
    /// Page ranges [first, last] written beside the cache now: one entry per call in flight (R1).
    active: Vec<(u64, u64)>,
    /// Calls that ended: a call's number is this count when it ends.
    ended: u64,
    /// Ended calls a running fill may have read under: (number, first, last). An entry stays while a noted fill
    /// overlaps it or an un-noted one runs (`close_way` weeds it), so the list is as long as the direct writes
    /// that met a read, not as the download.
    log: Vec<(u64, u64, u64)>,
    /// The number of the last ended call the log could not take: a fill that began before it reads again.
    lost: u64,
    /// Fills running without a note in `Inner::reading` (the list could not grow): their pages are not known here,
    /// so every call that ends while one runs is logged.
    blind: u64,
}

/// What a fill or a read-ahead job remembers when it begins (under `vfs.inode.data`): the direct writes that had
/// ended. Dropped when the fill ends, on every path, and not under `vfs.inode.data`: an un-noted fill leaves the
/// count then.
pub(crate) struct Mark<'a> {
    seq: u64,
    blind: Option<&'a Mutex<Inner>>,
}

impl Drop for Mark<'_> {
    fn drop(&mut self) {
        if let Some(inner) = self.blind.take() {
            let mut g = inner.lock();
            g.direct.blind = g.direct.blind.saturating_sub(1);
        }
    }
}

impl Direct {
    /// Whether page `i` is written beside the cache now (a write-back leaves it alone).
    pub(crate) fn covers(&self, i: u64) -> bool {
        !self.active.is_empty() && self.active.iter().any(|&(a, b)| a <= i && i <= b)
    }
    /// A fill begins; `noted`: its pages are in `Inner::reading`, the cache `inner` guards.
    pub(crate) fn mark<'a>(&mut self, noted: bool, inner: &'a Mutex<Inner>) -> Mark<'a> {
        if !noted {
            self.blind += 1;
        }
        Mark { seq: self.ended, blind: (!noted).then_some(inner) }
    }
    /// Whether a fill of pages [first, end) that began at `mark` may insert what it read: no direct write of one
    /// of its pages ended since.
    pub(crate) fn fresh(&self, mark: &Mark<'_>, first: u64, end: u64) -> bool {
        mark.seq >= self.lost && !self.log.iter().any(|&(n, a, b)| n > mark.seq && a < end && first <= b)
    }
    /// A call over [first, last] ended; `reading`: the noted fills running now.
    fn end(&mut self, first: u64, last: u64, reading: &[(u64, u64, bool)]) {
        self.ended = self.ended.wrapping_add(1);
        let read = |a: u64, b: u64| reading.iter().any(|&(s, e, _)| s <= b && a < e);
        if self.blind == 0 {
            // What no running fill reads any more is no fill's question.
            self.log.retain(|&(_, a, b)| read(a, b));
        }
        let met = read(first, last);
        if met {
            DIRECT_SPOILED.inc();
        }
        if met || self.blind > 0 {
            if self.log.try_reserve(1).is_ok() {
                self.log.push((self.ended, first, last));
            } else {
                self.lost = self.ended;
                DIRECT_LOG_LOST.inc();
            }
        }
    }
    fn enter(&mut self, first: u64, last: u64) -> bool {
        if self.active.try_reserve(1).is_err() {
            return false;
        }
        self.active.push((first, last));
        true
    }
    fn leave(&mut self, first: u64, last: u64) {
        if let Some(at) = self.active.iter().position(|&r| r == (first, last)) {
            self.active.swap_remove(at);
        }
    }
}

#[cfg(test)]
impl PageCache {
    /// The mark of a fill that could not note itself.
    pub(crate) fn direct_blind_mark(&self) -> Mark<'_> {
        self.inner.lock().direct.mark(false, &self.inner)
    }
    pub(crate) fn direct_fresh(&self, mark: &Mark<'_>, first: u64, end: u64) -> bool {
        self.inner.lock().direct.fresh(mark, first, end)
    }
    /// (ended calls kept in the log, un-noted fills counted).
    pub(crate) fn direct_log(&self) -> (usize, u64) {
        let g = self.inner.lock();
        (g.direct.log.len(), g.direct.blind)
    }
}

impl Inner {
    fn any_cached(&self, first: u64, last: u64) -> bool {
        self.pages.next_with(first, |_, _| ()).is_some_and(|(i, ())| i <= last)
    }
    fn any_dirty(&self, first: u64, last: u64) -> bool {
        let mut at = first;
        while let Some((i, dirty)) = self.pages.next_with(at, |_, p| p.is_dirty()) {
            if i > last {
                break;
            }
            if dirty {
                return true;
            }
            match i.checked_add(1) {
                Some(n) => at = n,
                None => break,
            }
        }
        false
    }
    /// Takes every page of [first, last] out of the index (a reader that still copies from one keeps its frame);
    /// returns how many were dirty.
    fn drop_range(&mut self, first: u64, last: u64) -> u64 {
        let (mut at, mut dirty, mut dropped) = (first, 0u64, 0u64);
        while let Some((i, ())) = self.pages.next_with(at, |_, _| ()) {
            if i > last {
                break;
            }
            if let Some(p) = self.pages.remove(i) {
                if p.take_dirty() {
                    self.own_dirty = self.own_dirty.saturating_sub(1);
                    dirty += 1;
                }
                dropped += 1;
            }
            match i.checked_add(1) {
                Some(n) => at = n,
                None => break,
            }
        }
        DIRECT_DROPPED.add(dropped);
        dirty
    }
}

/// How a direct write keeps write-backs off its range: None when it is listed in `Direct::active`; the write-back
/// lock, held across the write, when the list could not grow.
type Way<'a> = Option<MutexGuard<'a, ()>>;

impl PageCache {
    /// Runs `write`, which puts new bytes for the whole pages `[first, last]` on the backend's storage without
    /// this cache, and keeps the cache right about them (the module text): what was at the device or dirty of the
    /// range reaches the device before `write` runs, no write-back touches the range while it runs, and afterwards
    /// the cache holds nothing of the range, so the next read brings the new bytes in. Also after a failed
    /// `write` (its error is the answer): the storage's bytes are then whatever the device made of it, and the
    /// cache holds none of them.
    ///
    /// An error before `write` ran (a dirty page of the range could not be written out, no memory for it, EBUSY
    /// when the range keeps being dirtied through the cache): `write` did not run, the pages are still cached.
    /// A timeout among them (what the backend's `write_run` answers for one, ETIMEDOUT for ext4's disk) is, like a
    /// timed-out `write`, a request to these very blocks the device may still hold: the caller treats both alike
    /// (`fs_ext4::flight` keeps the blocks out of use), so the two need no telling apart.
    pub fn write_direct(&self, first: u64, last: u64, write: impl FnOnce() -> Result<(), Errno>) -> Result<(), Errno> {
        if self.backend.is_none() || last < first {
            return Err(EINVAL);
        }
        let way = self.make_way(first, last)?;
        DIRECT_WRITES.inc();
        let r = write();
        self.close_way(first, last, way);
        r
    }

    fn make_way(&self, first: u64, last: u64) -> Result<Way<'_>, Errno> {
        {
            let mut g = self.inner.lock();
            if !g.any_cached(first, last) && g.direct.enter(first, last) {
                return Ok(None);
            }
        }
        DIRECT_MET_PAGES.inc();
        // The write-back lock: a write-back of these pages in flight has ended once it is ours. As a durable
        // write-back, so the pages do not go at background priority with the lock held (the backend of a disk's
        // cache does not flush the device for it: `DiskFile`'s owner does).
        let one = self.wb_lock();
        for _ in 0..WAY_ROUNDS {
            self.sync_ranges_locked(&one, &[(first, last)], SyncKind::Fsync)?;
            let mut g = self.inner.lock();
            if g.any_dirty(first, last) {
                continue;
            }
            g.drop_range(first, last);
            return Ok(if g.direct.enter(first, last) { None } else { Some(one) });
        }
        klog::event!(Error, "vfs", "cache.direct_busy", first = first, pages = last - first + 1; "pages to be written beside the cache keep being dirtied through it: not written beside");
        Err(EBUSY)
    }

    fn close_way(&self, first: u64, last: u64, way: Way<'_>) {
        let mut g = self.inner.lock();
        let dirty = g.drop_range(first, last);
        // A fill that read pages of the range before the write ended holds old bytes: it finds this call in the
        // log and inserts nothing. Its waiters look again when it ends, as after any fill.
        let i = &mut *g;
        i.direct.end(first, last, &i.reading);
        if way.is_none() {
            i.direct.leave(first, last);
        }
        drop(g);
        drop(way);
        if dirty > 0 {
            klog::event!(Error, "vfs", "cache.direct_dirty", first = first, pages = dirty; "blocks were written through the cache while they were written beside it: the cache's bytes are dropped");
        }
    }
}
