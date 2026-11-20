//! BIGDIR: what the volume remembers of one directory between its changes, so that a create, unlink or rename in
//! a folder of 50000 names goes to the block it changes instead of walking all 620 (Steam's download on the
//! Mini-PC, 12.11.2026: 2.5 MB copied out of the disk's cache under `ext4.vol` per change).
//!
//! `DirIndex` is memory only and exact while it exists: every name's place (block and offset of its entry) under
//! a keyed 32-bit hash of the name (a place found is always compared with the name on the block, so equal hashes
//! only cost a block), and per block the largest record that still fits, so an add finds a block with room
//! without reading the full ones. The disk format stays the linear ext4 directory it was. `vol_dirx` builds it by
//! one pass at a directory's first change, keeps it in step with every change, and forgets it when a transaction
//! that touched the directory is not committed, when its memory is refused or wanted back, and at unmount.
//!
//! Exact is what the code means it to be, not what a change relies on: no byte written to a directory block comes
//! from the index, and wherever the index and a block disagree the block is right (`Lost::Stale`: the index is
//! forgotten, the change walks the directory).

use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use vfs::pool::{MetaCharge, Pool};

/// SipHash-1-3 of `name` under the volume's key (made at mount from the kernel's random numbers: names cannot be
/// chosen to share a hash), folded to 32 bits.
pub fn hash(key: (u64, u64), name: &[u8]) -> u32 {
    let mut v = [key.0 ^ 0x736f6d6570736575, key.1 ^ 0x646f72616e646f6d, key.0 ^ 0x6c7967656e657261, key.1 ^ 0x7465646279746573];
    let round = |v: &mut [u64; 4]| {
        v[0] = v[0].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(13) ^ v[0];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(16) ^ v[2];
        v[0] = v[0].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(21) ^ v[0];
        v[2] = v[2].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(17) ^ v[2];
        v[2] = v[2].rotate_left(32);
    };
    let mut chunks = name.chunks_exact(8);
    for c in &mut chunks {
        let m = u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
        v[3] ^= m;
        round(&mut v);
        v[0] ^= m;
    }
    let mut last = (name.len() as u64) << 56;
    for (i, b) in chunks.remainder().iter().enumerate() {
        last |= (*b as u64) << (8 * i);
    }
    v[3] ^= last;
    round(&mut v);
    v[0] ^= last;
    v[2] ^= 0xff;
    for _ in 0..3 {
        round(&mut v);
    }
    let h = v[0] ^ v[1] ^ v[2] ^ v[3];
    (h ^ (h >> 32)) as u32
}

/// The smallest piece memory is charged in; larger ones are a quarter of what is charged, so a growing index
/// asks the pool a few dozen times in all. (A fixed 64 KiB piece per index took 640 MB of the system's commit for
/// 10000 changed small directories that really held 2 MB.)
const CHARGE_MIN: u64 = 1 << 10;
/// What an index costs before its first name: itself and its place in the volume's map, the first node of each
/// tree, the first piece of each list.
const FIXED_BYTES: u64 = 512;
/// What one name costs in the tree of places (12 bytes in nodes that are two thirds full on average).
const NAME_BYTES: u64 = 20;
/// The smallest record (`EXT4_DIR_REC_LEN(1)`): blocks with less room are not offered.
const MIN_REC: u16 = 12;

/// Why an index can no longer follow its directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lost {
    /// No memory for it: the heap, or the mount's pool refused the charge.
    Memory,
    /// It is not as the directory's blocks are (never expected; counted `ext4.dirx.stale`).
    Stale,
}

/// Memory charged to the mount's pool as its owner grows: what the owner holds, at most a quarter (or
/// `CHARGE_MIN`) ahead.
#[derive(Default)]
pub struct Charge {
    pieces: Vec<MetaCharge>,
    bytes: u64,
}

impl Charge {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    /// The bytes the list of pieces itself holds.
    fn own_bytes(&self) -> u64 {
        (self.pieces.capacity() * core::mem::size_of::<MetaCharge>()) as u64
    }
    /// Charges up to `need` bytes; false when the pool (or the heap, for the list of pieces) refuses.
    pub fn cover(&mut self, pool: &Arc<Pool>, need: u64) -> bool {
        while self.bytes < need {
            if self.pieces.try_reserve(1).is_err() {
                return false;
            }
            let step = (need - self.bytes).max(CHARGE_MIN).max(self.bytes / 4);
            match pool.charge_meta(step) {
                Ok(c) => {
                    self.bytes += step;
                    self.pieces.push(c);
                }
                Err(_) => return false,
            }
        }
        true
    }
    /// Gives everything back. The list keeps its room, so charging again right after asks the heap for nothing
    /// (the reclaim thread does that).
    pub fn clear(&mut self) {
        self.pieces.clear();
        self.bytes = 0;
    }
}

#[derive(Default)]
pub struct DirIndex {
    /// (hash, block, offset of the entry in the block).
    names: BTreeSet<(u32, u32, u16)>,
    /// Per block: the largest record that fits (0 for a full block and for an htree index block).
    room: Vec<u16>,
    /// (room, block) of every block with room for the smallest record.
    by_room: BTreeSet<(u16, u32)>,
    /// When a change last used it (seconds since boot): under memory pressure the unused ones go.
    pub used: u64,
    charge: Charge,
}

/// A directory of at most one block gets no index: the walk reads the same one block a change through the
/// index would, and most directories are that small (an install that makes 100000 of them holds nothing here).
/// The index starts when the directory grows its second block (`State::add_name`), or at the first change of a
/// larger directory after mount.
pub fn walked(blocks: u32) -> bool {
    blocks <= 1
}

impl DirIndex {
    /// Names besides `.` and `..`.
    pub fn live(&self) -> usize {
        self.names.len()
    }
    pub fn blocks(&self) -> u32 {
        self.room.len() as u32
    }
    /// Room for the per-block table of a directory of `n` blocks (false: no memory).
    pub fn reserve_blocks(&mut self, n: u32) -> bool {
        self.room.try_reserve(n as usize).is_ok()
    }
    /// The next place recorded under hash `h` at or after (`block`, `off`).
    pub fn place(&self, h: u32, block: u32, off: u16) -> Option<(u32, u16)> {
        self.names.range((h, block, off)..=(h, u32::MAX, u16::MAX)).next().map(|&(_, b, o)| (b, o))
    }
    /// A new name's place. `Stale`: the index had a name at that very place already.
    pub fn insert(&mut self, h: u32, block: u32, off: usize) -> Result<(), Lost> {
        if self.names.insert((h, block, off as u16)) { Ok(()) } else { Err(Lost::Stale) }
    }
    /// A removed name's place. `Stale`: the index did not have it.
    pub fn remove(&mut self, h: u32, block: u32, off: usize) -> Result<(), Lost> {
        if self.names.remove(&(h, block, off as u16)) { Ok(()) } else { Err(Lost::Stale) }
    }
    /// Block `block` (an existing one, or the next new one) now has room for a record of `room` bytes. `Stale`:
    /// the index knows fewer blocks than the directory has.
    pub fn set_room(&mut self, block: u32, room: usize) -> Result<(), Lost> {
        let room = room.min(u16::MAX as usize & !3) as u16;
        let i = block as usize;
        if i == self.room.len() {
            if self.room.try_reserve(1).is_err() {
                return Err(Lost::Memory);
            }
            self.room.push(0);
        }
        let old = self.room.get_mut(i).ok_or(Lost::Stale)?;
        if *old >= MIN_REC {
            self.by_room.remove(&(*old, block));
        }
        *old = room;
        if room >= MIN_REC {
            self.by_room.insert((room, block));
        }
        Ok(())
    }
    /// A block with room for a record of `need` bytes: the one with the least room that is enough, so blocks
    /// fill up and the directory grows only when nothing fits.
    pub fn fit(&self, need: usize) -> Option<u32> {
        let need = u16::try_from(need).ok()?;
        self.by_room.range((need, 0)..).next().map(|&(_, b)| b)
    }
    pub fn room_of(&self, block: u32) -> Option<u16> {
        self.room.get(block as usize).copied()
    }
    /// Every (hash, block, offset), for the tests' comparison with a fresh pass.
    pub fn places(&self) -> impl Iterator<Item = (u32, u32, u16)> + '_ {
        self.names.iter().copied()
    }
    pub fn rooms(&self) -> &[u16] {
        &self.room
    }
    /// The bytes it holds, roughly.
    pub fn bytes(&self) -> u64 {
        let lists = self.room.len() as u64 * 2 + self.charge.own_bytes();
        FIXED_BYTES + self.names.len() as u64 * NAME_BYTES + self.by_room.len() as u64 * 12 + lists
    }
    pub fn charged(&self) -> u64 {
        self.charge.bytes()
    }
    /// Charges what it grew by to `pool`. False when the pool refuses (the caller then forgets the index).
    pub fn charge(&mut self, pool: &Arc<Pool>) -> bool {
        // The list of the charge's pieces is part of what it holds and may grow while it is charged.
        while self.charge.bytes() < self.bytes() {
            let need = self.bytes();
            if !self.charge.cover(pool, need) {
                return false;
            }
        }
        true
    }
}

/// Tests: an index made wrong on purpose, as no code path makes it (`tests::bigdir_stale`).
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub enum Corrupt {
    /// The place of the name with this hash moves by four bytes.
    Offset(u32),
    /// The name with this hash is not in the index.
    Missing(u32),
    /// No name is in the index: it counts an empty directory.
    Empty,
    /// The last two blocks are not in the per-block table (one missing block would pass for a new one).
    ShortRooms,
}

#[cfg(test)]
impl DirIndex {
    /// False: there was nothing to make wrong that way.
    pub fn corrupt(&mut self, how: Corrupt) -> bool {
        let first = |ix: &DirIndex, h: u32| ix.names.range((h, 0, 0)..=(h, u32::MAX, u16::MAX)).next().copied();
        match how {
            Corrupt::Offset(h) => first(self, h).is_some_and(|p| self.names.remove(&p) && self.names.insert((p.0, p.1, p.2 + 4))),
            Corrupt::Missing(h) => first(self, h).is_some_and(|p| self.names.remove(&p)),
            Corrupt::Empty => {
                let had = !self.names.is_empty();
                self.names.clear();
                had
            }
            Corrupt::ShortRooms => {
                let had = self.room.len() >= 2;
                for _ in 0..2 {
                    if let Some(r) = self.room.pop() {
                        self.by_room.remove(&(r, self.room.len() as u32));
                    }
                }
                had
            }
        }
    }
}
