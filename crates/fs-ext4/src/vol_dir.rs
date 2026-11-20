//! Directories on the volume: walking their blocks, listing (linear and htree directories alike: an htree's
//! index blocks hold no names), finding, adding and removing names. Adding to an htree directory first turns
//! it into a linear one (its index blocks become empty leaf blocks, the INDEX flag goes): Linux reads such a
//! directory as any linear one, and FS-8a does not insert into hash order (decision in the FS-8a notes).
//!
//! BIGDIR: a change does not walk the directory. `locate` and `add_name` go through the directory's index in
//! memory (`dirx`, `vol_dirx`) to the one block they change; every block written here is noted in the
//! transaction (`Tx::touch_dir`), so a transaction that is not committed forgets the index. `find` and the walk
//! in `add_name` are what is left for a directory without an index: one of a single block (`dirx::walked`: the
//! walk reads the block the index would), or one whose index was refused. The index only shows the way: every
//! block changed here is read and checked first, and the answers that remove something ("the directory is
//! empty", "the name is not there" before ENOENT) are read from the blocks (`is_empty_dir`, `locate_sure`).

use crate::dir::{self, DirEnt};
use crate::extent::{self, Edit, Found, Target};
use crate::inode::{DiskInode, FL_INDEX, FL_INLINE_DATA};
use crate::le::*;
use crate::state::State;
use crate::tx::Tx;
use alloc::vec::Vec;
use crate::errno::*;

/// Where a name sits: (logical block, its disk block, entry), and the block as it was read.
pub struct Slot {
    pub lblk: u32,
    pub pblk: u64,
    pub e: DirEnt,
    pub data: Vec<u8>,
}

impl State {
    /// The disk block of directory block `l` (a directory has no holes; one is EUCLEAN).
    pub fn dir_pblk(&self, tx: Option<&Tx>, di: &DiskInode, l: u32) -> Result<u64, i64> {
        if !extent::uses_extents(di) {
            return crate::blockmap::map(self, tx, di, l)?.ok_or(EUCLEAN);
        }
        match extent::lookup(self, tx, di, l)? {
            Found::Ext(e) if !e.unwritten => Ok(e.pblk + (l - e.lblk) as u64),
            _ => {
                klog::event!(Warn, "ext4", "dir.hole", ino = di.ino, block = l);
                Err(EUCLEAN)
            }
        }
    }

    pub fn dir_blocks(&self, di: &DiskInode) -> Result<u32, i64> {
        let n = di.size() / self.geo.bs;
        u32::try_from(n).map_err(|_| EUCLEAN)
    }

    /// A directory block, checked: its checksum (leaf tail), or recognised as an htree index block.
    /// Returns None for an index block.
    pub(crate) fn dir_block(&self, tx: Option<&Tx>, di: &DiskInode, l: u32) -> Result<Option<(u64, Vec<u8>)>, i64> {
        let p = self.dir_pblk(tx, di, l)?;
        let b = self.read(tx, p)?;
        self.dirx.read_counted();
        let bs = self.bs();
        if di.flags() & FL_INDEX != 0 && (l == 0 || dir::is_dx_node(&b, bs)) {
            return Ok(None);
        }
        if let Some(seed) = di.csum_seed(&self.geo)
            && !dir::csum_ok(&b, bs, seed)
        {
            klog::event!(Warn, "ext4", "dir.badcrc", ino = di.ino, block = l);
            return Err(EBADMSG);
        }
        Ok(Some((p, b)))
    }

    fn area(&self) -> usize {
        dir::area(self.bs(), self.geo.csum.is_some())
    }

    /// Every name of a directory except `.` and `..`: (name, inode, file type).
    pub fn list_dir(&self, di: &DiskInode) -> Result<Vec<(Vec<u8>, u32, u8)>, i64> {
        if di.flags() & FL_INLINE_DATA != 0 {
            return crate::inline::list(self, di);
        }
        let mut out = Vec::new();
        let n = self.dir_blocks(di)?;
        for l in 0..n {
            let Some((_, b)) = self.dir_block(None, di, l)? else { continue };
            for e in dir::walk(&b, self.area()) {
                let e = e?;
                let name = e.name(&b);
                if e.ino == 0 || name == b"." || name == b".." {
                    continue;
                }
                if e.ino > self.geo.inodes {
                    klog::event!(Warn, "ext4", "dir.bad_inode", dir = di.ino, ino = e.ino);
                    return Err(EUCLEAN);
                }
                out.push((name.to_vec(), e.ino, e.ftype));
            }
        }
        Ok(out)
    }

    /// `name` by a walk over the whole directory (a directory without an index; `locate` otherwise).
    pub fn find(&self, tx: Option<&Tx>, di: &DiskInode, name: &[u8]) -> Result<Option<Slot>, i64> {
        let n = self.dir_blocks(di)?;
        if !crate::dirx::walked(n) {
            self.dirx.scan_counted();
        }
        for l in 0..n {
            let Some((p, b)) = self.dir_block(tx, di, l)? else { continue };
            let mut at = None;
            for e in dir::walk(&b, self.area()) {
                let e = e?;
                if e.ino != 0 && e.name(&b) == name {
                    at = Some(e.owned(&b));
                    break;
                }
            }
            if let Some(e) = at {
                return Ok(Some(Slot { lblk: l, pblk: p, e, data: b }));
            }
        }
        Ok(None)
    }

    fn seal_dir(&self, di: &DiskInode, b: &mut [u8]) {
        dir::seal(b, self.bs(), di.csum_seed(&self.geo));
    }

    /// A changed directory block into `tx`, sealed; the transaction now knows it changed this directory.
    fn put_dir(&self, tx: &mut Tx, di: &DiskInode, p: u64, mut b: Vec<u8>) {
        self.seal_dir(di, &mut b);
        tx.touch_dir(di.ino);
        self.dirx.block_writes.set(self.dirx.block_writes.get() + 1);
        tx.put(p, b);
    }

    /// The block of `slot` as `tx` has it now: the transaction's own version when it changed the block after
    /// the slot was found (a rename that put the new name into the old one's block), else the slot's.
    fn slot_block(&self, tx: &Tx, slot: &Slot) -> Vec<u8> {
        match tx.get(slot.pblk) {
            Some(d) => d.to_vec(),
            None => slot.data.clone(),
        }
    }

    /// The slot's entry is still what it was found as (its place holds its name): EUCLEAN otherwise, before
    /// anything is written.
    fn slot_entry(&self, b: &[u8], slot: &Slot) -> Result<(), i64> {
        let same = dir::walk(b, self.area()).filter_map(Result::ok).any(|e| e.off == slot.e.off && e.ino != 0 && e.name(b) == slot.e.name.as_slice());
        if !same {
            klog::event!(Warn, "ext4", "dir.slot_moved", dir_block = slot.pblk, off = slot.e.off);
            return Err(EUCLEAN);
        }
        Ok(())
    }

    /// `ext4_add_entry` without hash order: a block with room (through the index: the fullest that still
    /// fits; without one: the first), else a new block at the end. EEXIST when the index, or the one block of
    /// a small directory, has the name (as Linux's search before the insert). `di` is the directory (changed in memory: size and blocks;
    /// the caller writes it). A new directory block leaves `keep` free blocks (`State::keep_for`).
    pub fn add_name(&mut self, tx: &mut Tx, di: &mut DiskInode, name: &[u8], ino: u32, ftype: u8, keep: u64) -> Result<(), i64> {
        if di.flags() & FL_INLINE_DATA != 0 || !extent::uses_extents(di) {
            return Err(EROFS);
        }
        if di.flags() & FL_INDEX != 0 {
            self.unindex(tx, di)?;
        }
        let n = self.dir_blocks(di)?;
        let area = self.area();
        if self.dirx_ready(tx, di)? {
            if self.locate(tx, di, name)?.is_some() {
                return Err(EEXIST);
            }
            // `locate` forgets an index that was not as the blocks are; then the walk below adds the name.
            if let Some(ix) = self.dirx.index(di.ino) {
                let Some(l) = ix.fit(dir::rec_len(name.len())) else {
                    return self.add_block(tx, di, name, ino, ftype, keep, n);
                };
                if let Some((p, mut b)) = self.dir_block(Some(tx), di, l)?
                    && let Some(off) = dir::insert_at(&mut b, area, name, ino, ftype)?
                {
                    let room = dir::room(&b, area)?;
                    self.put_dir(tx, di, p, b);
                    let h = self.dirx.hash(name);
                    self.dirx.update(di.ino, l, |ix| {
                        ix.insert(h, l, off)?;
                        ix.set_room(l, room)
                    });
                    return Ok(());
                }
                self.dirx_stale(di.ino, l);
            }
        }
        let one = crate::dirx::walked(n);
        if !one {
            // The VFS found the name free in its own map of the directory; a directory whose index was found
            // stale in this mount gets the look at every block all the same, until a fresh pass built its index.
            if self.dirx.suspect(di.ino) && self.find(Some(tx), di, name)?.is_some() {
                return Err(EEXIST);
            }
            self.dirx.scan_counted();
        }
        for l in 0..n {
            let Some((p, mut b)) = self.dir_block(Some(tx), di, l)? else { continue };
            // The one block is the whole directory: the answer the index gives a larger one costs no read here.
            if one && dir::walk(&b, area).filter_map(Result::ok).any(|e| e.ino != 0 && e.name(&b) == name) {
                return Err(EEXIST);
            }
            if dir::insert(&mut b, area, name, ino, ftype)? {
                self.put_dir(tx, di, p, b);
                return Ok(());
            }
        }
        if one {
            // The directory gets its second block: its index starts now, from the block just read, so the next
            // change does not pay the pass on top of its own blocks.
            self.dirx_make(tx, di)?;
        }
        self.add_block(tx, di, name, ino, ftype, keep, n)
    }

    /// A new block at the end of directory `di` (of `n` blocks) with `name` as its first entry.
    #[allow(clippy::too_many_arguments)]
    fn add_block(&mut self, tx: &mut Tx, di: &mut DiskInode, name: &[u8], ino: u32, ftype: u8, keep: u64, n: u32) -> Result<(), i64> {
        let area = self.area();
        let goal = if n > 0 { self.dir_pblk(Some(tx), di, n - 1)? + 1 } else { self.group_goal(di.ino) };
        let (p, _) = self.alloc_blocks(tx, goal, 1, keep)?;
        let mut b = dir::empty_block(self.bs(), di.csum_seed(&self.geo));
        dir::insert(&mut b, area, name, ino, ftype)?;
        let room = dir::room(&b, area)?;
        self.put_dir(tx, di, p, b);
        let tree = {
            let mut ed = Edit::new(self, tx, di, p + 1);
            ed.set(n, 1, Target::Map { pblk: p, unwritten: false })?;
            if !ed.freed.is_empty() {
                return Err(EUCLEAN);
            }
            ed.tree_blocks
        };
        di.set_size(di.size() + self.geo.bs);
        let b512 = di.blocks512(&self.geo) as i64 + (1 + tree) * (self.geo.bs / 512) as i64;
        di.set_blocks512(&self.geo, b512.max(0) as u64)?;
        let h = self.dirx.hash(name);
        self.dirx.update(di.ino, n, |ix| {
            ix.insert(h, n, 0)?;
            ix.set_room(n, room)
        });
        Ok(())
    }

    pub fn remove_name(&mut self, tx: &mut Tx, di: &DiskInode, slot: &Slot) -> Result<(), i64> {
        let mut b = self.slot_block(tx, slot);
        self.slot_entry(&b, slot)?;
        dir::remove(&mut b, self.area(), slot.e.off)?;
        let room = dir::room(&b, self.area())?;
        self.put_dir(tx, di, slot.pblk, b);
        let h = self.dirx.hash(&slot.e.name);
        let (l, off) = (slot.lblk, slot.e.off);
        self.dirx.update(di.ino, l, |ix| {
            ix.remove(h, l, off)?;
            ix.set_room(l, room)
        });
        Ok(())
    }

    pub fn retarget_name(&mut self, tx: &mut Tx, di: &DiskInode, slot: &Slot, ino: u32, ftype: u8) -> Result<(), i64> {
        let mut b = self.slot_block(tx, slot);
        dir::retarget(&mut b, slot.e.off, ino, ftype);
        self.put_dir(tx, di, slot.pblk, b);
        Ok(())
    }

    /// The `..` entry of directory `di` (block 0, right after `.`).
    pub fn dotdot(&self, tx: Option<&Tx>, di: &DiskInode) -> Result<Slot, i64> {
        if di.flags() & FL_INLINE_DATA != 0 {
            return Err(EROFS);
        }
        let p = self.dir_pblk(tx, di, 0)?;
        let b = self.read(tx, p)?;
        if g16(&b, 4) != 12 || g8(&b, 18) != 2 || b.get(20..22) != Some(b"..".as_slice()) {
            klog::event!(Warn, "ext4", "dir.bad_dotdot", ino = di.ino);
            return Err(EUCLEAN);
        }
        let e = DirEnt { off: 12, ino: g32(&b, 12), rec_len: g16(&b, 16) as usize, ftype: g8(&b, 19), name: b"..".to_vec() };
        Ok(Slot { lblk: 0, pblk: p, e, data: b })
    }

    /// No name besides `.` and `..`. "Empty" lets rmdir and a rename over the directory remove it, so it is
    /// always read from the blocks (a walk that ends at the first name); the index may only say "not empty".
    pub fn is_empty_dir(&mut self, di: &DiskInode) -> Result<bool, i64> {
        let indexed = self.dirx.index(di.ino).map(|ix| ix.live());
        if indexed.is_some_and(|live| live > 0) {
            return Ok(false);
        }
        if di.flags() & FL_INLINE_DATA != 0 {
            return Ok(self.list_dir(di)?.is_empty());
        }
        for l in 0..self.dir_blocks(di)? {
            let Some((_, b)) = self.dir_block(None, di, l)? else { continue };
            for e in dir::walk(&b, self.area()) {
                let e = e?;
                let name = e.name(&b);
                if e.ino != 0 && name != b"." && name != b".." {
                    if indexed.is_some() {
                        // The index counted no name.
                        self.dirx_stale(di.ino, l);
                    }
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// Turns an htree directory into a linear one: the root keeps `.` and `..` (whose entry now ends before
    /// the tail), each index block becomes an empty leaf; then the INDEX flag is cleared.
    pub fn unindex(&mut self, tx: &mut Tx, di: &mut DiskInode) -> Result<(), i64> {
        let bs = self.bs();
        let area = self.area();
        let seed = di.csum_seed(&self.geo);
        let n = self.dir_blocks(di)?;
        for l in 0..n {
            let p = self.dir_pblk(Some(tx), di, l)?;
            let mut b = self.read(Some(tx), p)?;
            if l == 0 {
                if g16(&b, 4) != 12 || g8(&b, 6) != 1 {
                    return Err(EUCLEAN);
                }
                p16(&mut b, 16, (area - 12) as u16);
                // The index data sits in the slack of `..`: harmless, but cleared so nothing looks like names.
                b[24..area].fill(0);
            } else if dir::is_dx_node(&b, bs) {
                b = dir::empty_block(bs, seed);
            } else {
                continue;
            }
            dir::seal(&mut b, bs, seed);
            tx.put(p, b);
        }
        // The index (if any) knew these blocks as index blocks without room: the next change builds it anew.
        tx.touch_dir(di.ino);
        self.dirx.forget(di.ino);
        di.set_flags(di.flags() & !FL_INDEX);
        klog::event!(Info, "ext4", "dir.unindexed", ino = di.ino; "htree directory turned linear for an insert (FS-8a)");
        Ok(())
    }

    /// A goal block near inode `ino`'s group.
    pub fn group_goal(&self, ino: u32) -> u64 {
        let g = (ino.max(1) - 1) / self.geo.ipg;
        self.geo.group_start(g)
    }
}
