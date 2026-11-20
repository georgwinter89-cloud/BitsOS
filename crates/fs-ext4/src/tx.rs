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
    /// BIGDIR: the directories whose blocks or inode it changed: what memory knows of them beside the inode
    /// (`dirx`: the index, the numbers for fstat) is forgotten when it is not committed (`State::abort`).
    /// Emptied by `settle_dirs` only: a transaction dropped with directories in it trips `drop`.
    dirs: Vec<u32>,
}

impl Drop for Tx {
    /// A transaction that changed a directory ends in `State::commit` or `State::abort`: both take its
    /// directories (`settle_dirs`). One that is just dropped (an early return between `Tx::new` and the commit)
    /// would leave the index in memory ahead of the disk with nothing to say so.
    fn drop(&mut self) {
        // A test that already fails drops its transactions on the way out: its own message is the one to show.
        #[cfg(test)]
        if std::thread::panicking() {
            return;
        }
        debug_assert!(self.dirs.is_empty(), "a transaction that changed directories {:?} was dropped without commit or abort", self.dirs);
    }
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
    pub fn touch_dir(&mut self, ino: u32) {
        if !self.dirs.contains(&ino) {
            self.dirs.push(ino);
        }
    }
    /// The directories it changed, taken by whoever decides what memory keeps of them: the commit that wrote
    /// (or joined) it, or the abort that forgets their indexes.
    pub fn settle_dirs(&mut self) -> Vec<u32> {
        core::mem::take(&mut self.dirs)
    }
}
