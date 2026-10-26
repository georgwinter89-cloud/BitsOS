//! One change of the volume's metadata: every block it changes. On a volume with a journal (FS-8b) `commit`
//! joins it to the running jbd2 transaction (`journal`): all its blocks reach the disk together or not at
//! all. Without a journal (FS-8a) its blocks go to their home places at `commit`, in the order the change
//! needs them (like a journal-less ext4 on Linux, a cut leaves what e2fsck repairs): blocks marked `early`
//! (allocation bitmaps: a block is marked used before anything points to it), then the group descriptors
//! and the superblock, then the rest in the order they were first changed. Freeing happens in a later
//! transaction, after the one that removed the last pointer; the blocks it frees are noted (`freed`) so the
//! journal keeps them busy until that transaction is committed. Reads of a block the transaction has
//! changed see the change (`get`).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

#[derive(Default)]
pub struct Tx {
    early: Vec<(u64, Vec<u8>)>,
    late: Vec<(u64, Vec<u8>)>,
    /// block -> (early?, index)
    at: BTreeMap<u64, (bool, usize)>,
    /// Group descriptor table blocks and the superblock changed.
    pub gd_dirty: alloc::collections::BTreeSet<u64>,
    pub sb_dirty: bool,
    /// What the in-memory state was before this transaction changed it (`State::abort` puts it back).
    pub undo_gd: BTreeMap<u32, Vec<u8>>,
    pub undo_sb: Option<Vec<u8>>,
    pub undo_nodes: BTreeMap<u32, Vec<u8>>,
    /// Block ranges (first, count) this transaction frees.
    pub freed: Vec<(u64, u64)>,
}

impl Tx {
    pub fn new() -> Tx {
        Tx::default()
    }
    pub fn get(&self, blk: u64) -> Option<&[u8]> {
        let (e, i) = *self.at.get(&blk)?;
        let l = if e { &self.early } else { &self.late };
        l.get(i).map(|(_, d)| d.as_slice())
    }
    fn put_in(&mut self, blk: u64, data: Vec<u8>, early: bool) {
        if let Some(&(e, i)) = self.at.get(&blk) {
            let l = if e { &mut self.early } else { &mut self.late };
            if let Some(slot) = l.get_mut(i) {
                slot.1 = data;
            }
            return;
        }
        let l = if early { &mut self.early } else { &mut self.late };
        l.push((blk, data));
        self.at.insert(blk, (early, l.len() - 1));
    }
    pub fn put(&mut self, blk: u64, data: Vec<u8>) {
        self.put_in(blk, data, false)
    }
    pub fn put_early(&mut self, blk: u64, data: Vec<u8>) {
        self.put_in(blk, data, true)
    }
    pub fn is_empty(&self) -> bool {
        self.at.is_empty() && self.gd_dirty.is_empty() && !self.sb_dirty && self.freed.is_empty()
    }
    /// (early blocks, late blocks) for the commit.
    pub fn take(&mut self) -> (Vec<(u64, Vec<u8>)>, Vec<(u64, Vec<u8>)>) {
        self.at.clear();
        (core::mem::take(&mut self.early), core::mem::take(&mut self.late))
    }
    /// Only the late blocks (FS-8a's commit writes the superblock between early and late).
    pub fn take_late(&mut self) -> Vec<(u64, Vec<u8>)> {
        let late = core::mem::take(&mut self.late);
        self.at.retain(|_, (e, _)| *e);
        late
    }
    pub fn blocks(&self) -> usize {
        self.at.len()
    }
}
