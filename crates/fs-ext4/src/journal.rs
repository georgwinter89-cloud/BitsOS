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

use crate::errno::*;
use crate::jbd2::{self, Csum, Jsb, Layout};
use crate::le::*;
use crate::recovery::Map;
use crate::sb;
use crate::state::State;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

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
        self.busy.range(..end).filter(move |(_, e)| **e > start).map(|(s, e)| (*s, *e))
    }
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
    /// Host tests: the log counts as this many blocks (commit, tail and too-big paths on a small log
    /// without changing the on-disk wrap point).
    #[cfg(test)]
    pub test_cap: u32,
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
            #[cfg(test)]
            test_cap: u32::MAX,
        }
    }
    fn capacity(&self) -> u32 {
        let c = self.end - self.first;
        #[cfg(test)]
        let c = c.min(self.test_cap);
        c
    }
    fn next(&self, j: u32) -> u32 {
        if j + 1 >= self.end { self.first } else { j + 1 }
    }
    /// Log blocks a transaction of `blocks` blocks and `revokes` revokes takes (descriptors and commit too).
    fn need(&self, blocks: usize, revokes: usize) -> u64 {
        let tpd = self.lay.tags_per_desc().max(1);
        let rpb = self.lay.revokes_per_block().max(1);
        (blocks + blocks.div_ceil(tpd) + revokes.div_ceil(rpb) + 1) as u64
    }
    fn need_run(&self) -> u64 {
        self.need(self.run.blocks.len(), self.run.revoked.len())
    }
}

impl State {
    pub fn journal_need_commit(&self) -> bool {
        self.jnl.as_ref().is_some_and(|j| j.want_commit)
    }

    /// Joins a finished transaction's blocks to the running journal transaction (see `tx` for the order
    /// FS-8a needed; inside one journal transaction the order does not matter).
    pub fn join(&mut self, blocks: Vec<(u64, Vec<u8>)>, freed: Vec<(u64, u64)>) -> Result<(), i64> {
        let Some(j) = self.jnl.as_ref() else { return Err(EINVAL) };
        let add_revokes: usize = freed.iter().map(|&(s, n)| j.logged.range(s..s + n).count()).sum();
        let total = j.need(j.run.blocks.len() + blocks.len(), j.run.revoked.len() + add_revokes);
        if total > j.capacity() as u64 {
            if j.need(blocks.len(), add_revokes) > j.capacity() as u64 {
                return self.write_around(blocks, freed);
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

    /// Commits the running transaction (fsync, sync_fs, the 5-second write-back, a full log).
    pub fn jcommit(&mut self) -> Result<(), i64> {
        if self.jnl.as_ref().is_none_or(|j| j.run.is_empty()) {
            return Ok(());
        }
        if !self.writable {
            return Err(EROFS);
        }
        let r = self.jcommit_inner();
        r.map_err(|e| self.failed(e))
    }

    fn jcommit_inner(&mut self) -> Result<(), i64> {
        let (empty, logless, full) = match self.jnl.as_ref() {
            None => return Ok(()),
            Some(j) => (j.run.is_empty(), j.run.blocks.is_empty() && j.run.revoked.is_empty(), j.used as u64 + j.need_run() > j.capacity() as u64),
        };
        if empty {
            return Ok(());
        }
        if logless {
            // Only busy blocks (their freeing was written around the log): nothing to log.
            if let Some(j) = self.jnl.as_mut() {
                j.run = Running::default();
            }
            return Ok(());
        }
        self.ensure_dirty()?;
        if full {
            self.tail_reset()?;
        }
        let t = self.now();
        let j = self.jnl.as_mut().ok_or(EINVAL)?;
        let need = j.need_run();
        let tid = j.tid;
        let tpd = j.lay.tags_per_desc().max(1);
        let mut log: Vec<(u32, Vec<u8>)> = Vec::new();
        log.try_reserve(need as usize + 1).map_err(|_| ENOMEM)?;
        let start = j.head;
        let mut pos = j.head;
        let list: Vec<(&u64, &Vec<u8>)> = j.run.blocks.iter().collect();
        for chunk in list.chunks(tpd) {
            let dpos = pos;
            pos = j.next(pos);
            let mut tags = Vec::new();
            let mut copies = Vec::new();
            for (b, d) in chunk {
                let esc = jbd2::gb32(d, 0) == jbd2::MAGIC;
                let mut c = (*d).clone();
                if esc {
                    c[..4].fill(0);
                }
                tags.push((**b, esc, j.lay.block_csum(tid, &c)));
                copies.push((pos, c));
                pos = j.next(pos);
            }
            log.push((dpos, j.lay.descriptor(tid, &j.jsb.uuid, &tags)));
            log.extend(copies);
        }
        let revoked: Vec<u64> = j.run.revoked.iter().copied().collect();
        for rb in j.lay.revoke_blocks(tid, &revoked) {
            log.push((pos, rb));
            pos = j.next(pos);
        }
        if !j.active {
            let s = j.jsb.serialise(start, tid, 0);
            let mut b0 = j.block0.clone();
            b0[..jbd2::SB_SIZE].copy_from_slice(&s);
            log.push((0, b0));
        }
        let commit = (pos, j.lay.commit(tid, t.sec.max(0) as u64, t.nsec));
        pos = j.next(pos);
        let blocks = j.run.blocks.len();
        self.write_log(&log)?;
        self.dev.flush()?;
        self.write_log(core::slice::from_ref(&commit))?;
        self.dev.flush()?;
        // Checkpoint: the home places, in runs of neighbouring blocks.
        let done = core::mem::take(&mut self.jnl.as_mut().ok_or(EINVAL)?.run.blocks);
        if let Err(e) = self.write_home(&done) {
            // Committed in the log (the next mount replays it); reads keep seeing it meanwhile.
            if let Some(j) = self.jnl.as_mut() {
                j.run.blocks = done;
            }
            return Err(e);
        }
        let j = self.jnl.as_mut().ok_or(EINVAL)?;
        j.run = Running::default();
        j.logged.extend(done.keys().copied());
        j.used += need as u32;
        j.head = pos;
        j.tid = tid.wrapping_add(1);
        j.active = true;
        j.want_commit = false;
        COMMITS.inc();
        LOGGED.add(blocks as u64);
        Ok(())
    }

    /// Writes journal blocks (journal block number, contents) in runs that are contiguous on the volume.
    fn write_log(&mut self, log: &[(u32, Vec<u8>)]) -> Result<(), i64> {
        let mut i = 0;
        while i < log.len() {
            let j = self.jnl.as_ref().ok_or(EINVAL)?;
            let (jb, _) = log[i];
            let p = j.map.phys(jb).ok_or(EUCLEAN)?;
            let room = j.map.contiguous(jb).max(1) as usize;
            let mut n = 1;
            while i + n < log.len() && n < room && n < 256 && log[i + n].0 == jb + n as u32 {
                n += 1;
            }
            let mut buf = Vec::new();
            buf.try_reserve_exact(n * self.bs()).map_err(|_| ENOMEM)?;
            for (_, d) in &log[i..i + n] {
                buf.extend_from_slice(d);
            }
            self.write_blocks(p, &buf)?;
            i += n;
        }
        Ok(())
    }

    /// Writes blocks to their home places, neighbours in one request (up to 1 MiB).
    pub fn write_home(&mut self, blocks: &BTreeMap<u64, Vec<u8>>) -> Result<(), i64> {
        let max = ((1usize << 20) / self.bs()).max(1);
        let mut it = blocks.iter().peekable();
        let mut buf = Vec::new();
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
            self.write_blocks(b, &buf)?;
        }
        Ok(())
    }

    /// The log is full: everything checkpointed is made stable, then the tail moves to the head.
    fn tail_reset(&mut self) -> Result<(), i64> {
        self.dev.flush()?;
        let j = self.jnl.as_mut().ok_or(EINVAL)?;
        let (head, tid) = (j.head, j.tid);
        let s = j.jsb.serialise(head, tid, 0);
        let mut b0 = j.block0.clone();
        b0[..jbd2::SB_SIZE].copy_from_slice(&s);
        self.write_log(&[(0, b0)])?;
        self.dev.flush()?;
        let j = self.jnl.as_mut().ok_or(EINVAL)?;
        j.used = 0;
        j.logged.clear();
        j.active = true;
        TAIL_RESETS.inc();
        klog::event!(Info, "ext4", "journal.tail_reset", tid = tid, head = head; "log full: checkpointed blocks flushed, tail moved");
        Ok(())
    }

    /// Commits, makes the home writes stable and marks the log empty (`jbd2_journal_flush` +
    /// `jbd2_mark_journal_empty`): nothing to replay afterwards.
    pub fn empty_journal(&mut self) -> Result<(), i64> {
        if self.jnl.is_none() {
            return Ok(());
        }
        self.jcommit_inner()?;
        let j = self.jnl.as_ref().ok_or(EINVAL)?;
        if !j.active {
            return Ok(());
        }
        self.dev.flush()?;
        let j = self.jnl.as_mut().ok_or(EINVAL)?;
        let (head, tid) = (j.head, j.tid);
        let s = j.jsb.serialise(0, tid, head);
        let mut b0 = j.block0.clone();
        b0[..jbd2::SB_SIZE].copy_from_slice(&s);
        self.write_log(&[(0, b0)])?;
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
