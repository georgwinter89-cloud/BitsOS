//! BIGDIR: the volume's side of the directory index (`dirx`): built by one pass at a directory's first change
//! after mount (`vol_dirx_find`), kept in step by `vol_dir` (every add, remove and new block), and forgotten
//! rather than repaired:
//!
//! - a transaction that is not committed (`State::abort`, a failed commit) forgets the index and the stat entry
//!   of every directory whose blocks or inode it changed (`Tx::dirs`), so memory is never ahead of the disk; the
//!   next change builds the index again. Changes fail before they touch a directory block in the usual cases
//!   (no room for a new block, a name that exists, a bad name), so those keep the index;
//! - an htree directory turned linear (`unindex`), a removed directory;
//! - an index that is not as its blocks are (`stale`: never expected; counted, logged, the change then walks the
//!   directory as before BIGDIR). Only that directory's index goes, and the directory is a suspect until its
//!   index has been built again: an add there looks at every block for the name first (`State::add_name`);
//! - memory: charged to the mount's pool as it grows (what it holds, `DirIndex::charge`), also while it is being
//!   built; a refused charge forgets the index (changes walk again), and `vfs::meta_shrink` takes back the ones
//!   no change used for a while.
//!
//! The index is built at the first change, not when the VFS loads the directory: most directories are only
//! read, and the pass (620 blocks for 50000 names, from the device's cache) is paid once per changed directory.
//! A directory of one block has none (`dirx::walked`): its index starts with its second block.
//!
//! The ages here (last use, the pause after a refusal) are seconds since boot, not the time of day: that clock
//! is set (the Mini-PC's after Windows ran, by an hour), and a pause that waits for it to catch up would send
//! every change back to the walk.

use crate::dirstat::{self, DirStats};
use crate::dirx::{self, Charge, DirIndex, Lost};
use crate::inode::DiskInode;
use crate::tx::Tx;
use crate::vol::Volume;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use core::cell::Cell;
use vfs::meta_shrink::{Level, MetaShrink};
use vfs::pool::Pool;

klog::counter!(FORGOTTEN, "ext4.dirx.forgotten");
klog::counter!(SHRUNK, "ext4.dirx.shrunk");
klog::counter!(REFUSED, "ext4.dirx.refused");
klog::counter!(STALE, "ext4.dirx.stale");
klog::counter!(SCANS, "ext4.dir.scans");
klog::counter!(BLOCK_READS, "ext4.dir.block_reads");
klog::counter!(STAT_MISSES, "ext4.dirstat.misses");
klog::counter!(STAT_REFUSED, "ext4.dirstat.refused");
klog::counter!(STAT_AGED, "ext4.dirstat.aged");

/// An index no change used for this long goes at a light call for memory, at a hard one after `HARD_IDLE_S`.
const IDLE_S: u64 = 60;
const HARD_IDLE_S: u64 = 2;
/// After the pool refused an index, no directory's index is built for this long: its changes walk, but not
/// each after a pass over the directory that ends refused again.
const REFUSED_PAUSE_S: u64 = 10;

/// What the volume remembers of its directories (under `ext4.vol`; `stats` has its own lock for fstat).
pub struct Dirs {
    pub(crate) map: BTreeMap<u32, DirIndex>,
    pub stats: Arc<DirStats>,
    /// What `stats` holds, charged (entries are made under `ext4.vol` only, so the charge lives here).
    stat_charge: Charge,
    /// The second of the last hard call that aged `stats`: several in one second are one.
    stat_aged: Option<u64>,
    /// The mount's pool (`glue::super_block`); None for a volume used without the VFS: nothing is charged.
    pub(crate) pool: Option<Arc<Pool>>,
    key: (u64, u64),
    /// The second until which no index is built (`REFUSED_PAUSE_S`).
    pub(crate) refused_until: u64,
    /// The last call for memory that found nothing to give: (second, idle seconds asked). The reclaim thread
    /// calls once per step; the same question within one second has the same answer without a pass.
    nothing_idle: Option<(u64, u64)>,
    /// Directories whose index was found stale in this mount and not built again since.
    suspects: BTreeSet<u32>,
    /// Tests: seconds added to the clock.
    pub skew_s: u64,
    /// Tests: no index, every change walks the directory as before BIGDIR.
    pub off: bool,
    /// Tests: the pool refuses a charge once an index holds more than this many bytes (0: every charge).
    pub refuse_above: Option<u64>,
    /// Tests: the bits of the hash that count (all; a few make many names share a hash).
    pub(crate) hash_mask: u32,
    /// Directory blocks read (by changes, listings and the index's pass) and written, and full walks by a change
    /// (the tests' bounds per change).
    pub block_reads: Cell<u64>,
    pub block_writes: Cell<u64>,
    pub scans: Cell<u64>,
    /// Indexes of this volume found stale (the counter `ext4.dirx.stale` is all volumes').
    pub stales: u64,
}

impl Default for Dirs {
    fn default() -> Dirs {
        let mut k = [0u8; 16];
        vfs::random::fill(&mut k);
        let half = |b: &[u8]| b.iter().fold(0u64, |a, &x| a << 8 | x as u64);
        Dirs {
            map: BTreeMap::new(),
            stats: Arc::new(DirStats::default()),
            stat_charge: Charge::default(),
            stat_aged: None,
            pool: None,
            key: (half(&k[..8]), half(&k[8..])),
            refused_until: 0,
            nothing_idle: None,
            suspects: BTreeSet::new(),
            skew_s: 0,
            off: false,
            refuse_above: None,
            hash_mask: u32::MAX,
            block_reads: Cell::new(0),
            block_writes: Cell::new(0),
            scans: Cell::new(0),
            stales: 0,
        }
    }
}

fn pool_covers(refuse_above: Option<u64>, pool: Option<&Arc<Pool>>, ix: &mut DirIndex) -> bool {
    if refuse_above.is_some_and(|b| ix.bytes() > b) {
        return false;
    }
    pool.is_none_or(|p| ix.charge(p))
}

impl Dirs {
    pub fn hash(&self, name: &[u8]) -> u32 {
        dirx::hash(self.key, name) & self.hash_mask
    }
    /// Seconds since boot (the monotonic clock; see the module docs).
    pub fn now_s(&self) -> u64 {
        vfs::poll::now() / 1_000_000_000 + self.skew_s
    }
    pub fn index(&self, ino: u32) -> Option<&DirIndex> {
        self.map.get(&ino)
    }
    pub fn read_counted(&self) {
        self.block_reads.set(self.block_reads.get() + 1);
        BLOCK_READS.inc();
    }
    pub fn scan_counted(&self) {
        self.scans.set(self.scans.get() + 1);
        SCANS.inc();
    }
    /// Nothing is remembered of directory `ino` any more (removed, turned linear).
    pub fn forget(&mut self, ino: u32) {
        self.map.remove(&ino);
        self.suspects.remove(&ino);
        self.stats.forget(ino);
    }
    /// A transaction that changed these directories was not committed.
    pub fn abort(&mut self, dirs: &[u32]) {
        for &ino in dirs {
            if self.map.remove(&ino).is_some() {
                FORGOTTEN.inc();
            }
            self.stats.forget(ino);
        }
    }
    /// Memory may be ahead of the disk everywhere (a failed write stopped the volume).
    pub fn clear(&mut self) {
        self.map.clear();
        self.stats.clear();
        self.stat_charge.clear();
    }
    /// The inode of directory `di` is written in `tx`: its numbers for fstat follow.
    pub fn inode_put(&self, tx: &mut Tx, di: &DiskInode, geo: &crate::sb::Geo) {
        tx.touch_dir(di.ino);
        self.stats.update(di.ino, (di.size(), di.blocks512(geo)));
    }
    /// Charges what `ix` grew by to the pool; false when the pool refuses.
    pub(crate) fn covers(&self, ix: &mut DirIndex) -> bool {
        pool_covers(self.refuse_above, self.pool.as_ref(), ix)
    }
    pub(crate) fn refused(&mut self) {
        REFUSED.inc();
        self.refused_until = self.now_s().saturating_add(REFUSED_PAUSE_S);
    }
    /// The index of `ino` is not as the directory's blocks are (never expected): counted, logged, forgotten. Only
    /// this index goes and nothing pauses: the next change builds it again from the blocks, and until then an add
    /// in this directory looks at every block for its name (`suspect`).
    pub fn stale(&mut self, ino: u32, block: u32) {
        STALE.inc();
        self.stales += 1;
        klog::event!(Warn, "ext4", "dirx.stale", dir = ino, block = block; "a directory's index did not match its blocks: forgotten, the change walks the directory");
        self.map.remove(&ino);
        self.suspects.insert(ino);
    }
    /// The index of `ino` was found stale and has not been built again.
    pub fn suspect(&self, ino: u32) -> bool {
        self.suspects.contains(&ino)
    }
    /// A fresh pass built the index of `ino`.
    pub(crate) fn built(&mut self, ino: u32, ix: DirIndex) {
        self.map.insert(ino, ix);
        self.suspects.remove(&ino);
    }
    /// Applies a change to the index of `ino` and charges what it grew by. An index that cannot follow is
    /// forgotten: for want of memory (changes walk for a while), or as stale.
    pub fn update(&mut self, ino: u32, block: u32, f: impl FnOnce(&mut DirIndex) -> Result<(), Lost>) {
        let Some(ix) = self.map.get_mut(&ino) else { return };
        let (refuse_above, pool) = (self.refuse_above, self.pool.as_ref());
        match f(ix).and_then(|()| if pool_covers(refuse_above, pool, ix) { Ok(()) } else { Err(Lost::Memory) }) {
            Ok(()) => {}
            Err(Lost::Memory) => {
                self.map.remove(&ino);
                self.refused();
            }
            Err(Lost::Stale) => self.stale(ino, block),
        }
    }
    /// Forgets the indexes no change used for `idle_s` seconds; returns their bytes. Called by the reclaim
    /// thread when memory is short: it asks for no memory itself (no list of the old ones, they all go anyway).
    pub fn shrink(&mut self, now_s: u64, idle_s: u64) -> u64 {
        if idle_s > 0 && self.nothing_idle.is_some_and(|(at, asked)| at == now_s && asked <= idle_s) {
            return 0;
        }
        let mut bytes = 0;
        self.map.retain(|_, ix| {
            let old = now_s.saturating_sub(ix.used) >= idle_s;
            if old {
                bytes += ix.charged().max(ix.bytes());
                SHRUNK.inc();
            }
            !old
        });
        self.nothing_idle = (bytes == 0).then_some((now_s, idle_s));
        bytes
    }
    /// The numbers of directory `ino` for fstat from now on. Their memory is charged first; when the pool refuses
    /// it they are not kept, and fstat asks the volume each time.
    pub fn stat_set(&mut self, ino: u32, v: (u64, u64)) {
        let need = (self.stats.len() as u64 + 1) * dirstat::ENTRY_BYTES;
        if !self.pool.as_ref().is_none_or(|p| self.stat_charge.cover(p, need)) {
            STAT_REFUSED.inc();
            return;
        }
        self.stats.set(ino, v);
    }
    /// Memory is short: the numbers no fstat asked for since the last such call go (`all`: every one). Returns
    /// the bytes given back (the charge's, as `shrink` counts an index). Asks the heap for nothing (the reclaim
    /// thread calls it).
    pub fn stat_shrink(&mut self, now_s: u64, all: bool) -> u64 {
        if !all && self.stat_aged.replace(now_s) == Some(now_s) {
            return 0;
        }
        let held = |d: &Dirs| d.stat_charge.bytes().max(d.stats.len() as u64 * dirstat::ENTRY_BYTES);
        let before = held(self);
        let gone = if all { self.stats.clear() } else { self.stats.age() };
        STAT_AGED.add(gone as u64);
        // Charged anew for what is left, so the pool gets back what went.
        self.stat_charge.clear();
        let need = self.stats.len() as u64 * dirstat::ENTRY_BYTES;
        if !self.pool.as_ref().is_none_or(|p| self.stat_charge.cover(p, need)) {
            self.stats.clear();
            self.stat_charge.clear();
        }
        before.saturating_sub(held(self))
    }
    /// Bytes charged for the fstat numbers.
    pub fn stat_charged(&self) -> u64 {
        self.stat_charge.bytes()
    }
}

impl Volume {
    /// The pool the directory indexes are charged to (the mount's, `glue::super_block`).
    pub fn set_meta_pool(&self, pool: Arc<Pool>) {
        self.lock().dirx.pool = Some(pool);
    }

    pub fn dir_stats(&self) -> Arc<DirStats> {
        self.lock().dirx.stats.clone()
    }

    /// (st_size, st_blocks) of directory `ino` from the volume, remembered for the next fstat (`DirStats`).
    pub fn dir_stat_slow(&self, ino: u32) -> Result<(u64, u64), i64> {
        STAT_MISSES.inc();
        // Once without the lock held over the device, then under it from memory (as `hold`): the entry is made
        // under the lock that every change of the inode holds.
        self.stat(ino)?;
        let mut st = self.lock();
        let d = st.inode(None, ino)?;
        let v = (d.size(), d.blocks512(&st.geo));
        if d.is_dir() {
            st.dirx.stat_set(ino, v);
        }
        Ok(v)
    }
}

impl MetaShrink for Volume {
    /// Never waits for the volume: a change that holds the lock is using its index right now.
    fn shrink_meta(&self, level: Level) -> u64 {
        let Some(mut st) = self.st.try_lock() else { return 0 };
        let now = st.dirx.now_s();
        match level {
            Level::Idle => st.dirx.shrink(now, IDLE_S),
            // `Hard` is the reclaim thread's thorough round (`vfs::reclaim::shrink`): a light one freed nothing.
            Level::Hard => st.dirx.shrink(now, HARD_IDLE_S) + st.dirx.stat_shrink(now, false),
            Level::All => st.dirx.shrink(now, 0) + st.dirx.stat_shrink(now, true),
        }
    }
}
