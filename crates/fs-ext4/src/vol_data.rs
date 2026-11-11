//! File content on the volume: reading through the block mapping (holes and unwritten extents read zero),
//! writing a run of pages in the order data needs (blocks marked used, then the data, then the extents and
//! the inode that point at it), truncate, the orphan list and freeing an inode.

use crate::extent::{self, Edit, Ext, Found, Target};
use crate::inode::{DiskInode, FL_INLINE_DATA};
use crate::le::*;
use crate::sb;
use crate::state::State;
use crate::tx::Tx;
use alloc::vec;
use alloc::vec::Vec;
use crate::errno::*;

/// A piece of a write: file blocks [l, l + n) at disk block `p`; `fresh` when allocated for this write,
/// `unwritten` when it was a preallocated (unwritten) extent.
struct Seg {
    l: u64,
    n: u64,
    p: u64,
    fresh: bool,
    unwritten: bool,
}

/// Largest single request to the device (bytes).
const IO_MAX: usize = 1 << 20;

/// A file read's pieces in order (`State::read_plan`): FS-STALL maps under the volume lock and reads after it.
pub struct ReadPlan {
    pieces: Vec<Piece>,
    total: usize,
}

/// `len` bytes of the read: at disk block `p`, `in_blk` bytes into it; None reads zeros (a hole, an unwritten extent).
struct Piece {
    len: usize,
    at: Option<(u64, usize)>,
}

impl ReadPlan {
    /// BL-3: the pieces in file order as (bytes, Some((disk block, bytes into it)) or None for zeros). One piece is
    /// one extent's contiguous run (at most `IO_MAX`), so it is one device request.
    pub(crate) fn pieces(&self) -> impl Iterator<Item = (usize, Option<(u64, usize)>)> + '_ {
        self.pieces.iter().map(|p| (p.len, p.at))
    }
    /// Bytes the plan covers (the read cut at the file's size).
    pub(crate) fn total(&self) -> usize {
        self.total
    }
}

/// Carries out a read plan into `buf` through `read(block, bytes)` (whole blocks); returns the count.
pub fn read_by_plan(plan: &ReadPlan, bs: u64, buf: &mut [u8], read: &mut dyn FnMut(u64, &mut [u8]) -> Result<(), i64>) -> Result<usize, i64> {
    let mut done = 0usize;
    let mut tmp = Vec::new();
    for pc in &plan.pieces {
        let dst = &mut buf[done..done + pc.len];
        match pc.at {
            None => dst.fill(0),
            Some((p, 0)) if pc.len % bs as usize == 0 => read(p, dst)?,
            Some((p, in_blk)) => {
                let blocks = (in_blk + pc.len).div_ceil(bs as usize);
                tmp.clear();
                tmp.try_reserve(blocks * bs as usize).map_err(|_| ENOMEM)?;
                tmp.resize(blocks * bs as usize, 0);
                read(p, &mut tmp)?;
                dst.copy_from_slice(&tmp[in_blk..in_blk + pc.len]);
            }
        }
        done += pc.len;
    }
    Ok(plan.total)
}

impl State {
    /// Where file block `l` lies: Some((disk block, blocks mapped from it, unwritten)) or None and the hole's
    /// length (u64::MAX: to the end).
    pub(crate) fn map_at(&self, tx: Option<&Tx>, di: &DiskInode, l: u64) -> Result<Result<(u64, u64, bool), u64>, i64> {
        if l >= 1 << 32 {
            return Ok(Err(u64::MAX));
        }
        if !extent::uses_extents(di) {
            return Ok(match crate::blockmap::map(self, tx, di, l as u32)? {
                Some(p) => Ok((p, 1, false)),
                None => Err(1),
            });
        }
        Ok(match extent::lookup(self, tx, di, l as u32)? {
            Found::Ext(e) => {
                let k = l - e.lblk as u64;
                Ok((e.pblk + k, e.len as u64 - k, e.unwritten))
            }
            Found::Hole(next) => Err(next.map_or(u64::MAX, |n| n as u64 - l)),
        })
    }

    /// Reads file bytes at `off` (up to the size); returns the count.
    pub fn read_file(&self, di: &DiskInode, off: u64, buf: &mut [u8]) -> Result<usize, i64> {
        match self.read_plan(di, off, buf.len())? {
            None => crate::inline::read(self, di, off, buf),
            Some(plan) => read_by_plan(&plan, self.geo.bs, buf, &mut |p, dst| self.read_blocks(p, dst)),
        }
    }

    /// Where the bytes of a read of `len` at `off` lie (None: inline data, read through the inode).
    pub fn read_plan(&self, di: &DiskInode, off: u64, len: usize) -> Result<Option<ReadPlan>, i64> {
        if di.flags() & FL_INLINE_DATA != 0 {
            return Ok(None);
        }
        let mut plan = ReadPlan { pieces: Vec::new(), total: 0 };
        let size = di.size();
        if off >= size {
            return Ok(Some(plan));
        }
        let total = len.min((size - off) as usize);
        let bs = self.geo.bs;
        let mut done = 0usize;
        while done < total {
            let pos = off + done as u64;
            let l = pos / bs;
            let in_blk = (pos % bs) as usize;
            let left = total - done;
            let piece = match self.map_at(None, di, l)? {
                Ok((p, n, unwritten)) => {
                    let span = ((n * bs) as usize - in_blk).min(left).min(IO_MAX);
                    Piece { len: span, at: (!unwritten).then_some((p, in_blk)) }
                }
                Err(hole) => {
                    let span = (hole.saturating_mul(bs).saturating_sub(in_blk as u64)).min(left as u64) as usize;
                    Piece { len: span.max(1).min(left), at: None }
                }
            };
            done += piece.len;
            plan.pieces.try_reserve(1).map_err(|_| ENOMEM)?;
            plan.pieces.push(piece);
        }
        plan.total = total;
        Ok(Some(plan))
    }

    /// Turns a block-mapped (ext2/ext3) file, directory or slow symlink into an extent tree before V2
    /// changes it (`ext4_ext_migrate`, linux-6.6/fs/ext4/migrate.c:409): the new tree and the inode in one
    /// transaction, the old indirect blocks freed in the next. Nothing to do for extent, inline, device and
    /// fast-symlink inodes.
    pub fn migrate(&mut self, ino: u32) -> Result<(), i64> {
        let di = self.inode(None, ino)?;
        let mapped = di.is_reg() || di.is_dir() || (di.is_symlink() && di.blocks512(&self.geo) != 0);
        if extent::uses_extents(&di) || di.flags() & FL_INLINE_DATA != 0 || !mapped {
            return Ok(());
        }
        let (runs, ind) = crate::blockmap::collect(self, None, &di)?;
        let bs = self.geo.bs;
        let goal = ind.first().copied().unwrap_or_else(|| self.group_goal(ino));
        self.with_tx(|st, tx| {
            let mut di = st.inode(Some(tx), ino)?;
            di.set_flags(di.flags() | crate::inode::FL_EXTENTS);
            extent::init_root(di.i_block_mut());
            let tree = {
                let mut ed = Edit::new(st, tx, &mut di, goal);
                for &(l, p, n) in &runs {
                    ed.set(l as u32, n, Target::Map { pblk: p, unwritten: false })?;
                }
                ed.tree_blocks
            };
            let b512 = di.blocks512(&st.geo) as i64 + (tree - ind.len() as i64) * (bs / 512) as i64;
            di.set_blocks512(&st.geo, b512.max(0) as u64)?;
            st.put_inode(tx, &mut di)
        })?;
        klog::event!(Info, "ext4", "migrated", ino = ino, runs = runs.len(), indirect = ind.len());
        let list: Vec<(u64, u64)> = ind.iter().map(|&b| (b, 1)).collect();
        self.free_list(&list)
    }

    /// fallocate's allocation (`ext4_alloc_file_blocks`, linux-6.6/fs/ext4/extents.c:4416): every hole of
    /// [off, off + len) gets unwritten extents (read as zeros until written) in one transaction, so the space
    /// survives eviction and a cut; the size is the caller's (mode 0 extends it afterwards). `keep`: as
    /// `alloc_blocks`.
    pub fn fallocate(&mut self, ino: u32, off: u64, len: u64, keep: u64) -> Result<(), i64> {
        let bs = self.geo.bs;
        if len == 0 {
            return Ok(());
        }
        let first = off / bs;
        let last = off.checked_add(len).ok_or(EFBIG)?.div_ceil(bs);
        if last > 1 << 32 {
            return Err(EFBIG);
        }
        self.migrate(ino)?;
        self.with_tx(|st, tx| {
            let mut di = st.inode(Some(tx), ino)?;
            if !di.is_reg() {
                return Err(EINVAL);
            }
            let mut segs: Vec<(u64, u64, u64)> = Vec::new();
            let mut goal = st.file_goal(&di, first)?;
            let mut l = first;
            while l < last {
                match st.map_at(Some(tx), &di, l)? {
                    Ok((p, m, _)) => {
                        goal = p + m;
                        l += m.min(last - l);
                    }
                    Err(hole) => {
                        let m = hole.min(last - l).min(extent::UNWRITTEN_MAX as u64);
                        let (p, got) = st.alloc_blocks(tx, goal, m, keep)?;
                        segs.push((l, got, p));
                        goal = p + got;
                        l += got;
                    }
                }
            }
            if segs.is_empty() {
                return Ok(());
            }
            let fresh: u64 = segs.iter().map(|s| s.1).sum();
            let tree = {
                let mut ed = Edit::new(st, tx, &mut di, goal);
                for &(l, n, p) in &segs {
                    ed.set(l as u32, n, Target::Map { pblk: p, unwritten: true })?;
                }
                ed.tree_blocks
            };
            let b512 = di.blocks512(&st.geo) as i64 + (fresh as i64 + tree) * (bs / 512) as i64;
            di.set_blocks512(&st.geo, b512.max(0) as u64)?;
            di.set_time(crate::inode::Stamp::C, st.now());
            st.put_inode(tx, &mut di)
        })
    }

    /// Writes `data` at `off` (block aligned) of a regular file whose size is `size`; the disk size grows to
    /// cover what was written (never past `size`). `keep`: blocks other files' cached data was promised.
    pub fn write_file(&mut self, ino: u32, off: u64, data: &[u8], size: u64, keep: u64) -> Result<(), i64> {
        let bs = self.geo.bs;
        let mut di = self.inode(None, ino)?;
        if !di.is_reg() {
            return Err(EINVAL);
        }
        if !extent::uses_extents(&di) {
            self.migrate(ino)?;
            di = self.inode(None, ino)?;
        }
        if di.flags() & FL_INLINE_DATA != 0 {
            // Only on an inline_data volume, which mounts read-only (`sb::parse`).
            return Err(EROFS);
        }
        if off % bs != 0 {
            return Err(EINVAL);
        }
        let first = off / bs;
        let n = (data.len() as u64).div_ceil(bs);
        if first + n > 1 << 32 {
            return Err(EFBIG);
        }
        // 1. the mapping, with new blocks for the holes (marked used first)
        let mut segs: Vec<Seg> = Vec::new();
        let mut atx = Tx::new();
        let mut goal = self.file_goal(&di, first)?;
        let mut l = first;
        let r = (|| -> Result<(), i64> {
            while l < first + n {
                let want = first + n - l;
                match self.map_at(Some(&atx), &di, l)? {
                    Ok((p, m, unwritten)) => {
                        let m = m.min(want);
                        segs.push(Seg { l, n: m, p, fresh: false, unwritten });
                        goal = p + m;
                        l += m;
                    }
                    Err(hole) => {
                        let m = hole.min(want).min(extent::INIT_MAX as u64);
                        let (p, got) = self.alloc_blocks(&mut atx, goal, m, keep)?;
                        segs.push(Seg { l, n: got, p, fresh: true, unwritten: false });
                        goal = p + got;
                        l += got;
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = r {
            // Nothing points at the blocks taken so far: the disk never saw them taken.
            self.abort(atx);
            return Err(e);
        }
        // Without a journal the allocation is on the disk before the data (FS-8a's order). With one, the
        // allocation, the mapping and the frees stay one transaction that joins after the data was written
        // (ordered mode), so no commit can fall between them and leave blocks taken that nothing uses.
        let journal = self.jnl.is_some();
        if !journal {
            self.commit(core::mem::take(&mut atx))?;
        }
        // 2. the data
        if let Err(e) = self.write_segs(&segs, first, data) {
            if journal {
                self.abort(atx);
            }
            return Err(e);
        }
        // 3. the pointers and the inode
        let mut map = |st: &mut State, tx: &mut Tx| -> Result<Vec<(u64, u64)>, i64> {
            let mut fresh = 0u64;
            let (tree, freed) = {
                let mut ed = Edit::new(st, tx, &mut di, goal);
                for s in segs.iter().filter(|s| s.fresh || s.unwritten) {
                    ed.set(s.l as u32, s.n, Target::Map { pblk: s.p, unwritten: false })?;
                    if s.fresh {
                        fresh += s.n;
                    }
                }
                (ed.tree_blocks, core::mem::take(&mut ed.freed))
            };
            let new_size = di.size().max(size.min(off + data.len() as u64));
            di.set_size(new_size);
            let b512 = di.blocks512(&st.geo) as i64 + (fresh as i64 + tree) * (bs / 512) as i64;
            di.set_blocks512(&st.geo, b512.max(0) as u64)?;
            let t = st.now();
            di.set_time(crate::inode::Stamp::C, t);
            st.put_inode(tx, &mut di)?;
            Ok(freed)
        };
        if !journal {
            let freed = self.with_tx(map)?;
            return self.free_list(&freed);
        }
        let r = map(self, &mut atx).and_then(|freed| freed.iter().try_for_each(|&(p, n)| self.free_blocks_range(&mut atx, p, n)));
        match r {
            Ok(()) => self.commit(atx),
            Err(e) => {
                self.abort(atx);
                Err(e)
            }
        }
    }

    /// Step 2 of `write_file`: the data of every segment to its blocks (the last block padded with zeros).
    fn write_segs(&mut self, segs: &[Seg], first: u64, data: &[u8]) -> Result<(), i64> {
        let bs = self.geo.bs;
        let mut pad = Vec::new();
        for s in segs {
            let mut b = 0u64;
            while b < s.n {
                let at = ((s.l - first + b) * bs) as usize;
                let cnt = ((s.n - b) as usize).min(IO_MAX / bs as usize);
                let end = (at + cnt * bs as usize).min(data.len());
                let src = &data[at..end];
                // The whole blocks straight from the caller's buffer; only a short last block goes through a copy
                // of one block (a copy of the whole piece was up to 1 MiB from the heap, inside write-back).
                let whole = src.len() / bs as usize * bs as usize;
                if whole > 0 {
                    self.write_blocks(s.p + b, &src[..whole])?;
                }
                if whole < src.len() {
                    pad.clear();
                    pad.extend_from_slice(&src[whole..]);
                    pad.resize(bs as usize, 0);
                    let p = core::mem::take(&mut pad);
                    self.write_blocks(s.p + b + whole as u64 / bs, &p)?;
                    pad = p;
                }
                b += cnt as u64;
            }
        }
        Ok(())
    }

    /// Frees blocks in a transaction of their own (after the one that dropped the last pointer).
    pub fn free_list(&mut self, list: &[(u64, u64)]) -> Result<(), i64> {
        if list.is_empty() {
            return Ok(());
        }
        self.with_tx(|st, tx| {
            for &(p, n) in list {
                st.free_blocks_range(tx, p, n)?;
            }
            Ok(())
        })
    }

    /// Where new blocks for file block `l` should go: after the block before it, else the inode's group.
    fn file_goal(&self, di: &DiskInode, l: u64) -> Result<u64, i64> {
        if l > 0
            && let Ok(Ok((p, _, _))) = self.map_at(None, di, l - 1)
        {
            return Ok(p + 1);
        }
        Ok(self.group_goal(di.ino))
    }

    /// Sets the size of a regular file on disk: blocks past it are freed, the rest of the last block is
    /// zeroed (so a later extension reads zeros, `ext4_block_truncate_page`).
    pub fn set_file_size(&mut self, ino: u32, size: u64) -> Result<(), i64> {
        let bs = self.geo.bs;
        self.migrate(ino)?;
        let mut di = self.inode(None, ino)?;
        if di.flags() & FL_INLINE_DATA != 0 {
            return Err(EROFS);
        }
        if size > (1u64 << 32) * bs {
            return Err(EFBIG);
        }
        let old = di.size();
        let edge = old.min(size);
        if edge % bs != 0
            && let Ok((p, _, false)) = self.map_at(None, &di, edge / bs)?
        {
            let mut b = vec![0u8; bs as usize];
            self.read_blocks(p, &mut b)?;
            b[(edge % bs) as usize..].fill(0);
            self.write_blocks(p, &b)?;
        }
        let keep_blocks = size.div_ceil(bs);
        let all = self.with_tx(|st, tx| {
            let (tree, mut displaced, freed) = {
                let mut ed = Edit::new(st, tx, &mut di, 0);
                if keep_blocks < 1 << 32 {
                    ed.set(keep_blocks as u32, (1u64 << 32) - keep_blocks, Target::Unmap)?;
                }
                (ed.tree_blocks, core::mem::take(&mut ed.displaced), core::mem::take(&mut ed.freed))
            };
            let gone: u64 = displaced.iter().map(|d| d.1).sum();
            di.set_size(size);
            let b512 = di.blocks512(&st.geo) as i64 - (gone as i64 - tree) * (bs / 512) as i64;
            di.set_blocks512(&st.geo, b512.max(0) as u64)?;
            let t = st.now();
            di.set_time(crate::inode::Stamp::M, t);
            di.set_time(crate::inode::Stamp::C, t);
            st.put_inode(tx, &mut di)?;
            displaced.extend(freed);
            Ok(displaced)
        })?;
        self.free_list(&all)
    }

    /// Every extent of a file (tests, `blocks`).
    pub fn extents(&self, di: &DiskInode) -> Result<Vec<Ext>, i64> {
        Ok(extent::all(self, None, di)?.0)
    }

    /// Puts `di` on the orphan list (`ext4_orphan_add`, the classic list through `i_dtime`).
    pub fn orphan_add(&mut self, tx: &mut Tx, di: &mut DiskInode) {
        self.save_sb(tx);
        let head = g32(&self.sb, sb::LAST_ORPHAN);
        di.set_dtime(head);
        p32(&mut self.sb, sb::LAST_ORPHAN, di.ino);
        tx.sb_dirty = true;
    }

    /// Takes inode `ino` off the orphan list (`ext4_orphan_del`).
    pub fn orphan_del(&mut self, tx: &mut Tx, ino: u32) -> Result<(), i64> {
        let mut cur = g32(&self.sb, sb::LAST_ORPHAN);
        let next_of = |s: &State, i: u32| -> Result<u32, i64> { Ok(s.inode(Some(tx), i)?.dtime()) };
        let mine = self.inode(Some(tx), ino)?.dtime();
        if cur == ino {
            self.save_sb(tx);
            p32(&mut self.sb, sb::LAST_ORPHAN, mine);
            tx.sb_dirty = true;
            return Ok(());
        }
        let mut steps = 0u32;
        while cur != 0 && steps <= self.geo.inodes {
            let nx = next_of(self, cur)?;
            if nx == ino {
                let mut p = self.inode(Some(tx), cur)?;
                p.set_dtime(mine);
                self.put_inode(tx, &mut p)?;
                return Ok(());
            }
            cur = nx;
            steps += 1;
        }
        klog::event!(Warn, "ext4", "orphan.missing", ino = ino);
        Ok(())
    }

    /// Frees an inode whose last link and last user went (`ext4_evict_inode` + `ext4_free_inode`): its
    /// blocks and xattr block are unmapped and the inode is cleared (dtime set) in one transaction, the
    /// blocks and the inode bit freed in the next. `orphan`: it is on the orphan list.
    pub fn evict(&mut self, ino: u32, orphan: bool) -> Result<(), i64> {
        let dir = self.inode(None, ino)?.is_dir();
        if let Err(e) = self.migrate(ino) {
            klog::event!(Warn, "ext4", "evict.migrate_failed", ino = ino, errno = e; "block-mapped inode: blocks left for e2fsck");
        }
        let free = self.with_tx(|st, tx| {
            if orphan {
                st.orphan_del(tx, ino)?;
            }
            let mut di = st.inode(Some(tx), ino)?;
            let mut free: Vec<(u64, u64)> = Vec::new();
            if extent::uses_extents(&di) && di.flags() & FL_INLINE_DATA == 0 {
                let mut ed = Edit::new(st, tx, &mut di, 0);
                ed.set(0, 1 << 32, Target::Unmap)?;
                free.extend(core::mem::take(&mut ed.displaced));
                free.extend(core::mem::take(&mut ed.freed));
            } else if !di.is_symlink() && di.blocks512(&st.geo) != 0 && !matches!(di.ftype(), crate::inode::S_IFCHR | crate::inode::S_IFBLK | crate::inode::S_IFIFO | crate::inode::S_IFSOCK) {
                klog::event!(Warn, "ext4", "evict.blockmap", ino = ino; "block-mapped inode: blocks left for e2fsck");
            }
            if let Some(b) = st.release_xattr_block(tx, &mut di)? {
                free.push((b, 1));
            }
            let t = st.now();
            di.set_links(0);
            di.set_size(0);
            di.set_blocks512(&st.geo, 0)?;
            di.set_dtime(t.sec as u32);
            st.put_inode(tx, &mut di)?;
            Ok(free)
        })?;
        self.with_tx(|st, tx| {
            for &(p, n) in &free {
                st.free_blocks_range(tx, p, n)?;
            }
            st.free_inode(tx, ino, dir)
        })?;
        self.nodes.remove(&ino);
        Ok(())
    }

    /// Mount-time `ext4_orphan_cleanup` (orphan.c): unlinked inodes are freed, linked ones truncated to
    /// their size (a truncate cut short).
    pub fn orphan_cleanup(&mut self) -> Result<(), i64> {
        let mut cur = g32(&self.sb, sb::LAST_ORPHAN);
        let mut n = 0u32;
        while cur != 0 {
            if cur > self.geo.inodes || n > self.geo.inodes || !self.inode_in_use(cur)? {
                klog::event!(Warn, "ext4", "orphan.bad_list", ino = cur, seen = n; "orphan list cut here");
                return self.with_tx(|st, tx| {
                    st.save_sb(tx);
                    p32(&mut st.sb, sb::LAST_ORPHAN, 0);
                    tx.sb_dirty = true;
                    Ok(())
                });
            }
            let di = self.inode(None, cur)?;
            let next = di.dtime();
            if di.links() == 0 {
                self.evict(cur, true)?;
                klog::event!(Info, "ext4", "orphan.freed", ino = cur);
            } else {
                let d = self.with_tx(|st, tx| {
                    st.orphan_del(tx, cur)?;
                    let mut d = st.inode(Some(tx), cur)?;
                    d.set_dtime(0);
                    st.put_inode(tx, &mut d)?;
                    Ok(d)
                })?;
                if d.is_reg() {
                    self.set_file_size(cur, d.size())?;
                }
                klog::event!(Info, "ext4", "orphan.truncated", ino = cur);
            }
            cur = if g32(&self.sb, sb::LAST_ORPHAN) == cur { next } else { g32(&self.sb, sb::LAST_ORPHAN) };
            n += 1;
        }
        Ok(())
    }
}
