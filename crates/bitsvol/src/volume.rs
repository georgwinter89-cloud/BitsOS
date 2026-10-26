//! Volumes and snapshots: create, snapshot, delete, rollback, and the data path (read, write, discard).
//!
//! A write into a chunk mapped after the volume's last snapshot goes to the chunk in place (no other tree can
//! name it, not even the committed one); any other write gets a fresh chunk (a partial write copies the rest of
//! the old chunk, Linux dm-thin break_sharing). Out of space: commit to release freed chunks, then drop the
//! oldest automatic snapshot, then ENOSPC.

use crate::error::{Result, VolError};
use crate::layout::{FLAG_AUTO, FLAG_READ_ONLY, FLAG_ROLLBACK, FLAG_SNAPSHOT, KIND_RECORD, LABEL_BOUND, Record, Tree};
use crate::pool::{Pool, Vol, zeroed};
use alloc::string::String;
use alloc::vec::Vec;
use devmodel::block::IoPriority;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    pub id: u64,
    pub origin: u64,
    pub label: String,
    pub size: u64,
    pub snapshot: bool,
    pub read_only: bool,
    pub auto: bool,
    pub created_ns: u64,
    /// A snapshot its origin goes back to at the origin's next open (VS-3).
    pub rollback_pending: bool,
}

/// Mapped chunks handled per tree walk while discarding (keeps the walk's buffer small).
const WALK_BATCH: usize = 1024;

impl Pool {
    fn vol(&self, id: u64) -> Result<&Vol> {
        self.vols.get(&id).ok_or(VolError::NotFound)
    }

    pub fn info(&self, id: u64) -> Result<VolumeInfo> {
        let r = &self.vol(id)?.rec;
        let mut label = String::new();
        label.try_reserve_exact(r.label.len()).map_err(|_| VolError::NoMemory)?;
        label.push_str(&r.label);
        Ok(VolumeInfo {
            id: r.id,
            origin: r.origin,
            label,
            size: r.size,
            snapshot: r.is_snapshot(),
            read_only: r.read_only(),
            auto: r.auto(),
            created_ns: r.created_ns,
            rollback_pending: r.rollback_pending(),
        })
    }

    pub fn list(&self) -> Result<Vec<VolumeInfo>> {
        let mut out = Vec::new();
        out.try_reserve_exact(self.vols.len()).map_err(|_| VolError::NoMemory)?;
        for &id in self.vols.keys() {
            out.push(self.info(id)?);
        }
        Ok(out)
    }

    /// Whether volume or snapshot `id` is open as a device (it cannot be deleted or rolled back then). A device
    /// closed a moment ago counts until the next `PoolHandle::lock` applies its release.
    pub fn is_open(&self, id: u64) -> bool {
        self.busy.contains_key(&id)
    }

    /// The newest snapshot taken of volume `id` (by creation time, then id), if any (VS-2).
    pub fn latest_snapshot(&self, id: u64) -> Option<u64> {
        self.vols
            .values()
            .filter(|v| v.rec.is_snapshot() && v.rec.origin == id)
            .max_by_key(|v| (v.rec.created_ns, v.rec.id))
            .map(|v| v.rec.id)
    }

    /// Whether volume `id` was written or discarded since its snapshot `snap` (VS-2's "nothing changed, no new
    /// automatic snapshot"). Exact: the snapshot shares the volume's root page, so the first change after it
    /// shadows the root (copy-on-write of a shared page), and nothing else moves a root.
    pub fn changed_since(&self, id: u64, snap: u64) -> Result<bool> {
        let (v, s) = (&self.vol(id)?.rec, &self.vol(snap)?.rec);
        if !s.is_snapshot() || s.origin != id {
            return Err(VolError::InvalidArgument);
        }
        Ok(v.root != s.root || v.depth != s.depth || v.size != s.size)
    }

    /// Chunks that deleting this volume or snapshot would free.
    pub fn exclusive_chunks(&mut self, id: u64) -> Result<u64> {
        let r = &self.vol(id)?.rec;
        let (root, depth) = (r.root, r.depth);
        if root == 0 { Ok(0) } else { self.exclusive(root, below(depth)?) }
    }

    /// `exclusive_chunks` in steps, for a caller that lets go of the pool lock in between (a list of hundreds
    /// of snapshots must not hold up the volume's I/O for a whole walk): counts the exclusive chunks of keys
    /// from `from` on, reading at most `budget` tree pages (at least one root-to-leaf path, so every step gets
    /// on), and returns where to go on (`None`: done). The volume may change between steps, so the sum is the
    /// size as of about then.
    pub fn exclusive_chunks_step(&mut self, id: u64, from: u64, budget: u64) -> Result<(u64, Option<u64>)> {
        let r = &self.vol(id)?.rec;
        let (root, depth) = (r.root, r.depth);
        if root == 0 {
            return Ok((0, None));
        }
        let mut left = budget.max(depth as u64 + 1);
        let mut n = 0;
        let next = self.exclusive_from(root, below(depth)?, 0, from, &mut left, &mut n)?;
        Ok((n, next))
    }

    /// Gives the record a page of this transaction (and the table its new place) before anything changes, so a
    /// commit never needs to allocate.
    fn mark_dirty(&mut self, id: u64) -> Result<()> {
        let old = self.vol(id)?.rec_page;
        if old == 0 || !self.is_dirty(old) {
            let p = self.alloc_page(KIND_RECORD, 0, Tree::Table)?;
            if let Err(e) = self.set_table(id, (p, 0)) {
                self.page_dec(p)?;
                return Err(e);
            }
            if old != 0 {
                self.page_dec(old)?;
            }
            self.vols.get_mut(&id).ok_or(VolError::NotFound)?.rec_page = p;
        }
        self.vols.get_mut(&id).ok_or(VolError::NotFound)?.dirty = true;
        Ok(())
    }

    fn set_table(&mut self, id: u64, v: (u64, u64)) -> Result<(u64, u64)> {
        let mut t = (self.table_root, self.table_depth);
        let r = self.tree_set(&mut t, Tree::Table, id, v);
        (self.table_root, self.table_depth) = t;
        self.table_dirty = true;
        r
    }

    /// The volume's record must be dirty already (its root may change).
    fn set_map(&mut self, id: u64, vc: u64, v: (u64, u64)) -> Result<(u64, u64)> {
        let r = &self.vol(id)?.rec;
        let mut t = (r.root, r.depth);
        let res = self.tree_set(&mut t, Tree::Map, vc, v);
        let r = &mut self.vols.get_mut(&id).ok_or(VolError::NotFound)?.rec;
        (r.root, r.depth) = t;
        res
    }

    /// Runs `f` again after making room each time it reports no space.
    fn with_room<T>(&mut self, mut f: impl FnMut(&mut Pool) -> Result<T>) -> Result<T> {
        let mut committed = false;
        loop {
            match f(self) {
                Err(VolError::NoSpace) => {
                    self.make_room(!committed)?;
                    committed = true;
                }
                r => return r,
            }
        }
    }

    /// Frees space: a commit releases what earlier changes freed (once per operation: a second one would only
    /// release what the failed attempt itself shadowed, and the next attempt shadows it again), then the
    /// oldest automatic snapshot goes.
    fn make_room(&mut self, try_commit: bool) -> Result<()> {
        if try_commit && self.has_pending() {
            return self.commit();
        }
        let victim = self
            .vols
            .values()
            .filter(|v| v.rec.auto() && !v.rec.rollback_pending() && !self.busy.contains_key(&v.rec.id))
            .min_by_key(|v| (v.rec.created_ns, v.rec.id))
            .map(|v| (v.rec.id, v.rec.created_ns));
        let Some((id, created)) = victim else {
            klog::event!(Warn, "bitsvol", "nospace", free = self.free, reserve = self.reserve, pending = 0u64);
            return Err(VolError::NoSpace);
        };
        self.delete_inner(id)?;
        self.dropped += 1;
        note!(self.cfg.quiet, "snapshot.dropped_for_space", id = id, created_ns = created, free = self.free);
        self.commit()
    }

    fn check_label(label: &str) -> Result<()> {
        if label.len() > LABEL_BOUND { Err(VolError::InvalidArgument) } else { Ok(()) }
    }

    fn new_record(&mut self, rec: Record) -> Result<u64> {
        let id = rec.id;
        self.vols.insert(id, Vol { rec, rec_page: 0, dirty: true });
        if let Err(e) = self.mark_dirty(id) {
            self.vols.remove(&id);
            return Err(e);
        }
        self.next_id += 1;
        Ok(id)
    }

    /// A new empty volume of `size` bytes (a multiple of the block size); durable when this returns.
    pub fn create_volume(&mut self, label: &str, size: u64, created_ns: u64) -> Result<u64> {
        Self::check_label(label)?;
        if size == 0 || size % self.bs != 0 {
            return Err(VolError::InvalidArgument);
        }
        let id = self.with_room(|p| {
            let rec = Record { id: p.next_id, origin: 0, flags: 0, depth: 0, size, root: 0, snap_time: p.time, created_ns, label: label.into() };
            p.new_record(rec)
        })?;
        self.commit()?;
        note!(self.cfg.quiet, "volume.create", id = id, size = size);
        Ok(id)
    }

    /// A read-only snapshot of `src` (`auto`: may be dropped when space runs out); durable when this returns.
    pub fn snapshot(&mut self, src: u64, label: &str, created_ns: u64, auto: bool) -> Result<u64> {
        let id = self.snapshot_uncommitted(src, label, created_ns, auto)?;
        self.commit()?;
        Ok(id)
    }

    /// `snapshot` without its commit (VS-2b: taken while the file system is frozen, made durable after the
    /// thaw): it holds `src` as of now at once (every later write to `src` copies its chunk first) and becomes
    /// durable with the next commit; a power cut before that loses it and leaves `src` as of the last commit.
    pub fn snapshot_uncommitted(&mut self, src: u64, label: &str, created_ns: u64, auto: bool) -> Result<u64> {
        Self::check_label(label)?;
        let id = self.with_room(|p| p.snapshot_inner(src, label, created_ns, auto))?;
        note!(self.cfg.quiet, "snapshot.create", id = id, origin = src, auto = auto as u64);
        Ok(id)
    }

    /// A copy window on volume `id` (VS-2b): until `end_copy_window`, no write to `id` lands in a chunk a commit
    /// already holds; each such chunk is copied first (the volume's snapshot time moves on as for a snapshot,
    /// without one, and again after every commit in the window). A power cut then leaves `id` exactly as of the
    /// last commit and every commit holds a prefix of its writes, so the window's flushes may wait for the
    /// commit that ends it (`PoolHandle::window`).
    pub fn begin_copy_window(&mut self, id: u64) -> Result<()> {
        self.vol(id)?;
        self.window = Some(id);
        self.window_step();
        Ok(())
    }

    pub fn end_copy_window(&mut self) {
        self.window = None;
    }

    /// The volume of the open copy window.
    pub fn copy_window(&self) -> Option<u64> {
        self.window
    }

    /// Chunks mapped up to now are older than the volume's snapshot time: the next write to each copies it.
    /// Only in memory: the record is written when the volume changes next (it does with the first copy), and a
    /// time on disk that lags only lets chunks no snapshot shares be written in place after a reopen.
    pub(crate) fn window_step(&mut self) {
        if let Some(id) = self.window {
            self.time += 1;
            let t = self.time;
            if let Some(v) = self.vols.get_mut(&id) {
                v.rec.snap_time = t;
            }
        }
    }

    fn snapshot_inner(&mut self, src: u64, label: &str, created_ns: u64, auto: bool) -> Result<u64> {
        let s = self.vol(src)?.rec.clone();
        self.mark_dirty(src)?;
        let flags = FLAG_SNAPSHOT | FLAG_READ_ONLY | if auto { FLAG_AUTO } else { 0 };
        let time = self.time + 1;
        let rec = Record { id: self.next_id, origin: src, flags, depth: s.depth, size: s.size, root: s.root, snap_time: time, created_ns, label: label.into() };
        let id = self.new_record(rec)?;
        if s.root != 0 {
            self.page_inc(s.root)?;
        }
        self.time = time;
        self.vols.get_mut(&src).ok_or(VolError::NotFound)?.rec.snap_time = time;
        Ok(id)
    }

    /// Deletes a volume or snapshot; its chunks are free after the commit this makes.
    pub fn delete(&mut self, id: u64) -> Result<()> {
        if self.busy.contains_key(&id) || self.vol(id)?.rec.rollback_pending() {
            return Err(VolError::Busy);
        }
        self.delete_inner(id)?;
        self.commit()?;
        note!(self.cfg.quiet, "volume.delete", id = id);
        Ok(())
    }

    fn delete_inner(&mut self, id: u64) -> Result<()> {
        let urgent = core::mem::replace(&mut self.urgent, true);
        let r = self.delete_tree_and_record(id);
        self.urgent = urgent;
        r
    }

    fn delete_tree_and_record(&mut self, id: u64) -> Result<()> {
        let (rec_page, root, depth) = {
            let v = self.vol(id)?;
            (v.rec_page, v.rec.root, v.rec.depth)
        };
        if rec_page != 0 {
            let old = self.set_table(id, (0, 0))?;
            if old.0 != rec_page {
                return Err(VolError::Corrupt("volume table entry"));
            }
            self.page_dec(rec_page)?;
        }
        // The record goes first: a drop that fails half way then only leaks space until the next open
        // (which rebuilds the counts from what is reachable), while a record left pointing at a half-dropped
        // tree would name chunks that get reused.
        self.vols.remove(&id);
        self.table_dirty = true;
        if root != 0 {
            if let Err(e) = below(depth).and_then(|l| self.drop_tree(root, l, Tree::Map)) {
                self.leaks += 1;
                klog::event!(Error, "bitsvol", "delete.leak", id = id, error = alloc::format!("{e:?}"));
            }
        }
        Ok(())
    }

    /// Rolls volume `id` back to snapshot `snap`. The state before is kept as a new snapshot (returned), so
    /// the rollback itself can be undone; everything happens in one commit.
    pub fn rollback(&mut self, id: u64, snap: u64, label: &str, created_ns: u64) -> Result<u64> {
        let now = self.rollback_switch(id, snap, label, created_ns)?;
        self.commit()?;
        note!(self.cfg.quiet, "volume.rollback", id = id, to = snap, kept_as = now);
        Ok(now)
    }

    /// `rollback` in memory, without its commit: on an error before the root switch the volume is as it was.
    fn rollback_switch(&mut self, id: u64, snap: u64, label: &str, created_ns: u64) -> Result<u64> {
        Self::check_label(label)?;
        if self.busy.contains_key(&id) {
            return Err(VolError::Busy);
        }
        if self.vol(id)?.rec.read_only() || !self.vol(snap)?.rec.is_snapshot() {
            return Err(VolError::InvalidArgument);
        }
        // The target must not be the automatic snapshot that making room drops.
        *self.busy.entry(snap).or_insert(0) += 1;
        let now = self.with_room(|p| {
            p.mark_dirty(snap)?;
            p.snapshot_inner(id, label, created_ns, false)
        });
        if let Some(n) = self.busy.get_mut(&snap) {
            *n -= 1;
            if *n == 0 {
                self.busy.remove(&snap);
            }
        }
        let now = now?;
        let s = self.vol(snap)?.rec.clone();
        let old = self.vol(id)?.rec.clone();
        if s.root != 0 {
            self.page_inc(s.root)?;
        }
        if old.root != 0 {
            self.drop_tree(old.root, below(old.depth)?, Tree::Map)?;
        }
        self.time += 1;
        let time = self.time;
        let v = &mut self.vols.get_mut(&id).ok_or(VolError::NotFound)?.rec;
        (v.root, v.depth, v.size, v.snap_time) = (s.root, s.depth, s.size, time);
        self.vols.get_mut(&snap).ok_or(VolError::NotFound)?.rec.snap_time = time;
        Ok(now)
    }

    /// The snapshot volume `id` goes back to at its next open (VS-3), if one is scheduled.
    pub fn pending_rollback(&self, id: u64) -> Option<u64> {
        self.vols.values().find(|v| v.rec.origin == id && v.rec.is_snapshot() && v.rec.rollback_pending()).map(|v| v.rec.id)
    }

    /// Schedules a rollback of volume `id` to its snapshot `snap` for the volume's next open (the volume is in
    /// use now: a mounted file system's caches would not see a root switch). Replaces an earlier schedule;
    /// durable when this returns. Returns the snapshot scheduled before, if any.
    pub fn schedule_rollback(&mut self, id: u64, snap: u64) -> Result<Option<u64>> {
        let (v, s) = (self.vol(id)?.rec.clone(), self.vol(snap)?.rec.clone());
        if v.is_snapshot() || v.read_only() || !s.is_snapshot() || s.origin != id {
            return Err(VolError::InvalidArgument);
        }
        let old = self.pending_rollback(id);
        if old == Some(snap) {
            return Ok(old);
        }
        let r = self.set_rollback_flag(snap, true).and_then(|_| old.map_or(Ok(()), |o| self.set_rollback_flag(o, false))).and_then(|_| self.commit());
        if let Err(e) = r {
            // Not durable: memory says what the disk says.
            self.flag_in_memory(snap, false);
            if let Some(o) = old {
                self.flag_in_memory(o, true);
            }
            return Err(e);
        }
        note!(self.cfg.quiet, "volume.rollback_scheduled", id = id, to = snap, replaced = old.unwrap_or(0));
        Ok(old)
    }

    /// Drops a scheduled rollback of volume `id`; returns the snapshot it named. Durable when this returns.
    pub fn cancel_rollback(&mut self, id: u64) -> Result<Option<u64>> {
        self.vol(id)?;
        let Some(s) = self.pending_rollback(id) else { return Ok(None) };
        if let Err(e) = self.set_rollback_flag(s, false).and_then(|_| self.commit()) {
            self.flag_in_memory(s, true);
            return Err(e);
        }
        note!(self.cfg.quiet, "volume.rollback_cancelled", id = id, to = s);
        Ok(Some(s))
    }

    /// Carries out a scheduled rollback of volume `id` (`rollback`: the state before is kept as the snapshot
    /// `label`, the schedule is cleared in the same commit). `Ok(None)`: nothing scheduled. On an error the
    /// schedule stays.
    pub fn apply_pending_rollback(&mut self, id: u64, label: &str, created_ns: u64) -> Result<Option<(u64, u64)>> {
        let Some(s) = self.pending_rollback(id) else { return Ok(None) };
        self.flag_in_memory(s, false);
        let kept = match self.rollback_switch(id, s, label, created_ns) {
            Ok(kept) => kept,
            Err(e) => {
                self.flag_in_memory(s, true);
                return Err(e);
            }
        };
        // Switched in memory, which is what the file system above reads from now on. A failed commit leaves
        // it as every other change of the pool: durable at the next commit that succeeds; until then the disk
        // still says "scheduled", so a start before that rolls back again from the same on-disk state.
        if let Err(e) = self.commit() {
            klog::event!(Warn, "bitsvol", "volume.rollback_not_durable_yet", id = id, to = s, error = alloc::format!("{e:?}"); "durable at the next commit");
        }
        note!(self.cfg.quiet, "volume.rollback", id = id, to = s, kept_as = kept);
        Ok(Some((s, kept)))
    }

    fn flag_in_memory(&mut self, id: u64, on: bool) {
        if let Some(v) = self.vols.get_mut(&id) {
            v.rec.flags = if on { v.rec.flags | FLAG_ROLLBACK } else { v.rec.flags & !FLAG_ROLLBACK };
        }
    }

    fn set_rollback_flag(&mut self, id: u64, on: bool) -> Result<()> {
        self.mark_dirty(id)?;
        self.flag_in_memory(id, on);
        Ok(())
    }

    /// Byte range of a request, checked against the volume.
    fn range(&self, id: u64, lba: u64, len: usize, write: bool) -> Result<(u64, u64)> {
        let r = &self.vol(id)?.rec;
        if write && r.read_only() {
            return Err(VolError::ReadOnly);
        }
        let off = lba.checked_mul(self.bs).ok_or(VolError::InvalidArgument)?;
        let end = off.checked_add(len as u64).ok_or(VolError::InvalidArgument)?;
        if len as u64 % self.bs != 0 || end > r.size {
            return Err(VolError::InvalidArgument);
        }
        Ok((off, end))
    }

    pub fn read(&mut self, id: u64, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<()> {
        let (mut off, _) = self.range(id, lba, buf.len(), false)?;
        let mut done = 0usize;
        while done < buf.len() {
            let (vc, at) = (off / self.cs, off % self.cs);
            let n = ((self.cs - at) as usize).min(buf.len() - done);
            let r = &self.vol(id)?.rec;
            let (p, _) = self.tree_get(r.root, r.depth, Tree::Map, vc)?;
            if p == 0 {
                buf[done..done + n].fill(0);
            } else {
                self.io_read(p * self.cs + at, &mut buf[done..done + n], prio)?;
            }
            done += n;
            off += n as u64;
        }
        Ok(())
    }

    /// `fua`: durable when this returns (commits).
    pub fn write(&mut self, id: u64, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<()> {
        let (mut off, _) = self.range(id, lba, buf.len(), true)?;
        let mut done = 0usize;
        while done < buf.len() {
            let (vc, at) = (off / self.cs, off % self.cs);
            let n = ((self.cs - at) as usize).min(buf.len() - done);
            let data = &buf[done..done + n];
            self.with_room(|p| p.write_piece(id, vc, at, data, prio))?;
            done += n;
            off += n as u64;
            if self.dirty_pages >= self.cfg.commit_dirty_pages {
                self.commit()?;
            }
        }
        if fua {
            self.commit()?;
        }
        Ok(())
    }

    fn write_piece(&mut self, id: u64, vc: u64, at: u64, data: &[u8], prio: IoPriority) -> Result<()> {
        let r = &self.vol(id)?.rec;
        let snap_time = r.snap_time;
        let (p, t) = self.tree_get(r.root, r.depth, Tree::Map, vc)?;
        if p != 0 && t >= snap_time {
            return self.io_write(p * self.cs + at, data, false, prio);
        }
        let q = self.alloc_chunk(self.reserve)?;
        if let Err(e) = self.fill_chunk(q, p, at, data, prio).and_then(|_| self.map_chunk(id, vc, q)) {
            self.unalloc_chunk(q)?;
            return Err(e);
        }
        if p != 0 {
            self.chunk_dec(p)?;
        }
        Ok(())
    }

    fn fill_chunk(&mut self, q: u64, p: u64, at: u64, data: &[u8], prio: IoPriority) -> Result<()> {
        if data.len() as u64 == self.cs {
            return self.io_write(q * self.cs, data, false, prio);
        }
        let mut b = zeroed(self.cs as usize)?;
        if p != 0 {
            self.io_read(p * self.cs, &mut b, prio)?;
        }
        b[at as usize..at as usize + data.len()].copy_from_slice(data);
        self.io_write(q * self.cs, &b, false, prio)
    }

    fn map_chunk(&mut self, id: u64, vc: u64, q: u64) -> Result<()> {
        self.mark_dirty(id)?;
        self.set_map(id, vc, (q, self.time))?;
        Ok(())
    }

    /// Unmaps every whole chunk inside the range; parts of chunks stay (Linux dm-thin does the same).
    pub fn discard(&mut self, id: u64, lba: u64, blocks: u64) -> Result<()> {
        let len = usize::try_from(blocks.checked_mul(self.bs).ok_or(VolError::InvalidArgument)?).map_err(|_| VolError::InvalidArgument)?;
        let (off, end) = self.range(id, lba, len, true)?;
        let (first, last) = (off.div_ceil(self.cs), end / self.cs);
        let mut from = first;
        let urgent = core::mem::replace(&mut self.urgent, true);
        let r = (|| {
            while from < last {
                let mut batch = Vec::new();
                let r = &self.vol(id)?.rec;
                let (root, depth) = (r.root, r.depth);
                self.tree_walk(root, depth, Tree::Map, from, WALK_BATCH, &mut batch)?;
                let Some(&(lastk, _)) = batch.last() else { break };
                for (vc, (p, _)) in batch.into_iter().filter(|&(k, _)| k < last) {
                    let old = self.with_room(|pool| {
                        pool.mark_dirty(id)?;
                        pool.set_map(id, vc, (0, 0))
                    })?;
                    if old.0 != p {
                        return Err(VolError::Corrupt("mapping changed during discard"));
                    }
                    self.chunk_dec(p)?;
                }
                from = lastk + 1;
            }
            Ok(())
        })();
        self.urgent = urgent;
        r
    }
}

/// The level below the root of a tree of `depth` levels (open rejects a root with depth 0).
fn below(depth: u32) -> Result<u32> {
    depth.checked_sub(1).ok_or(VolError::Corrupt("tree depth"))
}
