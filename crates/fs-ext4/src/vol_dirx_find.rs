//! BIGDIR: the pass that builds a directory's index, and finding a name through it (`vol_dirx` has the state).
//!
//! What the index says is checked against the block wherever it is used: a place it gives is compared with the
//! entry there (`locate`), and "this name is not here" is taken from it only where a wrong answer adds nothing
//! and removes nothing. An unlink or rename about to answer ENOENT asks the blocks first (`locate_sure`): that
//! costs one walk of the directory, on a path the VFS reaches only with a name it holds itself (a lookup that
//! misses ends in the VFS's own map of the loaded directory and never comes here).

use crate::dir;
use crate::dirx::{self, DirIndex};
use crate::errno::*;
use crate::inode::DiskInode;
use crate::state::State;
use crate::tx::Tx;
use crate::vol_dir::Slot;

klog::counter!(BUILT, "ext4.dirx.built");
klog::counter!(BUILT_BLOCKS, "ext4.dirx.built_blocks");
klog::counter!(MISS_WALKS, "ext4.dirx.miss_walks");

/// The pass charges the index to the pool every this many blocks, so an index the pool will refuse is given up
/// early and never held whole without a charge (64 KiB of a directory make about 20 KiB of index).
const CHARGE_BLOCKS: u32 = 16;

impl State {
    /// One pass over directory `di` as `tx` sees it. `charge`: to the mount's pool as it grows (not for the
    /// tests' comparison). None: no memory for it.
    pub(crate) fn dirx_build(&self, tx: Option<&Tx>, di: &DiskInode, charge: bool) -> Result<Option<DirIndex>, i64> {
        let n = self.dir_blocks(di)?;
        let area = dir::area(self.bs(), self.geo.csum.is_some());
        let mut ix = DirIndex::default();
        if !ix.reserve_blocks(n) {
            return Ok(None);
        }
        for l in 0..n {
            let mut room = 0;
            if let Some((_, b)) = self.dir_block(tx, di, l)? {
                for e in dir::walk(&b, area) {
                    let e = e?;
                    room = room.max(e.slack());
                    let name = e.name(&b);
                    if e.ino == 0 || name == b"." || name == b".." {
                        continue;
                    }
                    if e.ino > self.geo.inodes {
                        klog::event!(Warn, "ext4", "dir.bad_inode", dir = di.ino, ino = e.ino);
                        return Err(EUCLEAN);
                    }
                    // Two entries at one place cannot be: the walk gives each offset once.
                    let _ = ix.insert(self.dirx.hash(name), l, e.off);
                }
            }
            if ix.set_room(l, room).is_err() || (charge && (l + 1) % CHARGE_BLOCKS == 0 && !self.dirx.covers(&mut ix)) {
                return Ok(None);
            }
        }
        if charge && !self.dirx.covers(&mut ix) {
            return Ok(None);
        }
        Ok(Some(ix))
    }

    /// Whether directory `di` has its index now (built here when missing). False: the caller walks the
    /// directory (its one block, or all of them: switched off for a test, an inline directory, no memory).
    pub fn dirx_ready(&mut self, tx: &Tx, di: &DiskInode) -> Result<bool, i64> {
        let now = self.dirx.now_s();
        if let Some(ix) = self.dirx.map.get_mut(&di.ino) {
            ix.used = now;
            return Ok(true);
        }
        if dirx::walked(self.dir_blocks(di)?) {
            return Ok(false);
        }
        self.dirx_make(tx, di)
    }

    /// Builds the index of `di` by one pass, whatever its size (`dirx_ready`; `add_name` for a directory about
    /// to get its second block). False: there is none (switched off, inline, the pool refused lately or now).
    pub fn dirx_make(&mut self, tx: &Tx, di: &DiskInode) -> Result<bool, i64> {
        let now = self.dirx.now_s();
        if self.dirx.off || di.flags() & crate::inode::FL_INLINE_DATA != 0 || now < self.dirx.refused_until {
            return Ok(false);
        }
        let Some(mut ix) = self.dirx_build(Some(tx), di, true)? else {
            self.dirx.refused();
            klog::event!(Warn, "ext4", "dirx.refused", dir = di.ino, size = di.size(); "no memory for a directory's index: changes walk their directories for a while");
            return Ok(false);
        };
        ix.used = now;
        BUILT.inc();
        BUILT_BLOCKS.add(ix.blocks() as u64);
        self.dirx.built(di.ino, ix);
        Ok(true)
    }

    /// The index of `ino` does not match its blocks (`Dirs::stale`).
    pub fn dirx_stale(&mut self, ino: u32, block: u32) {
        self.dirx.stale(ino, block);
    }

    /// Where `name` sits in directory `di`: through the index (one block read), or by walking the directory
    /// when it has none. None from an indexed directory is the index's word alone (`locate_sure`).
    pub fn locate(&mut self, tx: &Tx, di: &DiskInode, name: &[u8]) -> Result<Option<Slot>, i64> {
        if !self.dirx_ready(tx, di)? {
            return self.find(Some(tx), di, name);
        }
        let h = self.dirx.hash(name);
        let area = dir::area(self.bs(), self.geo.csum.is_some());
        let (mut block, mut off) = (0u32, 0u16);
        loop {
            let Some((l, o)) = self.dirx.map.get(&di.ino).and_then(|ix| ix.place(h, block, off)) else { return Ok(None) };
            let Some((p, b)) = self.dir_block(Some(tx), di, l)? else {
                self.dirx_stale(di.ino, l);
                return self.find(Some(tx), di, name);
            };
            let mut at = None;
            for e in dir::walk(&b, area) {
                let e = e?;
                if e.off == o as usize && e.ino != 0 {
                    at = Some(e);
                }
                if e.off >= o as usize {
                    break;
                }
            }
            let Some(e) = at else {
                self.dirx_stale(di.ino, l);
                return self.find(Some(tx), di, name);
            };
            if e.name(&b) == name {
                let e = e.owned(&b);
                return Ok(Some(Slot { lblk: l, pblk: p, e, data: b }));
            }
            // Another name with this hash: the next place.
            (block, off) = (l, o + 1);
        }
    }

    /// `locate` for a caller that answers ENOENT to None (unlink, rmdir, rename, exchange): the index's "not
    /// here" is confirmed by a walk over the directory's blocks. A name the walk finds was missing from the
    /// index: stale, and the walk's answer stands.
    pub fn locate_sure(&mut self, tx: &Tx, di: &DiskInode, name: &[u8]) -> Result<Option<Slot>, i64> {
        if let Some(s) = self.locate(tx, di, name)? {
            return Ok(Some(s));
        }
        // No index (also: one `locate` just found stale): `locate` walked already.
        if self.dirx.index(di.ino).is_none() {
            return Ok(None);
        }
        MISS_WALKS.inc();
        let found = self.find(Some(tx), di, name)?;
        if let Some(s) = &found {
            self.dirx_stale(di.ino, s.lblk);
        }
        Ok(found)
    }
}
