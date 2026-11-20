//! BIGDIR: st_size and st_blocks of the directories fstat asked about, answered without `ext4.vol`.
//!
//! Written under `ext4.vol` only (an entry is made by the first stat that had to ask the volume, kept in step by
//! every write of the directory's inode, forgotten when a transaction that wrote it is not committed), read under
//! its own short lock.
//!
//! Memory: an entry is charged to the mount's pool before it is made (`vol_dirx::Dirs::stat_set`), and the table
//! has two ages so that it shrinks without a pass under its spin lock: an fstat moves its entry to the young age,
//! a hard call for memory (`age`) drops the old age and makes the young one old. What fstat asks about all the
//! time (Steam's folders) stays; a directory asked about once goes with the second such call after it. Under the
//! lock only tree nodes are made (a few hundred bytes each); a dropped age is freed after the lock is released.

use alloc::collections::BTreeMap;
use ksync::SpinLock;

/// What one entry costs (20 bytes in tree nodes that are two thirds full on average, and their parents).
pub const ENTRY_BYTES: u64 = 40;

type Map = BTreeMap<u32, (u64, u64)>;

#[derive(Default)]
struct Ages {
    young: Map,
    old: Map,
}

pub struct DirStats {
    ages: SpinLock<Ages>,
}

impl Default for DirStats {
    fn default() -> DirStats {
        DirStats { ages: SpinLock::named("ext4.dirstat", Ages::default()) }
    }
}

impl DirStats {
    pub fn get(&self, ino: u32) -> Option<(u64, u64)> {
        let mut a = self.ages.lock();
        if let Some(v) = a.young.get(&ino) {
            return Some(*v);
        }
        let v = a.old.remove(&ino)?;
        a.young.insert(ino, v);
        Some(v)
    }
    pub fn set(&self, ino: u32, v: (u64, u64)) {
        let mut a = self.ages.lock();
        a.old.remove(&ino);
        a.young.insert(ino, v);
    }
    /// The new numbers of a directory somebody asked about before.
    pub fn update(&self, ino: u32, v: (u64, u64)) {
        let mut a = self.ages.lock();
        let a = &mut *a;
        if let Some(e) = a.young.get_mut(&ino).or_else(|| a.old.get_mut(&ino)) {
            *e = v;
        }
    }
    pub fn forget(&self, ino: u32) {
        let mut a = self.ages.lock();
        a.young.remove(&ino);
        a.old.remove(&ino);
    }
    pub fn len(&self) -> usize {
        let a = self.ages.lock();
        a.young.len() + a.old.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Drops the entries no fstat asked for since the last call; returns how many.
    pub fn age(&self) -> usize {
        let dropped = {
            let mut a = self.ages.lock();
            let young = core::mem::take(&mut a.young);
            core::mem::replace(&mut a.old, young)
        };
        dropped.len()
    }
    pub fn clear(&self) -> usize {
        let old = core::mem::take(&mut *self.ages.lock());
        old.young.len() + old.old.len()
    }
}
