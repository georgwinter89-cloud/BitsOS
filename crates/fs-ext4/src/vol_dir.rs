//! Directories on the volume: walking their blocks, listing (linear and htree directories alike: an htree's
//! index blocks hold no names), finding, adding and removing names. Adding to an htree directory first turns
//! it into a linear one (its index blocks become empty leaf blocks, the INDEX flag goes): Linux reads such a
//! directory as any linear one, and FS-8a does not insert into hash order (decision in the FS-8a notes).

use crate::dir::{self, DirEnt};
use crate::extent::{self, Edit, Found, Target};
use crate::inode::{DiskInode, FL_INDEX, FL_INLINE_DATA};
use crate::le::*;
use crate::state::State;
use crate::tx::Tx;
use alloc::vec::Vec;
use crate::errno::*;

/// Where a name sits: (logical block, its disk block, entry).
pub struct Slot {
    pub lblk: u32,
    pub pblk: u64,
    pub e: DirEnt,
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
    fn dir_block(&self, tx: Option<&Tx>, di: &DiskInode, l: u32) -> Result<Option<(u64, Vec<u8>)>, i64> {
        let p = self.dir_pblk(tx, di, l)?;
        let b = self.read(tx, p)?;
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
            for e in dir::entries(&b, self.area())? {
                if e.ino == 0 || e.name == b"." || e.name == b".." {
                    continue;
                }
                if e.ino > self.geo.inodes {
                    klog::event!(Warn, "ext4", "dir.bad_inode", dir = di.ino, ino = e.ino);
                    return Err(EUCLEAN);
                }
                out.push((e.name, e.ino, e.ftype));
            }
        }
        Ok(out)
    }

    pub fn find(&self, tx: Option<&Tx>, di: &DiskInode, name: &[u8]) -> Result<Option<Slot>, i64> {
        let n = self.dir_blocks(di)?;
        for l in 0..n {
            let Some((p, b)) = self.dir_block(tx, di, l)? else { continue };
            for e in dir::entries(&b, self.area())? {
                if e.ino != 0 && e.name == name {
                    return Ok(Some(Slot { lblk: l, pblk: p, e }));
                }
            }
        }
        Ok(None)
    }

    fn seal_dir(&self, di: &DiskInode, b: &mut [u8]) {
        dir::seal(b, self.bs(), di.csum_seed(&self.geo));
    }

    /// `ext4_add_entry` without hash order: the first block with room, else a new block at the end.
    /// `di` is the directory (changed in memory: size and blocks; the caller writes it).
    /// A new directory block leaves `keep` free blocks (`State::keep_for`).
    pub fn add_name(&mut self, tx: &mut Tx, di: &mut DiskInode, name: &[u8], ino: u32, ftype: u8, keep: u64) -> Result<(), i64> {
        if di.flags() & FL_INLINE_DATA != 0 || !extent::uses_extents(di) {
            return Err(EROFS);
        }
        if di.flags() & FL_INDEX != 0 {
            self.unindex(tx, di)?;
        }
        let n = self.dir_blocks(di)?;
        let area = self.area();
        for l in 0..n {
            let Some((p, mut b)) = self.dir_block(Some(tx), di, l)? else { continue };
            if dir::insert(&mut b, area, name, ino, ftype)? {
                self.seal_dir(di, &mut b);
                tx.put(p, b);
                return Ok(());
            }
        }
        let goal = if n > 0 { self.dir_pblk(Some(tx), di, n - 1)? + 1 } else { self.group_goal(di.ino) };
        let (p, _) = self.alloc_blocks(tx, goal, 1, keep)?;
        let mut b = dir::empty_block(self.bs(), di.csum_seed(&self.geo));
        dir::insert(&mut b, area, name, ino, ftype)?;
        self.seal_dir(di, &mut b);
        tx.put(p, b);
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
        Ok(())
    }

    pub fn remove_name(&mut self, tx: &mut Tx, di: &DiskInode, slot: &Slot) -> Result<(), i64> {
        let mut b = self.read(Some(tx), slot.pblk)?;
        dir::remove(&mut b, self.area(), slot.e.off)?;
        self.seal_dir(di, &mut b);
        tx.put(slot.pblk, b);
        Ok(())
    }

    pub fn retarget_name(&mut self, tx: &mut Tx, di: &DiskInode, slot: &Slot, ino: u32, ftype: u8) -> Result<(), i64> {
        let mut b = self.read(Some(tx), slot.pblk)?;
        dir::retarget(&mut b, slot.e.off, ino, ftype);
        self.seal_dir(di, &mut b);
        tx.put(slot.pblk, b);
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
        Ok(Slot { lblk: 0, pblk: p, e })
    }

    pub fn is_empty_dir(&self, di: &DiskInode) -> Result<bool, i64> {
        Ok(self.list_dir(di)?.is_empty())
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
