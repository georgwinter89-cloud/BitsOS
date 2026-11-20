//! FS-W: one step of a file's write-back (`Volume::write`, at most `file::WRITE_STEP_PAGES`) in three parts, the
//! middle one with the volume lock released, so a download's data at the device holds up no other call on the volume.
//!
//! 1. `write_begin`, under `ext4.vol`: the range is mapped; for its holes blocks are picked and *claimed* in memory
//!    only (`Flights::claims`). No bitmap, no count and no transaction knows of them yet; the allocator skips them
//!    and every free-space question counts them as taken (`State::free_now`).
//! 2. `write_segs`, no lock: the data goes to the device. FS-W2: to the device itself, beside the cache a disk has
//!    for the volume's metadata (`Dev::write_pages`), so a journal commit's flush carries no file data.
//! 3. `write_end`, under `ext4.vol`: the claimed blocks are marked used, the extents, the size and the inode follow,
//!    all in one transaction that joins the running journal transaction now.
//!
//! Ordered mode holds as before FS-W, by the order of the program: the transaction that makes the mapping joins
//! only after the device took the data, so the commit that carries it began later and its flush before the commit
//! block makes the data stable first. A commit by another thread between 1 and 3 holds nothing of the step. A cut
//! before 3 is committed leaves the blocks free on the disk with the data in them (harmless: only blocks that are
//! free in the last committed state are claimed, the journal's busy blocks are skipped as the allocator skips them);
//! there is no state in which blocks are marked used and nothing points at them. Linux reaches the same end with
//! unwritten extents that its I/O completion converts (`mpage_map_one_extent` with EXT4_GET_BLOCKS_UNWRIT_EXT,
//! linux-7/fs/ext4/inode.c:2345, then `ext4_end_io_end` -> `ext4_convert_unwritten_io_end_vec`, page-io.c:167);
//! V2 keeps the allocation itself back instead, so a cut never leaves an extent to convert.
//!
//! Without a journal the one transaction of 3 writes the bitmaps before the descriptors and those before the
//! pointers (`tx`'s order), after the data: a cut leaves free blocks holding data, or blocks marked used that
//! nothing points at yet (e2fsck's bitmap differences), never a pointer at a free block or at data not written.
//! That order is the order of `Dev::write`, so it is the disk's only where the device has no cache of its own in
//! front (`MemDev`, an image file). A disk's cache (`CachedDiskDev`) writes the pages of a flush in page order: on
//! a disk a volume without a journal has Linux's guarantee without one, e2fsck can repair a cut, no more. FS-W2
//! must not make that worse: with the data at the disk at once, a block freed by a delete that still sits in the
//! disk's cache would be overwritten under a file the disk still has, for as long as until the next flush. So
//! such a volume hands a freed block out only after a device flush (`held`).
//!
//! One step per file at a time (`Flights::files`). What would change the file's mapping or free its blocks while a
//! step is at the device waits for it (`Volume::lock_file`: another step, truncate, fallocate, a freeze) or is done
//! by the step's end (`evict` of an unlinked file: `Flights::evict_after`). So the mapping `write_end` meets is the
//! one `write_begin` saw, and no block the step writes to can be freed and handed to another file under it.
//! `write_end` checks that once more before it maps (`same_mapping`). The VFS already keeps a file's write-backs
//! apart (`vfs.cache.sync`) and makes truncate wait for them; the volume does not rely on it.

use crate::dev::Dev;
use crate::errno::*;
use crate::extent::{self, Edit, Target};
use crate::inode::{DiskInode, FL_INLINE_DATA};
use crate::sb::Geo;
use crate::state::State;
use crate::tx::Tx;
use crate::vol_data::IO_MAX;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use vfs::pagecache::CachePage;

/// A piece of a step: file blocks [l, l + n) at disk block `p`; `fresh` when claimed for this step, `unwritten`
/// when it was a preallocated (unwritten) extent.
struct Seg {
    l: u64,
    n: u64,
    p: u64,
    fresh: bool,
    unwritten: bool,
}

/// A step between `write_begin` and `write_end`.
pub struct Step {
    ino: u32,
    first: u64,
    segs: Vec<Seg>,
    goal: u64,
}

/// A file's step in flight: the file blocks [lo, hi) it writes.
struct Flying {
    lo: u64,
    hi: u64,
    /// The file lost its last name and holder meanwhile: `write_end` frees it instead of mapping.
    evict: bool,
}

/// The steps at the device now, behind the volume lock.
#[derive(Default)]
pub struct Flights {
    files: BTreeMap<u32, Flying>,
    /// Blocks picked for a step's holes: start -> end (exclusive), one entry per piece.
    claims: BTreeMap<u64, u64>,
    claimed: u64,
    /// FS-W2: blocks a request that timed out may still be written to by the device (`State::write_end`): start ->
    /// end (exclusive), merged. Never handed out again in this mount.
    late: BTreeMap<u64, u64>,
    /// How many blocks of `late` the bitmaps show free (the others still belong to files).
    late_free: u64,
}

impl Flights {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
    /// Whether a step of file `ino` is at the device.
    pub fn has(&self, ino: u32) -> bool {
        self.files.contains_key(&ino)
    }
    /// Blocks claimed and not yet marked used.
    pub fn claimed(&self) -> u64 {
        self.claimed
    }
    /// Whether file block `l` of `ino` is written by a step in flight (mapped when it ends).
    pub fn covers(&self, ino: u32, l: u64) -> bool {
        self.files.get(&ino).is_some_and(|f| l >= f.lo && l < f.hi)
    }
    pub(crate) fn claims_in(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.claims.range(..end).filter(move |(_, e)| **e > start).map(|(s, e)| (*s, *e))
    }
    fn claim(&mut self, p: u64, n: u64) {
        self.claims.insert(p, p + n);
        self.claimed += n;
    }
    /// Gives the claim at `p` back (nothing when it was taken or given back already).
    pub(crate) fn unclaim(&mut self, p: u64, n: u64) {
        if self.claims.remove(&p).is_some() {
            self.claimed = self.claimed.saturating_sub(n);
        }
    }
    pub(crate) fn late_in(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.late.range(..end).filter(move |(_, e)| **e > start).map(|(s, e)| (*s, *e))
    }
    /// Blocks [p, p + n) are kept out of use until the next mount; `free`: the bitmaps show them free (a claim's).
    /// A file whose step times out twice brings the same blocks again: one merged range, each block counted once.
    pub(crate) fn quarantine(&mut self, p: u64, n: u64, free: bool) {
        let (mut a, mut b) = (p, p + n);
        while let Some((s, e)) = self.late.range(..=b).next_back().map(|(s, e)| (*s, *e)).filter(|&(_, e)| e >= a) {
            self.late.remove(&s);
            (a, b) = (a.min(s), b.max(e));
        }
        self.late.insert(a, b);
        if free {
            self.late_free += n;
        }
    }
    fn late_overlap(&self, p: u64, n: u64) -> u64 {
        self.late_in(p, p + n).map(|(s, e)| e.min(p + n) - s.max(p)).sum()
    }
    /// Blocks [p, p + n) were freed in a transaction: those of `late` are free now and still not to be had.
    pub(crate) fn late_freed(&mut self, p: u64, n: u64) {
        if !self.late.is_empty() {
            self.late_free += self.late_overlap(p, n);
        }
    }
    /// How many blocks of `late` the frees `ranges` of one transaction counted free.
    pub(crate) fn late_among(&self, ranges: &[(u64, u64)]) -> u64 {
        if self.late.is_empty() { 0 } else { ranges.iter().map(|&(p, n)| self.late_overlap(p, n)).sum() }
    }
    /// The transaction that freed them was undone (`State::abort`): they belong to their file again.
    pub(crate) fn late_unfreed(&mut self, blocks: u64) {
        self.late_free = self.late_free.saturating_sub(blocks);
    }
    /// An unlinked file nobody holds is freed now (false), or by its step's end (true).
    fn evict_after(&mut self, ino: u32) -> bool {
        match self.files.get_mut(&ino) {
            Some(f) => {
                f.evict = true;
                true
            }
            None => false,
        }
    }
}

impl State {
    /// Free blocks an allocation may still count on: the superblock's count less the blocks claimed by steps in
    /// flight (theirs already, only not marked yet) and the free blocks a late write may still land on.
    pub fn free_now(&self) -> u64 {
        self.free_blocks().saturating_sub(self.flights.claimed + self.flights.late_free)
    }

    /// Part 1 of a step: maps `len` bytes at `off` (block aligned) of regular file `ino` and claims blocks for the
    /// holes. `keep`: blocks other files' cached data was promised. The file must have no step in flight
    /// (`Volume::lock_file`).
    pub fn write_begin(&mut self, ino: u32, off: u64, len: usize, keep: u64) -> Result<Step, i64> {
        let bs = self.geo.bs;
        if self.flights.has(ino) {
            return Err(EBUSY);
        }
        let mut di = self.inode(None, ino)?;
        if !di.is_reg() {
            return Err(EINVAL);
        }
        if di.links() == 0 && !self.nodes.get(&ino).is_some_and(|n| n.holders > 0) {
            // Unlinked and nobody holds it: freed, or about to be. Its blocks are not this caller's any more.
            return Err(ENOENT);
        }
        if !extent::uses_extents(&di) {
            self.migrate(ino)?;
            di = self.inode(None, ino)?;
        }
        if di.flags() & FL_INLINE_DATA != 0 {
            // Only on an inline_data volume, which mounts read-only (`sb::parse`).
            return Err(EROFS);
        }
        if off % bs != 0 {
            return Err(EINVAL);
        }
        let first = off / bs;
        let n = (len as u64).div_ceil(bs);
        if first + n > 1 << 32 {
            return Err(EFBIG);
        }
        let mut segs: Vec<Seg> = Vec::new();
        let mut goal = self.file_goal(&di, first)?;
        let mut l = first;
        let r = (|| -> Result<(), i64> {
            while l < first + n {
                let want = first + n - l;
                segs.try_reserve(1).map_err(|_| ENOMEM)?;
                match self.map_at(None, &di, l)? {
                    Ok((p, m, unwritten)) => {
                        let m = m.min(want);
                        segs.push(Seg { l, n: m, p, fresh: false, unwritten });
                        goal = p + m;
                        l += m;
                    }
                    Err(hole) => {
                        let m = self.grant(hole.min(want).min(extent::INIT_MAX as u64), keep)?;
                        let (g, at, got, _) = self.free_run(None, goal, m)?;
                        let p = self.geo.group_start(g) + at;
                        self.flights.claim(p, got);
                        segs.push(Seg { l, n: got, p, fresh: true, unwritten: false });
                        goal = p + got;
                        l += got;
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = r {
            self.unclaim_all(&segs);
            return Err(e);
        }
        self.flights.files.insert(ino, Flying { lo: first, hi: first + n, evict: false });
        Ok(Step { ino, first, segs, goal })
    }

    /// Frees an inode whose last name went and that nobody holds (`evict`); while a step of it is at the device,
    /// the step's end does it (the inode is on the orphan list until then: a cut leaves it to the next mount).
    pub fn evict_unheld(&mut self, ino: u32) -> Result<(), i64> {
        if self.flights.evict_after(ino) { Ok(()) } else { self.evict(ino, true) }
    }

    /// A step that failed: the cache keeps its pages dirty and writes them again. Their promises were given back
    /// when the step began, and a write(2) into a page of a step in flight takes none (`reserve_pages`). So the
    /// step's holes get promises again: exactly the blocks its claims held, given back just before under the same
    /// hold of the lock, so the room is there. Without them the pages' next write-back could meet a volume that
    /// other writers filled meanwhile.
    fn promise_holes(&mut self, step: &Step) {
        let per_page = (4096 / self.geo.bs).max(1);
        let Some(n) = self.nodes.get_mut(&step.ino) else { return };
        let mut blocks = 0u64;
        for s in step.segs.iter().filter(|s| s.fresh) {
            for l in s.l..s.l + s.n {
                *n.reserved.entry(l / per_page).or_insert(0) += 1;
            }
            blocks += s.n;
        }
        self.reserved += blocks;
    }

    fn unclaim_all(&mut self, segs: &[Seg]) {
        for s in segs.iter().filter(|s| s.fresh) {
            self.flights.unclaim(s.p, s.n);
        }
    }

    /// Part 3 of a step: `wrote` is part 2's answer (the bytes that reached the device). The claimed blocks, the
    /// extents, the size (grown to cover what was written, never past `size`) and the inode in one transaction; a
    /// failed data write only gives the claims back (EIO to the caller, the volume stays writable as on Linux: the
    /// disk never heard of the blocks); one that timed out keeps the step's blocks out of use (below).
    pub fn write_end(&mut self, step: &Step, wrote: Result<u64, i64>, off: u64, len: usize, size: u64) -> Result<(), i64> {
        let marked = self.flights.files.remove(&step.ino).is_some_and(|f| f.evict);
        // Still without a name and a holder: the file goes now, and a file that goes gets no mapping.
        let gone = marked && self.inode(None, step.ino).is_ok_and(|d| d.links() == 0) && !self.nodes.get(&step.ino).is_some_and(|n| n.holders > 0);
        // FS-W2: a request that timed out may still be at the device (the block layer drops its answer, it cannot
        // take the command back) and land whenever the device gets to it. Every block of the step is kept out of
        // use until the next mount, so nothing else is put where it could land: the new blocks (free on the disk,
        // only this mount does not use them) and the ones the file already had (a later truncate or unlink frees
        // them; without this they would go to another file or to a directory after the next commit). Also when
        // the request was the write-out of an old page of these blocks before the data (`PageCache::write_direct`
        // answers that with the same error): it is a late write to the same blocks.
        let late = wrote == Err(ETIMEDOUT);
        let r = match wrote {
            Err(e) => Err(e),
            Ok(bytes) => {
                self.written += bytes;
                if gone { Ok(()) } else { self.map_step(step, off, len, size) }
            }
        };
        if late {
            for s in &step.segs {
                if s.fresh {
                    self.flights.unclaim(s.p, s.n);
                }
                self.flights.quarantine(s.p, s.n, s.fresh);
            }
            let blocks: u64 = step.segs.iter().map(|s| s.n).sum();
            klog::event!(Warn, "ext4", "flight.blocks_kept", ino = step.ino, blocks = blocks; "a write to a file's blocks timed out at the device: the blocks are left unused until the next mount");
        } else {
            // Whatever `map_step` did not take (it failed before, or never ran).
            self.unclaim_all(&step.segs);
        }
        if r.is_err() && !gone {
            self.promise_holes(step);
        }
        if gone && self.writable {
            if let Err(e) = self.evict(step.ino, true) {
                klog::event!(Warn, "ext4", "evict.failed", ino = step.ino, errno = e);
            }
        }
        r
    }

    /// The mapping `write_begin` saw still stands: nothing but this step may have changed it (the module text).
    /// A difference is a bug in that exclusion; mapping over it would lose blocks or point two files at one.
    fn same_mapping(&self, di: &DiskInode, segs: &[Seg]) -> Result<(), i64> {
        for s in segs {
            let same = match self.map_at(None, di, s.l)? {
                Ok((p, m, unwritten)) => !s.fresh && p == s.p && m >= s.n && unwritten == s.unwritten,
                Err(hole) => s.fresh && hole >= s.n,
            };
            if !same {
                klog::event!(Error, "ext4", "write.mapping_changed", ino = di.ino, lblk = s.l, blocks = s.n; "a file's mapping changed under its write-back step: the step is dropped");
                return Err(EIO);
            }
        }
        Ok(())
    }

    fn map_step(&mut self, step: &Step, off: u64, len: usize, size: u64) -> Result<(), i64> {
        let bs = self.geo.bs;
        if !self.writable {
            return Err(EROFS);
        }
        // Read again: the inode's times, owner or xattrs may have changed while the data was written.
        let mut di = self.inode(None, step.ino)?;
        if !di.is_reg() || !extent::uses_extents(&di) {
            return Err(EIO);
        }
        self.same_mapping(&di, &step.segs)?;
        let journal = self.jnl.is_some();
        let mut tx = Tx::new();
        let r = (|| -> Result<Vec<(u64, u64)>, i64> {
            for s in step.segs.iter().filter(|s| s.fresh) {
                self.take_claimed(&mut tx, s.p, s.n)?;
            }
            let mut fresh = 0u64;
            let (tree, freed) = {
                let mut ed = Edit::new(self, &mut tx, &mut di, step.goal);
                for s in step.segs.iter().filter(|s| s.fresh || s.unwritten) {
                    ed.set(s.l as u32, s.n, Target::Map { pblk: s.p, unwritten: false })?;
                    if s.fresh {
                        fresh += s.n;
                    }
                }
                (ed.tree_blocks, core::mem::take(&mut ed.freed))
            };
            let new_size = di.size().max(size.min(off + len as u64));
            di.set_size(new_size);
            let b512 = di.blocks512(&self.geo) as i64 + (fresh as i64 + tree) * (bs / 512) as i64;
            di.set_blocks512(&self.geo, b512.max(0) as u64)?;
            let t = self.now();
            di.set_time(crate::inode::Stamp::C, t);
            self.put_inode(&mut tx, &mut di)?;
            if journal {
                // One journal transaction: the frees cannot be cut off from the pointers' going.
                for &(p, n) in &freed {
                    self.free_blocks_range(&mut tx, p, n)?;
                }
            }
            Ok(freed)
        })();
        match r {
            Ok(freed) => {
                self.commit(tx)?;
                // Without a journal a free is written in a transaction after the one that dropped the pointer.
                if journal { Ok(()) } else { self.free_list(&freed) }
            }
            Err(e) => {
                self.abort(tx);
                Err(e)
            }
        }
    }
}

/// What a step writes: bytes (a kept buffer, tests), or the file's cache pages themselves, `len` bytes from the
/// first page's start (FS-W2: a disk takes the pages as its request's memory, nothing is copied).
pub enum Src<'a> {
    Bytes(&'a [u8]),
    Pages(&'a [Arc<CachePage>], usize),
}

impl Src<'_> {
    pub fn len(&self) -> usize {
        match self {
            Src::Bytes(d) => d.len(),
            Src::Pages(_, len) => *len,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Copies the bytes from `at` on into `out` (a short last block, a device that takes no pages).
    fn copy(&self, at: usize, out: &mut [u8]) -> Result<(), i64> {
        match self {
            Src::Bytes(d) => out.copy_from_slice(d.get(at..at + out.len()).ok_or(EIO)?),
            Src::Pages(pages, _) => {
                let mut done = 0;
                while done < out.len() {
                    let (i, o) = ((at + done) / PAGE, (at + done) % PAGE);
                    let n = (PAGE - o).min(out.len() - done);
                    pages.get(i).ok_or(EIO)?.read(o, &mut out[done..done + n]);
                    done += n;
                }
            }
        }
        Ok(())
    }
}

const PAGE: usize = 4096;

/// Part 2 of a step, no lock held: the data of every piece to its blocks (a short last block padded with zeros).
/// Returns the bytes written.
///
/// FS-W2: the data goes to the device itself (`Dev::write_data`, `Dev::write_pages`), not into the cache a disk
/// has for the volume's metadata, where it waited for the next journal commit's flush and was copied once more.
/// Only where a block is whole pages of that cache (4 KiB blocks): a smaller block shares its page with other
/// blocks, metadata among them, and keeps going through `Dev::write`.
///
/// Ordered mode with the data beside the cache: this call returns when the device answered every write, so the
/// transaction that maps the step (`write_end`, after this) joins a journal transaction whose commit begins later;
/// that commit's flush before its commit block (`Plan::run`) is a flush of the device after these writes ended, and
/// makes them durable first. One device flush covers every step of every file that ended before it; a step still
/// here is in no transaction yet.
pub fn write_segs(dev: &dyn Dev, geo: &Geo, step: &Step, src: &Src<'_>) -> Result<u64, i64> {
    let bs = geo.bs;
    let direct = bs % PAGE as u64 == 0;
    let total = src.len();
    let mut pad = Vec::new();
    let mut bytes = 0u64;
    let inside = |blk: u64, len: usize| -> Result<(), i64> {
        let n = (len as u64).div_ceil(bs);
        if blk.checked_add(n).is_none_or(|e| e > geo.blocks) || blk < geo.first_data_block { Err(EUCLEAN) } else { Ok(()) }
    };
    for s in &step.segs {
        let mut b = 0u64;
        while b < s.n {
            let at = ((s.l - step.first + b) * bs) as usize;
            let cnt = ((s.n - b) as usize).min(IO_MAX / bs as usize);
            let end = (at + cnt * bs as usize).min(total);
            let len = end.checked_sub(at).ok_or(EIO)?;
            // The whole blocks straight from the caller's memory; only a short last block goes through a copy
            // of one block (a copy of the whole piece was up to 1 MiB from the heap, inside write-back).
            let whole = len / bs as usize * bs as usize;
            if whole > 0 {
                inside(s.p + b, whole)?;
                let to = (s.p + b) * bs;
                match src {
                    Src::Bytes(d) if direct => dev.write_data(to, &d[at..at + whole])?,
                    Src::Bytes(d) => dev.write(to, &d[at..at + whole])?,
                    Src::Pages(pages, _) if direct => {
                        let mut segs = Vec::new();
                        crate::vol_pages::add_segs(&mut segs, pages, at, whole)?;
                        dev.write_pages(to, segs)?;
                    }
                    Src::Pages(..) => {
                        // Not asked for by the write-back (`file`: pages only with 4 KiB blocks); right all the same.
                        let mut tmp = Vec::new();
                        tmp.try_reserve_exact(whole).map_err(|_| ENOMEM)?;
                        tmp.resize(whole, 0);
                        src.copy(at, &mut tmp)?;
                        dev.write(to, &tmp)?;
                    }
                }
                bytes += whole as u64;
            }
            if whole < len {
                pad.clear();
                pad.try_reserve_exact(bs as usize).map_err(|_| ENOMEM)?;
                pad.resize(bs as usize, 0);
                src.copy(at + whole, &mut pad[..len - whole])?;
                let blk = s.p + b + whole as u64 / bs;
                inside(blk, pad.len())?;
                if direct { dev.write_data(blk * bs, &pad)? } else { dev.write(blk * bs, &pad)? }
                bytes += bs;
            }
            b += cnt as u64;
        }
    }
    Ok(bytes)
}
