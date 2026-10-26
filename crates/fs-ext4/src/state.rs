//! The volume's state behind its lock (`ext4.vol`): superblock, group descriptors, the inodes in memory, and
//! block and inode I/O through a transaction.

use crate::dev::Dev;
use crate::gd::{self, Table};
use crate::inode::DiskInode;
use crate::journal::Journal;
use crate::le::*;
use crate::sb::{self, Geo};
use crate::tx::Tx;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use crate::errno::*;
use vfs::abi::Time;

/// An inode the VFS holds (or one being changed).
pub struct Node {
    pub di: DiskInode,
    /// VFS inodes for it (normally one; two only while a racing lookup's loser is dropped): freeing waits
    /// for the last `forget` (unlinked while open: on the orphan list).
    pub holders: u32,
    /// File pages promised blocks by `write_at` that the write-back has not allocated yet (ENOSPC at write):
    /// page -> blocks promised.
    pub reserved: BTreeMap<u64, u32>,
}

pub struct State {
    pub dev: Arc<dyn Dev>,
    pub geo: Geo,
    pub sb: Vec<u8>,
    pub gd: Table,
    pub nodes: BTreeMap<u32, Node>,
    /// Blocks promised to cached file data (the sum of the nodes' reservations, in blocks).
    pub reserved: u64,
    pub now: fn() -> Time,
    pub next_gen: u32,
    pub writable: bool,
    /// Bytes written to the device (superblock `s_kbytes_written`).
    pub written: u64,
    /// The internal journal (FS-8b); None: no journal, or one V2 does not use (then read-only).
    pub jnl: Option<Journal>,
    /// The superblock on disk says "not cleanly unmounted" (and "needs recovery" with a journal).
    pub dirty: bool,
    /// Frozen for a snapshot (VS-2): no change reaches the disk until the thaw.
    pub frozen: bool,
}

impl State {
    pub fn bs(&self) -> usize {
        self.geo.bs as usize
    }
    pub fn now(&self) -> Time {
        (self.now)()
    }
    /// A metadata block as this transaction sees it.
    pub fn read(&self, tx: Option<&Tx>, blk: u64) -> Result<Vec<u8>, i64> {
        if let Some(d) = tx.and_then(|t| t.get(blk)) {
            return Ok(d.to_vec());
        }
        if let Some(d) = self.jnl.as_ref().and_then(|j| j.run.blocks.get(&blk)) {
            return Ok(d.clone());
        }
        self.read_block(blk)
    }
    pub fn read_block(&self, blk: u64) -> Result<Vec<u8>, i64> {
        if blk >= self.geo.blocks {
            klog::event!(Warn, "ext4", "block.out_of_range", block = blk);
            return Err(EUCLEAN);
        }
        let mut b = Vec::new();
        b.try_reserve_exact(self.bs()).map_err(|_| ENOMEM)?;
        b.resize(self.bs(), 0);
        self.dev.read(blk * self.geo.bs, &mut b)?;
        Ok(b)
    }
    /// Reads `n` consecutive blocks into `buf` (file data).
    pub fn read_blocks(&self, blk: u64, buf: &mut [u8]) -> Result<(), i64> {
        let n = (buf.len() as u64).div_ceil(self.geo.bs);
        if blk.checked_add(n).is_none_or(|e| e > self.geo.blocks) {
            return Err(EUCLEAN);
        }
        self.dev.read(blk * self.geo.bs, buf)?;
        // Metadata of the running journal transaction (directory blocks, slow symlinks) is not home yet.
        if let Some(j) = self.jnl.as_ref() {
            let bs = self.geo.bs as usize;
            for (b, d) in j.run.blocks.range(blk..blk + n) {
                let at = (*b - blk) as usize * bs;
                let end = (at + bs).min(buf.len());
                buf[at..end].copy_from_slice(&d[..end - at]);
            }
        }
        Ok(())
    }
    pub fn write_blocks(&mut self, blk: u64, data: &[u8]) -> Result<(), i64> {
        let n = (data.len() as u64).div_ceil(self.geo.bs);
        if blk.checked_add(n).is_none_or(|e| e > self.geo.blocks) || blk < self.geo.first_data_block {
            return Err(EUCLEAN);
        }
        self.written += data.len() as u64;
        self.dev.write(blk * self.geo.bs, data)
    }

    /// Where inode `ino` lives: (block, byte offset in it).
    pub fn inode_loc(&self, ino: u32) -> Result<(u64, usize), i64> {
        if ino == 0 || ino > self.geo.inodes {
            return Err(EUCLEAN);
        }
        let g = (ino - 1) / self.geo.ipg;
        let idx = ((ino - 1) % self.geo.ipg) as u64;
        let byte = idx * self.geo.inode_size as u64;
        Ok((self.gd.inode_table(g) + byte / self.geo.bs, (byte % self.geo.bs) as usize))
    }
    /// The inode as on disk (or as this transaction changed it; or the node's copy when the VFS holds it).
    pub fn inode(&self, tx: Option<&Tx>, ino: u32) -> Result<DiskInode, i64> {
        if let Some(n) = self.nodes.get(&ino) {
            return Ok(n.di.clone());
        }
        let (blk, off) = self.inode_loc(ino)?;
        let b = self.read(tx, blk)?;
        let raw = b.get(off..off + self.geo.inode_size).ok_or(EUCLEAN)?.to_vec();
        let di = DiskInode { ino, raw };
        if !di.csum_ok(&self.geo) {
            klog::event!(Warn, "ext4", "inode.badcrc", ino = ino);
            return Err(EBADMSG);
        }
        if di.extra_isize() > 0 && (128 + di.extra_isize() > self.geo.inode_size || di.extra_isize() % 4 != 0) {
            klog::event!(Warn, "ext4", "inode.bad_extra_isize", ino = ino, extra = di.extra_isize());
            return Err(EUCLEAN);
        }
        Ok(di)
    }
    /// Writes `di` (sealed with its checksum) into its inode-table block in `tx`, and into the node.
    pub fn put_inode(&mut self, tx: &mut Tx, di: &mut DiskInode) -> Result<(), i64> {
        if di.size() > 0x7FFF_FFFF && g32(&self.sb, sb::FEATURE_RO_COMPAT) & sb::ro::LARGE_FILE == 0 {
            // `ext4_do_update_inode` (linux-6.6/fs/ext4/inode.c:5088-5118): the first size past 2 GiB sets
            // large_file in the same change (sync with it here: the superblock goes in this transaction).
            self.save_sb(tx);
            let f = g32(&self.sb, sb::FEATURE_RO_COMPAT) | sb::ro::LARGE_FILE;
            p32(&mut self.sb, sb::FEATURE_RO_COMPAT, f);
            tx.sb_dirty = true;
            klog::event!(Info, "ext4", "feature.large_file", ino = di.ino);
        }
        di.bump_version();
        di.seal(&self.geo);
        let (blk, off) = self.inode_loc(di.ino)?;
        let mut b = self.read(Some(tx), blk)?;
        b.get_mut(off..off + self.geo.inode_size).ok_or(EUCLEAN)?.copy_from_slice(&di.raw);
        tx.put(blk, b);
        if let Some(n) = self.nodes.get_mut(&di.ino) {
            tx.undo_nodes.entry(di.ino).or_insert_with(|| n.di.raw.clone());
            n.di = di.clone();
        }
        Ok(())
    }

    pub fn free_blocks(&self) -> u64 {
        lohi(g32(&self.sb, sb::FREE_BLOCKS_LO), if self.geo.is64() { g32(&self.sb, sb::FREE_BLOCKS_HI) } else { 0 })
    }
    pub fn free_inodes(&self) -> u32 {
        g32(&self.sb, sb::FREE_INODES)
    }
    /// Notes the superblock and group `g`'s descriptor before a change (for `abort`).
    pub fn save_sb(&self, tx: &mut Tx) {
        if tx.undo_sb.is_none() {
            tx.undo_sb = Some(self.sb.clone());
        }
    }
    pub fn save_gd(&self, tx: &mut Tx, g: u32) {
        tx.undo_gd.entry(g).or_insert_with(|| self.gd.desc(g).to_vec());
    }
    /// Forgets a transaction that will not be committed: the in-memory superblock, descriptors and inodes
    /// are as before it (the disk never saw it).
    pub fn abort(&mut self, tx: Tx) {
        if let Some(s) = tx.undo_sb {
            self.sb = s;
        }
        for (g, d) in tx.undo_gd {
            self.gd.desc_mut(g).copy_from_slice(&d);
        }
        for (ino, raw) in tx.undo_nodes {
            if let Some(n) = self.nodes.get_mut(&ino) {
                n.di.raw = raw;
            }
        }
    }
    /// A failed write leaves memory ahead of the disk: no more writes until e2fsck (Linux errors=remount-ro).
    pub fn failed(&mut self, e: i64) -> i64 {
        if self.writable {
            self.writable = false;
            klog::event!(Error, "ext4", "write.failed", errno = e; "volume now read-only (errors=remount-ro)");
        }
        e
    }
    pub fn set_free(&mut self, tx: &mut Tx, blocks: u64, inodes: u32) {
        self.save_sb(tx);
        self.sb = sb::with_counts(&self.sb, &self.geo, blocks, inodes);
        tx.sb_dirty = true;
    }
    /// Group `g`'s descriptor changed: reseal it and mark its table block.
    pub fn gd_changed(&mut self, tx: &mut Tx, g: u32) {
        self.gd.seal(&self.geo, g);
        tx.gd_dirty.insert(g as u64 / self.geo.descs_per_block());
    }

    /// Runs one change in a transaction of its own: committed when `f` succeeds, forgotten when it fails.
    pub fn with_tx<T>(&mut self, f: impl FnOnce(&mut State, &mut Tx) -> Result<T, i64>) -> Result<T, i64> {
        let mut tx = Tx::new();
        match f(self, &mut tx) {
            Ok(v) => {
                self.commit(tx)?;
                Ok(v)
            }
            Err(e) => {
                self.abort(tx);
                Err(e)
            }
        }
    }

    /// Writes a transaction (see `tx`): joined to the running journal transaction, or (no journal) to the
    /// blocks' home places.
    pub fn commit(&mut self, mut tx: Tx) -> Result<(), i64> {
        if tx.is_empty() {
            return Ok(());
        }
        if !self.writable {
            return Err(EROFS);
        }
        if self.frozen {
            klog::event!(Warn, "ext4", "commit.frozen"; "a change reached a frozen volume; refused");
            self.abort(tx);
            return Err(EBUSY);
        }
        let before = if self.dirty { None } else { tx.undo_sb.clone() };
        let r = self.ensure_dirty_from(before).and_then(|()| if self.jnl.is_some() { self.join_tx(&mut tx) } else { self.commit_inner(&mut tx) });
        r.map_err(|e| self.failed(e))
    }
    /// The blocks of `tx` in FS-8a's order (early, descriptors, superblock, the rest).
    fn tx_blocks(&mut self, tx: &mut Tx, sb_block: bool) -> Result<Vec<(u64, Vec<u8>)>, i64> {
        let (early, late) = tx.take();
        let mut out = early;
        for nr in core::mem::take(&mut tx.gd_dirty) {
            let dpb = self.geo.descs_per_block() as usize;
            let ds = self.geo.desc_size;
            let first = nr as usize * dpb * ds;
            // Unused tail of the last table block: as it was on disk (mkfs leaves zeros).
            let mut blk = vec![0u8; self.bs()];
            let end = (first + dpb * ds).min(self.gd.raw.len());
            if first < end {
                blk[..end - first].copy_from_slice(&self.gd.raw[first..end]);
            }
            out.push((gd::desc_block(&self.geo, nr), blk));
        }
        if tx.sb_dirty && sb_block {
            self.stamp_super();
            out.push(self.sb_block()?);
        }
        out.extend(late);
        Ok(out)
    }
    fn join_tx(&mut self, tx: &mut Tx) -> Result<(), i64> {
        let blocks = self.tx_blocks(tx, true)?;
        let freed = core::mem::take(&mut tx.freed);
        self.join(blocks, freed)
    }
    fn commit_inner(&mut self, tx: &mut Tx) -> Result<(), i64> {
        let late = tx.take_late();
        let blocks = self.tx_blocks(tx, false)?;
        for (b, d) in &blocks {
            self.write_blocks(*b, d)?;
        }
        if tx.sb_dirty {
            self.write_super()?;
        }
        for (b, d) in &late {
            self.write_blocks(*b, d)?;
        }
        Ok(())
    }
    /// Before the first change after a mount or a clean sync: the superblock on disk says "not clean" (and
    /// "needs recovery" with a journal), as of `before` (the superblock before the change being committed)
    /// when given, so the disk never holds counts ahead of its bitmaps.
    pub fn ensure_dirty(&mut self) -> Result<(), i64> {
        self.ensure_dirty_from(None)
    }
    pub fn ensure_dirty_from(&mut self, before: Option<Vec<u8>>) -> Result<(), i64> {
        if self.dirty || !self.writable {
            return Ok(());
        }
        let t = self.now().sec;
        let journal = self.jnl.is_some();
        let mark = |sb: &mut Vec<u8>| {
            let s = g16(sb, sb::STATE) & !sb::STATE_VALID;
            p16(sb, sb::STATE, s);
            p32(sb, sb::MTIME, t as u32);
            let mc = g16(sb, sb::MNT_COUNT).wrapping_add(1);
            p16(sb, sb::MNT_COUNT, mc);
            if journal {
                let f = g32(sb, sb::FEATURE_INCOMPAT) | sb::incompat::RECOVER;
                p32(sb, sb::FEATURE_INCOMPAT, f);
            }
        };
        mark(&mut self.sb);
        match before {
            Some(mut b) => {
                mark(&mut b);
                self.write_sb_image(&mut b)?;
            }
            None => self.write_super()?,
        }
        self.dirty = true;
        Ok(())
    }
    /// The write time and bytes written, into the in-memory superblock (sealed).
    fn stamp_super(&mut self) {
        let t = self.now().sec;
        p32(&mut self.sb, sb::WTIME, t as u32);
        p8(&mut self.sb, 0x274, (t >> 32) as u8);
        let kb = g64(&self.sb, sb::KBYTES_WRITTEN) + self.written / 1024;
        self.written %= 1024;
        p64(&mut self.sb, sb::KBYTES_WRITTEN, kb);
        sb::set_checksum(&mut self.sb, &self.geo);
    }
    /// The primary superblock (byte 1024) written directly, with the time of this write and the bytes
    /// written (state changes outside a transaction: dirty/clean marks).
    pub fn write_super(&mut self) -> Result<(), i64> {
        self.stamp_super();
        let s = self.sb.clone();
        self.dev.write(sb::OFFSET, &s)
    }
    fn write_sb_image(&mut self, img: &mut [u8]) -> Result<(), i64> {
        self.stamp_super();
        for f in [sb::WTIME, sb::KBYTES_WRITTEN, sb::KBYTES_WRITTEN + 4] {
            let v = g32(&self.sb, f);
            p32(img, f, v);
        }
        p8(img, 0x274, g8(&self.sb, 0x274));
        sb::set_checksum(img, &self.geo);
        self.dev.write(sb::OFFSET, img)
    }
}

impl State {
    /// Promises blocks for the unmapped blocks of file pages [first, last] (see `Volume::reserve`).
    pub fn reserve_pages(&mut self, ino: u32, first: u64, last: u64) -> Result<(), i64> {
        let bs = self.geo.bs;
        let bpp = (4096 / bs).max(1);
        let Some(n) = self.nodes.get(&ino) else { return Ok(()) };
        let di = n.di.clone();
        let mapped_end = di.size().div_ceil(4096);
        let mut add: Vec<(u64, u32)> = Vec::new();
        for p in first..=last {
            if n.reserved.contains_key(&p) {
                continue;
            }
            let mut need = 0u32;
            for k in 0..bpp {
                let l = p * bpp + k;
                let mapped = p < mapped_end && l < 1 << 32 && matches!(crate::extent::lookup(self, None, &di, l as u32)?, crate::extent::Found::Ext(_));
                if !mapped {
                    need += 1;
                }
            }
            if need > 0 {
                add.push((p, need));
            }
        }
        let total: u64 = add.iter().map(|a| a.1 as u64).sum();
        if total == 0 {
            return Ok(());
        }
        // The root reserve is for root (and s_def_resuid/resgid) only; extent-tree blocks a write-back may
        // need come out of it for anyone.
        // The caller's credentials are asked only when the reserve decides (not on every write).
        if self.free_blocks() < self.keep_for(None) + total {
            let who = vfs::inode::current_cred().map(|c| (c.uid, c.gid));
            if who.is_none() || self.free_blocks() < self.keep_for(who) + total {
                return Err(ENOSPC);
            }
        }
        self.reserved += total;
        if let Some(n) = self.nodes.get_mut(&ino) {
            n.reserved.extend(add);
        }
        Ok(())
    }

    /// Free blocks a metadata allocation for `who` (uid, gid; None: not known, treated as a user) must leave:
    /// the blocks promised to dirty pages, and the root reserve unless `who` may use it (root, `s_def_resuid`
    /// or `s_def_resgid`: `ext4_has_free_clusters`, linux-6.6/fs/ext4/balloc.c:612).
    pub fn keep_for(&self, who: Option<(u32, u32)>) -> u64 {
        let (resuid, resgid) = (g16(&self.sb, sb::DEF_RESUID) as u32, g16(&self.sb, sb::DEF_RESGID) as u32);
        let root = who.is_some_and(|(u, g)| u == 0 || u == resuid || (resgid != 0 && g == resgid));
        self.reserved + if root { 0 } else { crate::sb::reserved_blocks(&self.sb, &self.geo) }
    }

    /// Gives back the promises for pages in `pages` (written now, or truncated away); returns the blocks.
    pub fn release_reserved(&mut self, ino: u32, pages: core::ops::Range<u64>) -> u64 {
        let Some(n) = self.nodes.get_mut(&ino) else { return 0 };
        let keys: Vec<u64> = n.reserved.range(pages).map(|(k, _)| *k).collect();
        let mut sum = 0u64;
        for k in keys {
            sum += n.reserved.remove(&k).unwrap_or(0) as u64;
        }
        self.reserved = self.reserved.saturating_sub(sum);
        sum
    }
}
