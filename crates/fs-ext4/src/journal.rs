//! The jbd2 journal of a volume (internal journal inode, linux-6.6/fs/jbd2): every metadata transaction
//! (`tx::Tx`) joins the running journal transaction in memory; a commit writes it to the log (descriptor
//! blocks with the logged copies, revoke blocks, then a commit block) and then to the blocks' home places.
//!
//! Commit order (`jbd2_journal_commit_transaction`, commit.c:347, ordered mode): the file data a transaction
//! points at was written before its mapping joined (vol_data `write_file`); log blocks; device flush (data and
//! log stable); commit block; device flush (the commit is stable: fsync may return); home writes
//! (checkpoint). The log tail moves only when the log is full (`tail_reset`: a flush makes the home writes
//! stable first) and at `empty` (sync_fs, freeze: `s_start` 0, nothing to replay).
//!
//! Blocks freed in the running transaction stay busy until it is committed (the allocator skips them,
//! mballoc's busy extents): a cut never finds a block that the last committed state still uses holding new
//! data. A freed block that was logged since the tail gets a revoke record, so replay does not write an old
//! logged copy over what the block holds later.
//!
//! FS-STALL: a commit's device work runs without the volume lock (jbd2's committing transaction). Under the lock
//! `begin_commit` moves the running transaction out (`Committing`; new changes join a new running one), the log and
//! home writes and both flushes run unlocked (`Plan::run`), and `end_commit` books the log under the lock again. One
//! commit is in flight at a time; reads see its blocks until it ended, the allocator keeps its freed blocks busy. A
//! holder of the lock that must commit itself (a full log, a sync) waits for the one in flight first and ends it
//! (`settle_commit`): the writer of the log never needs the lock for its I/O. A failed commit stays where it is
//! (`Journal::retry`): reads and the allocator keep seeing it, and the next commit logs it again, alone and before
//! the running transaction (no merge: nothing allocated on the failure path, each transaction keeps the size
//! `join` checked).

use crate::dev::Dev;
use crate::errno::*;
use crate::jbd2::{self, Csum, Jsb, Layout};
use crate::le::*;
use crate::recovery::Map;
use crate::sb;
use crate::state::State;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use block::prio::Lift;
use ksync::{SpinLock, WaitQueue};
use vfs::abi::Time;

klog::counter!(COMMITS, "ext4.journal.commits");
klog::counter!(LOGGED, "ext4.journal.blocks");
klog::counter!(TAIL_RESETS, "ext4.journal.tail_resets");
klog::counter!(pub REPLAYS, "ext4.journal.replays");
klog::counter!(TOO_BIG, "ext4.journal.too_big");

/// Journals replayed at mount so far (all volumes).
pub fn replays() -> u64 {
    REPLAYS.get()
}

/// Log tail moves so far (all volumes; tests and the Mini-PC check list).
pub fn tail_resets() -> u64 {
    TAIL_RESETS.get()
}

/// Journal commits so far (all volumes).
pub fn commits() -> u64 {
    COMMITS.get()
}

/// Transactions too big for the log, written around it so far (all volumes).
pub fn too_big() -> u64 {
    TOO_BIG.get()
}

/// The running transaction: the newest copy of every block it changed, its revokes and busy blocks.
#[derive(Default)]
pub struct Running {
    pub blocks: BTreeMap<u64, Vec<u8>>,
    pub revoked: BTreeSet<u64>,
    /// Freed blocks: start -> end (exclusive).
    pub busy: BTreeMap<u64, u64>,
}

impl Running {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.revoked.is_empty() && self.busy.is_empty()
    }
    /// Whether any block of [start, end) is busy.
    pub fn busy_in(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        busy_overlap(&self.busy, start, end)
    }
}

fn busy_overlap(busy: &BTreeMap<u64, u64>, start: u64, end: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
    busy.range(..end).filter(move |(_, e)| **e > start).map(|(s, e)| (*s, *e))
}

/// The transaction in flight to the log (see the module text): its blocks, revokes and busy blocks as they were
/// when it left `run`, and how its writing ended.
pub struct Committing {
    pub blocks: BTreeMap<u64, Vec<u8>>,
    pub revoked: BTreeSet<u64>,
    pub busy: BTreeMap<u64, u64>,
    tid: u32,
    /// Log blocks it takes.
    need: u64,
    end: SpinLock<Option<Result<Logged, Failed>>>,
    ended: WaitQueue,
}

/// A commit written: the log's new head, whether the tail moved first, the bytes it wrote.
#[derive(Clone, Copy)]
pub struct Logged {
    head: u32,
    reset: bool,
    bytes: u64,
    /// The log superblock was written with this start (the journal's copy follows it at `end_commit`).
    sb_start: Option<u32>,
}

/// A commit that failed: the errno, and whether the log was whole already (the home writes failed; `logged` then
/// says what the log took).
#[derive(Clone, Copy)]
pub struct Failed {
    errno: i64,
    home: bool,
    logged: Option<Logged>,
}

impl Committing {
    pub fn busy_in(&self, start: u64, end: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        busy_overlap(&self.busy, start, end)
    }
    pub fn tid(&self) -> u32 {
        self.tid
    }
    pub fn finish(&self, r: Result<Logged, Failed>) {
        *self.end.lock() = Some(r);
        self.ended.wake_all();
    }
    fn wait(&self) -> Result<Logged, Failed> {
        self.ended.wait_until(|| self.end.lock().is_some());
        self.outcome().unwrap_or(Err(Failed { errno: EIO, home: false, logged: None }))
    }
    fn outcome(&self) -> Option<Result<Logged, Failed>> {
        *self.end.lock()
    }
    /// How it ended, for a writer whose commit another holder of the lock ended (`settle_commit`).
    pub fn result(&self) -> Result<(), i64> {
        match self.outcome() {
            Some(Ok(_)) => Ok(()),
            Some(Err(f)) => Err(f.errno),
            None => Err(EIO),
        }
    }
}

/// Everything the log writer needs, taken under the lock (`begin_commit`), used without it (`run`).
pub struct Plan {
    pub tx: Arc<Committing>,
    /// The log has it already: only the home writes (`Journal::home_only`).
    home_only: bool,
    #[cfg(test)]
    home_nomem: alloc::sync::Arc<core::sync::atomic::AtomicU32>,
    dev: Arc<dyn Dev>,
    map: Map,
    lay: Layout,
    jsb: Jsb,
    block0: Vec<u8>,
    first: u32,
    end: u32,
    head: u32,
    /// The log superblock says the log is in use (else it goes with this commit).
    active: bool,
    /// The log is full: the tail moves to the head first (`tail_reset`).
    full: bool,
    now: Time,
    bs: u64,
    blocks: u64,
    first_data_block: u64,
}

pub struct Journal {
    pub map: Map,
    pub jsb: Jsb,
    pub lay: Layout,
    /// Journal block 0 as read (the superblock is its first 1024 bytes).
    block0: Vec<u8>,
    first: u32,
    end: u32,
    /// The next log block to write.
    head: u32,
    /// Log blocks in use since the tail.
    used: u32,
    /// The running transaction's id.
    pub tid: u32,
    /// The journal superblock on disk has `s_start` != 0.
    active: bool,
    /// Home blocks logged since the tail (they need a revoke when freed).
    logged: BTreeSet<u64>,
    pub run: Running,
    /// The running transaction is big enough to commit at the next operation's start.
    pub want_commit: bool,
    /// The transaction being written to the log, the volume lock released (FS-STALL); or, with `retry`, one whose
    /// commit failed and that goes to the log again before the running one (older than it).
    pub committing: Option<Arc<Committing>>,
    pub retry: bool,
    /// With `retry`: the transaction is committed in the log, only its home writes are still to do (they found no
    /// memory, round 2 item 2): its retry writes only those, and it counts as committed.
    pub home_only: bool,
    /// Host tests: the log counts as this many blocks (commit, tail and too-big paths on a small log
    /// without changing the on-disk wrap point).
    #[cfg(test)]
    pub test_cap: u32,
    /// Host tests: the home writes find no buffer this many more times.
    #[cfg(test)]
    pub test_home_nomem: alloc::sync::Arc<core::sync::atomic::AtomicU32>,
}

impl Journal {
    /// The journal of a mounted volume: `map` from the journal inode, `jsb` checked. `writable`: the
    /// features V2 writes are set now (written with the first journal superblock write): csum v3 on
    /// metadata_csum volumes, none otherwise, 64bit on 64bit volumes, revoke (ext4 `set_journal_csum_feature_set`,
    /// super.c:4078, and super.c:4912/4940).
    pub fn new(map: Map, mut jsb: Jsb, block0: Vec<u8>, fs_csum: bool, fs_64: bool, writable: bool, next_tid: u32) -> Journal {
        if writable {
            jsb.compat &= !jbd2::COMPAT_CHECKSUM;
            jsb.incompat &= !(jbd2::INCOMPAT_CSUM_V2 | jbd2::INCOMPAT_CSUM_V3 | jbd2::INCOMPAT_ASYNC_COMMIT);
            jsb.incompat |= jbd2::INCOMPAT_REVOKE;
            if fs_csum {
                jsb.incompat |= jbd2::INCOMPAT_CSUM_V3;
            }
            if fs_64 {
                jsb.incompat |= jbd2::INCOMPAT_64BIT;
            }
        }
        let lay = Layout { bs: jsb.bs as usize, csum: jsb.csum(), is64: jsb.is64(), seed: jsb.seed() };
        let (first, end) = (jsb.first, jsb.log_end());
        Journal {
            map,
            jsb,
            lay,
            block0,
            first,
            end,
            head: first,
            used: 0,
            tid: next_tid,
            active: false,
            logged: BTreeSet::new(),
            run: Running::default(),
            want_commit: false,
            committing: None,
            retry: false,
            home_only: false,
            #[cfg(test)]
            test_cap: u32::MAX,
            #[cfg(test)]
            test_home_nomem: alloc::sync::Arc::new(core::sync::atomic::AtomicU32::new(0)),
        }
    }
    fn capacity(&self) -> u32 {
        let c = self.end - self.first;
        #[cfg(test)]
        let c = c.min(self.test_cap);
        c
    }
    /// Log blocks a transaction of `blocks` blocks and `revokes` revokes takes (descriptors and commit too).
    fn need(&self, blocks: usize, revokes: usize) -> u64 {
        let tpd = self.lay.tags_per_desc().max(1);
        let rpb = self.lay.revokes_per_block().max(1);
        (blocks + blocks.div_ceil(tpd) + revokes.div_ceil(rpb) + 1) as u64
    }
    /// Whether transaction `tid` and every one before it are committed (none waits in `committing`, the running
    /// one is newer).
    pub fn committed(&self, tid: u32) -> bool {
        let after = |t: u32| t.wrapping_sub(tid) as i32 > 0;
        self.committing.as_ref().is_none_or(|c| after(c.tid) || (self.retry && self.home_only)) && after(self.tid)
    }
    /// A commit is being written now (not one waiting to be retried).
    pub fn in_flight(&self) -> bool {
        self.committing.is_some() && !self.retry
    }
    fn need_run(&self) -> u64 {
        self.need(self.run.blocks.len(), self.run.revoked.len())
    }
}

impl State {
    /// The running transaction is big (a quarter of the log), or the volume is about to run out of space while
    /// blocks freed in it (or in the one in flight) stay busy: then a change commits first, with the volume lock
    /// released (`Volume::lock_change`), instead of the allocator committing under it (`alloc.commit_for_busy`).
    pub fn journal_need_commit(&self) -> bool {
        let Some(j) = self.jnl.as_ref() else { return false };
        if j.want_commit {
            return true;
        }
        let busy: u64 = j.run.busy.iter().chain(j.committing.iter().flat_map(|c| c.busy.iter())).map(|(s, e)| e - s).sum();
        busy > 0 && self.free_blocks() < self.keep_for(None) + busy
    }

    /// Joins a finished transaction's blocks to the running journal transaction (see `tx` for the order
    /// FS-8a needed; inside one journal transaction the order does not matter).
    pub fn join(&mut self, blocks: Vec<(u64, Vec<u8>)>, freed: Vec<(u64, u64)>) -> Result<(), i64> {
        let Some(j) = self.jnl.as_ref() else { return Err(EINVAL) };
        let add_revokes: usize = freed.iter().map(|&(s, n)| j.logged.range(s..s + n).count()).sum();
        let total = j.need(j.run.blocks.len() + blocks.len(), j.run.revoked.len() + add_revokes);
        if total > j.capacity() as u64 {
            if j.need(blocks.len(), add_revokes) > j.capacity() as u64 {
                return self.write_around(blocks, freed).map_err(|e| self.failed(e));
            }
            // Rare: one operation bigger than three quarters of the log. Its earlier transactions are
            // committed apart from this one: a cut between them can leave blocks or an inode marked used that
            // nothing references (e2fsck's bitmap differences, never a wrong pointer). A file write is one
            // transaction (`write_file`) and never splits here.
            klog::event!(Info, "ext4", "journal.full_midop", blocks = blocks.len(), running = j.run.blocks.len());
            self.jcommit_inner()?;
        }
        let j = self.jnl.as_mut().ok_or(EINVAL)?;
        for (s, n) in &freed {
            let (s, e) = (*s, s + n);
            let mut gone: Vec<u64> = j.logged.range(s..e).copied().collect();
            gone.extend(j.run.blocks.range(s..e).map(|(b, _)| *b));
            for b in gone {
                j.run.blocks.remove(&b);
                if j.logged.contains(&b) {
                    j.run.revoked.insert(b);
                }
            }
            let end = j.run.busy.get(&s).copied().unwrap_or(0).max(e);
            j.run.busy.insert(s, end);
        }
        for (b, d) in blocks {
            if freed.iter().any(|&(s, n)| b >= s && b < s + n) || j.run.busy_in(b, b + 1).next().is_some() {
                continue;
            }
            j.run.blocks.insert(b, d);
        }
        if j.need_run() > (j.capacity() / 4) as u64 {
            j.want_commit = true;
        }
        Ok(())
    }

    /// A transaction too big for the log at all (Linux bounds every handle by its credits; V2's operations
    /// are not split yet, see the FS-8b notes): the journal is emptied and the blocks go to their home places
    /// directly in FS-8a's order, as a journal-less volume would. Before they are written the superblock
    /// records an error (`STATE_ERROR`) and keeps it until the next clean point (`make_clean`: syncfs,
    /// unmount, freeze): a cut in between makes the next mount read-only and e2fsck check the whole volume
    /// instead of trusting a half-applied change (one operation may need several such transactions).
    fn write_around(&mut self, blocks: Vec<(u64, Vec<u8>)>, freed: Vec<(u64, u64)>) -> Result<(), i64> {
        klog::event!(Warn, "ext4", "journal.too_big", blocks = blocks.len(); "a transaction larger than the log goes to its home blocks unjournaled, the volume marked for e2fsck until the next sync");
        TOO_BIG.inc();
        self.empty_journal()?;
        let state = g16(&self.sb, sb::STATE);
        if state & sb::STATE_ERROR == 0 {
            p16(&mut self.sb, sb::STATE, state | sb::STATE_ERROR);
            self.write_super()?;
            self.dev.flush()?;
        }
        // The superblock's block carries the transaction's counts: written last, with the mark.
        let sb_blk = sb::OFFSET / self.geo.bs;
        for (b, d) in &blocks {
            if *b != sb_blk {
                self.write_blocks(*b, d)?;
            }
        }
        let _ = freed;
        self.write_super()?;
        self.dev.flush()
    }

    /// Commits the running transaction under the volume lock (a full log, the allocator, a sync); the 5-second
    /// write-back and fsync commit without holding it (`Volume::commit_out`).
    pub fn jcommit(&mut self) -> Result<(), i64> {
        if self.jnl.as_ref().is_none_or(|j| j.run.is_empty() && j.committing.is_none()) {
            return Ok(());
        }
        if !self.writable {
            return Err(EROFS);
        }
        self.timeout_begin();
        let r = self.jcommit_inner();
        r.map_err(|e| self.commit_failed(e))
    }

    /// A timeout leaves the running transaction as it was: the next commit logs it again from the same head (a
    /// commit block the device may have taken lies over no home block yet, see `Plan::run`). So does a buffer the
    /// log writer could not get (FS-STALL: memory, not the disk); failed home writes stopped the volume already.
    pub fn commit_failed(&mut self, e: i64) -> i64 {
        if self.just_slow(e) || (e == ENOMEM && self.writable) {
            if e == ETIMEDOUT { EIO } else { e }
        } else {
            self.failed(e)
        }
    }

    fn jcommit_inner(&mut self) -> Result<(), i64> {
        self.settle_commit()?;
        // A failed one first, then the running one.
        for _ in 0..2 {
            let Some(plan) = self.begin_commit()? else { return Ok(()) };
            plan.tx.finish(plan.run(None));
            let tx = plan.tx.clone();
            drop(plan);
            self.end_commit(tx)?;
        }
        Ok(())
    }

    /// Waits for the commit in flight (its writer needs no lock for that) and ends it here.
    pub fn settle_commit(&mut self) -> Result<(), i64> {
        let Some(c) = self.jnl.as_ref().filter(|j| j.in_flight()).and_then(|j| j.committing.clone()) else { return Ok(()) };
        let _ = c.wait();
        self.end_commit(c)
    }

    /// Takes the transaction to log next (no commit may be in flight: `settle_commit`, or the volume's commit
    /// lock): one whose commit failed, else the running one. None: nothing to log.
    pub fn begin_commit(&mut self) -> Result<Option<Plan>, i64> {
        let (empty, logless, retry) = match self.jnl.as_ref() {
            None => return Ok(None),
            Some(j) => (j.run.is_empty(), j.run.blocks.is_empty() && j.run.revoked.is_empty(), j.retry),
        };
        if retry {
            let (dev, bs, blocks, first_data_block, now) = (self.dev.clone(), self.geo.bs, self.geo.blocks, self.geo.first_data_block, self.now());
            let j = self.jnl.as_mut().ok_or(EINVAL)?;
            let tx = j.committing.clone().ok_or(EINVAL)?;
            *tx.end.lock() = None;
            j.retry = false;
            let home_only = j.home_only;
            let full = j.used as u64 + tx.need > j.capacity() as u64;
            return Ok(Some(Plan { tx, home_only, #[cfg(test)] home_nomem: j.test_home_nomem.clone(), dev, map: j.map.clone(), lay: j.lay, jsb: j.jsb.clone(), block0: j.block0.clone(), first: j.first, end: j.end, head: j.head, active: j.active, full, now, bs, blocks, first_data_block }));
        }
        if empty || logless {
            // Only busy blocks (their freeing was written around the log): nothing to log.
            if let Some(j) = self.jnl.as_mut() {
                j.run = Running::default();
                j.want_commit = false;
            }
            return Ok(None);
        }
        self.ensure_dirty()?;
        let now = self.now();
        let (dev, bs, blocks, first_data_block) = (self.dev.clone(), self.geo.bs, self.geo.blocks, self.geo.first_data_block);
        let j = self.jnl.as_mut().ok_or(EINVAL)?;
        let need = j.need_run();
        let full = j.used as u64 + need > j.capacity() as u64;
        let run = core::mem::take(&mut j.run);
        // Logged from now on: a block of it freed while it is in flight gets its revoke (`join`).
        j.logged.extend(run.blocks.keys().copied());
        let tx = Arc::new(Committing { blocks: run.blocks, revoked: run.revoked, busy: run.busy, tid: j.tid, need, end: SpinLock::named("ext4.commit.end", None), ended: WaitQueue::new() });
        j.tid = j.tid.wrapping_add(1);
        j.want_commit = false;
        j.committing = Some(tx.clone());
        let home_only = false;
        Ok(Some(Plan { tx, home_only, #[cfg(test)] home_nomem: j.test_home_nomem.clone(), dev, map: j.map.clone(), lay: j.lay, jsb: j.jsb.clone(), block0: j.block0.clone(), first: j.first, end: j.end, head: j.head, active: j.active, full, now, bs, blocks, first_data_block }))
    }

    /// Books a written commit, or keeps a failed one for its retry (`Journal::retry`). Only for the commit in
    /// flight; the errno is the device's (the caller decides between a timeout and a stopped volume), except that
    /// failed home writes stop the volume here (the log holds it: logging it again could lose the only whole copy).
    pub fn end_commit(&mut self, tx: Arc<Committing>) -> Result<(), i64> {
        let Some(j) = self.jnl.as_mut() else { return tx.result() };
        if !j.in_flight() || !j.committing.as_ref().is_some_and(|c| Arc::ptr_eq(c, &tx)) {
            return tx.result();
        }
        let outcome = tx.outcome().unwrap_or(Err(Failed { errno: EIO, home: false, logged: None }));
        if j.home_only {
            return match outcome {
                Ok(l) => {
                    j.committing = None;
                    j.home_only = false;
                    self.written += l.bytes;
                    Ok(())
                }
                Err(f) if f.errno == ENOMEM => {
                    j.retry = true;
                    Ok(())
                }
                Err(f) => {
                    j.retry = true;
                    Err(self.failed(f.errno))
                }
            };
        }
        // The log took it, only the home writes found no memory: booked as committed (fsync may answer Ok), kept for
        // its home writes, the volume stays writable (round 2 item 2).
        let outcome = match outcome {
            Err(Failed { errno: ENOMEM, home: true, logged: Some(l) }) => {
                j.retry = true;
                j.home_only = true;
                Ok(l)
            }
            o => o,
        };
        match outcome {
            Ok(l) => {
                if !j.home_only {
                    j.committing = None;
                }
                if l.reset {
                    j.used = 0;
                    j.logged = tx.blocks.keys().copied().collect();
                }
                j.used += tx.need as u32;
                if let Some(start) = l.sb_start {
                    j.jsb.serialise(start, tx.tid, 0);
                }
                j.head = l.head;
                j.active = true;
                self.written += l.bytes;
                COMMITS.inc();
                LOGGED.add(tx.blocks.len() as u64);
                Ok(())
            }
            Err(f) => {
                j.retry = true;
                if f.home { Err(self.failed(f.errno)) } else { Err(f.errno) }
            }
        }
    }

    /// Commits, makes the home writes stable and marks the log empty (`jbd2_journal_flush` +
    /// `jbd2_mark_journal_empty`): nothing to replay afterwards.
    pub fn empty_journal(&mut self) -> Result<(), i64> {
        if self.jnl.is_none() {
            return Ok(());
        }
        self.jcommit_inner()?;
        let j = self.jnl.as_ref().ok_or(EINVAL)?;
        // Home writes still owed (no memory for them): the log is their only copy, it must not be emptied.
        if j.committing.is_some() {
            return Err(ENOMEM);
        }
        if !j.active {
            return Ok(());
        }
        self.dev.flush()?;
        let j = self.jnl.as_mut().ok_or(EINVAL)?;
        let (head, tid) = (j.head, j.tid);
        let s = j.jsb.serialise(0, tid, head);
        let mut b0 = j.block0.clone();
        b0[..jbd2::SB_SIZE].copy_from_slice(&s);
        let p0 = j.map.phys(0).ok_or(EUCLEAN)?;
        self.write_blocks(p0, &b0)?;
        self.dev.flush()?;
        let j = self.jnl.as_mut().ok_or(EINVAL)?;
        j.used = 0;
        j.logged.clear();
        j.active = false;
        Ok(())
    }

    /// The block holding the primary superblock, with the in-memory superblock in it.
    pub fn sb_block(&self) -> Result<(u64, Vec<u8>), i64> {
        let blk = sb::OFFSET / self.geo.bs;
        let off = (sb::OFFSET % self.geo.bs) as usize;
        let mut b = self.read(None, blk)?;
        b.get_mut(off..off + sb::SIZE).ok_or(EUCLEAN)?.copy_from_slice(&self.sb);
        Ok((blk, b))
    }
}

impl Plan {
    fn next(&self, j: u32) -> u32 {
        if j + 1 >= self.end { self.first } else { j + 1 }
    }

    /// The commit's device work, no volume lock held (`jbd2_journal_commit_transaction`, ordered mode): the data
    /// went to the device before its mapping joined; log blocks; flush; commit block; flush (fsync may return);
    /// home writes. `lift`: the commit lock's waiters, whose priority the requests take.
    pub fn run(&self, lift: Option<&Lift>) -> Result<Logged, Failed> {
        let log_err = |errno| Failed { errno, home: false, logged: None };
        let floor = || lift.map(|l| block::prio::floor(l.prio()));
        let tx = &*self.tx;
        let tid = tx.tid;
        let mut bytes = 0u64;
        if self.home_only {
            let _f = floor();
            let mut done = Logged { head: self.head, reset: false, bytes: 0, sb_start: None };
            self.home_writes(&mut done.bytes).map_err(|errno| Failed { errno, home: true, logged: None })?;
            return Ok(done);
        }
        if self.full {
            let _f = floor();
            self.tail_reset(tid, &mut bytes).map_err(log_err)?;
        }
        let tpd = self.lay.tags_per_desc().max(1);
        let mut log: Vec<(u32, Vec<u8>)> = Vec::new();
        log.try_reserve(tx.need as usize + 1).map_err(|_| log_err(ENOMEM))?;
        let start = self.head;
        let mut pos = self.head;
        // Every buffer of the write-back path is asked for fallibly: a full machine gets ENOMEM (the commit is
        // tried again), never the allocation panic (FS-STALL).
        let oom = |_| log_err(ENOMEM);
        let mut it = tx.blocks.iter().peekable();
        while it.peek().is_some() {
            let dpos = pos;
            pos = self.next(pos);
            let mut tags = Vec::new();
            tags.try_reserve_exact(tpd).map_err(oom)?;
            let mut copies = Vec::new();
            copies.try_reserve_exact(tpd).map_err(oom)?;
            for (b, d) in it.by_ref().take(tpd) {
                let esc = jbd2::gb32(d, 0) == jbd2::MAGIC;
                let mut c = Vec::new();
                c.try_reserve_exact(d.len()).map_err(oom)?;
                c.extend_from_slice(d);
                if esc {
                    c[..4].fill(0);
                }
                tags.push((*b, esc, self.lay.block_csum(tid, &c)));
                copies.push((pos, c));
                pos = self.next(pos);
            }
            log.push((dpos, self.lay.descriptor(tid, &self.jsb.uuid, &tags)));
            log.extend(copies);
        }
        let mut revoked: Vec<u64> = Vec::new();
        revoked.try_reserve_exact(tx.revoked.len()).map_err(oom)?;
        revoked.extend(tx.revoked.iter().copied());
        for rb in self.lay.revoke_blocks(tid, &revoked) {
            log.push((pos, rb));
            pos = self.next(pos);
        }
        if !self.active && !self.full {
            let s = self.jsb.clone().serialise(start, tid, 0);
            let mut b0 = self.block0.clone();
            b0[..jbd2::SB_SIZE].copy_from_slice(&s);
            log.push((0, b0));
        }
        let commit = (pos, self.lay.commit(tid, self.now.sec.max(0) as u64, self.now.nsec));
        pos = self.next(pos);
        {
            let _f = floor();
            self.write_log(&log, &mut bytes).map_err(log_err)?;
            self.dev.flush().map_err(log_err)?;
        }
        {
            let _f = floor();
            self.write_log(core::slice::from_ref(&commit), &mut bytes).map_err(log_err)?;
            self.dev.flush().map_err(log_err)?;
        }
        // Checkpoint: the home places, in runs of neighbouring blocks.
        let sb_start = if self.full { Some(self.head) } else if !self.active { Some(start) } else { None };
        let logged = Logged { head: pos, reset: self.full, bytes, sb_start };
        let _f = floor();
        let mut home_bytes = 0;
        match self.home_writes(&mut home_bytes) {
            Ok(()) => Ok(Logged { bytes: bytes + home_bytes, ..logged }),
            Err(errno) => Err(Failed { errno, home: true, logged: Some(Logged { bytes: bytes + home_bytes, ..logged }) }),
        }
    }

    /// The home writes; a buffer they could not get is waited for a little between tries (the heap takes page
    /// cache back meanwhile), never a busy loop (round 2 item 2). ENOMEM after the last try: the caller keeps the
    /// transaction for them (`Journal::home_only`). A device error ends them at once.
    fn home_writes(&self, bytes: &mut u64) -> Result<(), i64> {
        let mut tries = 0;
        loop {
            match self.write_home(&self.tx.blocks, bytes) {
                Err(ENOMEM) if tries + 1 < HOME_TRIES => {
                    tries += 1;
                    // Host tests have no clock to sleep on.
                    #[cfg(not(test))]
                    sched::sleep_ns(1_000_000 << tries.min(4));
                }
                r => return r,
            }
        }
    }

    fn write_blocks(&self, blk: u64, data: &[u8], bytes: &mut u64) -> Result<(), i64> {
        let n = (data.len() as u64).div_ceil(self.bs);
        if blk.checked_add(n).is_none_or(|e| e > self.blocks) || blk < self.first_data_block {
            return Err(EUCLEAN);
        }
        *bytes += data.len() as u64;
        self.dev.write(blk * self.bs, data)
    }

    /// Writes journal blocks (journal block number, contents) in runs that are contiguous on the volume.
    fn write_log(&self, log: &[(u32, Vec<u8>)], bytes: &mut u64) -> Result<(), i64> {
        let mut i = 0;
        while i < log.len() {
            let (jb, _) = log[i];
            let p = self.map.phys(jb).ok_or(EUCLEAN)?;
            let room = self.map.contiguous(jb).max(1) as usize;
            let mut n = 1;
            while i + n < log.len() && n < room && n < 256 && log[i + n].0 == jb + n as u32 {
                n += 1;
            }
            let mut buf = Vec::new();
            if buf.try_reserve_exact(n * self.bs as usize).is_err() {
                n = 1;
                buf.try_reserve_exact(self.bs as usize).map_err(|_| ENOMEM)?;
            }
            for (_, d) in &log[i..i + n] {
                buf.extend_from_slice(d);
            }
            self.write_blocks(p, &buf, bytes)?;
            i += n;
        }
        Ok(())
    }

    /// Writes blocks to their home places, neighbours in one request (up to 1 MiB).
    fn write_home(&self, blocks: &BTreeMap<u64, Vec<u8>>, bytes: &mut u64) -> Result<(), i64> {
        #[cfg(test)]
        if self.home_nomem.try_update(core::sync::atomic::Ordering::AcqRel, core::sync::atomic::Ordering::Acquire, |n| n.checked_sub(1)).is_ok() {
            return Err(ENOMEM);
        }
        let max = ((1usize << 20) / self.bs as usize).max(1);
        let mut it = blocks.iter().peekable();
        // A machine without a free MiB in one piece writes block by block (the home writes must not fail for it).
        let mut buf = Vec::new();
        let max = if buf.try_reserve_exact(max * self.bs as usize).is_ok() {
            max
        } else {
            buf.try_reserve_exact(self.bs as usize).map_err(|_| ENOMEM)?;
            1
        };
        while let Some((&b, d)) = it.next() {
            buf.clear();
            buf.extend_from_slice(d);
            let mut n = 1u64;
            while let Some(&(&nb, nd)) = it.peek() {
                if nb != b + n || n as usize >= max {
                    break;
                }
                buf.extend_from_slice(nd);
                n += 1;
                it.next();
            }
            self.write_blocks(b, &buf, bytes)?;
        }
        Ok(())
    }

    /// The log is full: everything checkpointed is made stable, then the tail moves to the head.
    fn tail_reset(&self, tid: u32, bytes: &mut u64) -> Result<(), i64> {
        self.dev.flush()?;
        let s = self.jsb.clone().serialise(self.head, tid, 0);
        let mut b0 = self.block0.clone();
        b0[..jbd2::SB_SIZE].copy_from_slice(&s);
        self.write_log(&[(0, b0)], bytes)?;
        self.dev.flush()?;
        TAIL_RESETS.inc();
        klog::event!(Info, "ext4", "journal.tail_reset", tid = tid, head = self.head; "log full: checkpointed blocks flushed, tail moved");
        Ok(())
    }
}

/// Tries of a commit's home writes that found no buffer before the volume stops.
const HOME_TRIES: u32 = 6;

/// `JBD2_MIN_JOURNAL_BLOCKS` (include/linux/jbd2.h:69).
const MIN_JOURNAL_BLOCKS: u32 = 1024;

/// Loads the internal journal of a volume (`ext4_get_journal_inode` + `jbd2_journal_load` without the
/// replay): Err(reason) when V2 cannot use it.
pub fn load(st: &State, writable: bool) -> Result<Result<(Journal, Jsb), &'static str>, i64> {
    let ino = g32(&st.sb, sb::JOURNAL_INUM);
    if ino == 0 {
        return Ok(Err("external journal device"));
    }
    let unusable = |e: i64| if e == ENOMEM { Err(e) } else { Ok(Err("journal inode or superblock unreadable")) };
    let di = match st.inode(None, ino) {
        Ok(d) => d,
        Err(e) => return unusable(e),
    };
    let bs = st.geo.bs;
    let blocks = di.size() / bs;
    if blocks < 8 || blocks > u32::MAX as u64 {
        return Ok(Err("journal inode size"));
    }
    let mut map = Map::default();
    let mut l = 0u64;
    while l < blocks {
        let m = match st.map_at(None, &di, l) {
            Ok(m) => m,
            Err(e) => return unusable(e),
        };
        match m {
            Ok((p, m, _)) => {
                let m = m.min(blocks - l).min(u32::MAX as u64);
                if p.checked_add(m).is_none_or(|e| e > st.geo.blocks) || p < st.geo.first_data_block {
                    return Ok(Err("journal blocks outside the volume"));
                }
                match map.runs.last_mut() {
                    Some(r) if r.1 + r.2 as u64 == p && r.0 as u64 + r.2 as u64 == l => r.2 += m as u32,
                    _ => {
                        map.runs.try_reserve(1).map_err(|_| ENOMEM)?;
                        map.runs.push((l as u32, p, m as u32));
                    }
                }
                l += m;
            }
            Err(_) => return Ok(Err("journal inode has holes")),
        }
    }
    let b0 = match st.read_block(map.phys(0).ok_or(EUCLEAN)?) {
        Ok(b) => b,
        Err(e) => return unusable(e),
    };
    let jsb = match Jsb::parse(&b0, bs as u32, blocks) {
        Ok(j) => j,
        Err(why) => return Ok(Err(why)),
    };
    // `journal_load_superblock` (journal.c:1392): at least JBD2_MIN_JOURNAL_BLOCKS, and room for the
    // fast-commit area when the journal has one.
    if jsb.maxlen < MIN_JOURNAL_BLOCKS || jsb.maxlen - MIN_JOURNAL_BLOCKS < jsb.maxlen - jsb.log_end() {
        return Ok(Err("journal too short"));
    }
    // A fast_commit volume mounts like any other (Linux too uses no fast commits while the journal lacks
    // the feature); a journal with the feature keeps its fast-commit area unused at the log's end
    // (`log_end`), but fast-commit blocks left to replay are beyond V2.
    if jsb.incompat & jbd2::INCOMPAT_FAST_COMMIT != 0 && jsb.start != 0 {
        return Ok(Err("fast-commit journal needs recovery"));
    }
    if jsb.compat & jbd2::COMPAT_CHECKSUM != 0 && jsb.csum() == Csum::None && jsb.start != 0 {
        return Ok(Err("journal with crc32 v1 transaction checksums needs recovery"));
    }
    if jsb.incompat & jbd2::INCOMPAT_ASYNC_COMMIT != 0 && jsb.start != 0 {
        return Ok(Err("async-commit journal needs recovery"));
    }
    let fs_csum = st.geo.csum.is_some();
    let next = jsb.sequence.wrapping_add(1);
    let disk = jsb.clone();
    let j = Journal::new(map, jsb, b0, fs_csum, st.geo.is64(), writable, next);
    Ok(Ok((j, disk)))
}
