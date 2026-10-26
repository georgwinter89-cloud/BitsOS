//! Block and inode bitmaps (with their checksums and the `BLOCK_UNINIT`/`INODE_UNINIT` groups mkfs leaves,
//! linux-6.6/fs/ext4/balloc.c:183 `ext4_init_block_bitmap`, ialloc.c `ext4_read_inode_bitmap`) and the
//! allocators: blocks near a goal (the file's last block, else its inode's group), inodes in the parent's
//! group for files and in the emptiest group for directories (a simple form of Linux's Orlov spreading).

use crate::crc::crc32c;
use crate::gd::{self, BLOCK_UNINIT, INODE_UNINIT};
use crate::state::State;
use crate::tx::Tx;
use alloc::vec;
use alloc::vec::Vec;
use crate::errno::*;

fn bit(b: &[u8], i: u64) -> bool {
    b.get((i / 8) as usize).is_some_and(|x| x >> (i % 8) & 1 != 0)
}
fn set(b: &mut [u8], i: u64, on: bool) {
    if let Some(x) = b.get_mut((i / 8) as usize) {
        if on {
            *x |= 1 << (i % 8)
        } else {
            *x &= !(1 << (i % 8))
        }
    }
}
/// Sets every bit from `from` to the end of the block (`ext4_mark_bitmap_end`).
fn mark_end(b: &mut [u8], from: u64) {
    for i in from..b.len() as u64 * 8 {
        set(b, i, true);
    }
}

impl State {
    fn bitmap_csum(&self, b: &[u8], bits: u64) -> Option<u32> {
        let seed = self.geo.csum?;
        Some(crc32c(seed, &b[..((bits / 8) as usize).min(b.len())]))
    }

    /// Group `g`'s block bitmap (built as Linux builds it for an uninitialised group).
    pub fn block_bitmap(&self, tx: Option<&Tx>, g: u32) -> Result<Vec<u8>, i64> {
        let geo = &self.geo;
        let in_group = geo.blocks_in(g);
        if self.gd.flags(g) & BLOCK_UNINIT != 0 && gd::Table::has_csum(geo) {
            let blk = self.gd.block_bitmap(g);
            if let Some(d) = tx.and_then(|t| t.get(blk)) {
                return Ok(d.to_vec());
            }
            let mut b = vec![0u8; self.bs()];
            for i in 0..gd::base_meta_blocks(geo, g).min(in_group) {
                set(&mut b, i, true);
            }
            let start = geo.group_start(g);
            let itb = gd::itb_per_group(geo);
            let mine = [(self.gd.block_bitmap(g), 1), (self.gd.inode_bitmap(g), 1), (self.gd.inode_table(g), itb)];
            for (first, n) in mine {
                for x in first..first + n {
                    if x >= start && x < start + in_group {
                        set(&mut b, x - start, true);
                    }
                }
            }
            mark_end(&mut b, in_group);
            return Ok(b);
        }
        let b = self.read(tx, self.gd.block_bitmap(g))?;
        if tx.and_then(|t| t.get(self.gd.block_bitmap(g))).is_none()
            && let Some(c) = self.bitmap_csum(&b, geo.bpg)
            && !self.gd.bitmap_csum_ok(g, false, c)
        {
            klog::event!(Warn, "ext4", "bitmap.badcrc", group = g, kind = "block");
            return Err(EBADMSG);
        }
        Ok(b)
    }

    pub fn inode_bitmap(&self, tx: Option<&Tx>, g: u32) -> Result<Vec<u8>, i64> {
        let blk = self.gd.inode_bitmap(g);
        if self.gd.flags(g) & INODE_UNINIT != 0 && gd::Table::has_csum(&self.geo) {
            if let Some(d) = tx.and_then(|t| t.get(blk)) {
                return Ok(d.to_vec());
            }
            let mut b = vec![0u8; self.bs()];
            mark_end(&mut b, self.geo.ipg as u64);
            return Ok(b);
        }
        let b = self.read(tx, blk)?;
        if tx.and_then(|t| t.get(blk)).is_none()
            && let Some(c) = self.bitmap_csum(&b, self.geo.ipg as u64)
            && !self.gd.bitmap_csum_ok(g, true, c)
        {
            klog::event!(Warn, "ext4", "bitmap.badcrc", group = g, kind = "inode");
            return Err(EBADMSG);
        }
        Ok(b)
    }

    fn put_bitmap(&mut self, tx: &mut Tx, g: u32, inode: bool, b: Vec<u8>, early: bool) {
        let bits = if inode { self.geo.ipg as u64 } else { self.geo.bpg };
        if let Some(c) = self.bitmap_csum(&b, bits) {
            self.gd.set_bitmap_csum(g, inode, c);
        }
        let blk = if inode { self.gd.inode_bitmap(g) } else { self.gd.block_bitmap(g) };
        let f = self.gd.flags(g) & !if inode { INODE_UNINIT } else { BLOCK_UNINIT };
        self.gd.set_flags(g, f);
        self.gd_changed(tx, g);
        if early { tx.put_early(blk, b) } else { tx.put(blk, b) }
    }

    /// Up to `want` free blocks in one run, the first at or after `goal` if possible (then anywhere).
    /// Marked used in `tx` (early). `keep` blocks stay for others (the reserved-for-root count).
    pub fn alloc_blocks(&mut self, tx: &mut Tx, goal: u64, want: u64, keep: u64) -> Result<(u64, u64), i64> {
        if want == 0 {
            return Err(EINVAL);
        }
        if self.free_blocks() <= keep {
            return Err(ENOSPC);
        }
        let want = want.min(self.free_blocks() - keep).min(self.geo.bpg);
        let geo = &self.geo;
        let goal = if goal >= geo.first_data_block && goal < geo.blocks { goal } else { geo.first_data_block };
        let g0 = geo.group_of_block(goal);
        let n = geo.groups;
        for k in 0..=n {
            let g = (g0 + k) % n;
            if self.gd.free_blocks(g) == 0 {
                continue;
            }
            let mut b = self.block_bitmap(Some(tx), g)?;
            let in_group = self.geo.blocks_in(g);
            let base = self.geo.group_start(g);
            // Blocks freed by a transaction the journal has not committed yet are not handed out (`busy`).
            let busy = self.busy_in(tx, base, base + in_group);
            let real = if busy.is_empty() { None } else { Some(b.clone()) };
            for (s, e) in busy {
                for x in s.max(base)..e.min(base + in_group) {
                    set(&mut b, x - base, true);
                }
            }
            let from = if k == 0 { goal - self.geo.group_start(g) } else { 0 };
            let Some((first, len)) = find_run(&b, from, in_group, want).or_else(|| if k == 0 && from > 0 { find_run(&b, 0, in_group, want) } else { None }) else {
                continue;
            };
            let mut b = real.unwrap_or(b);
            for i in first..first + len {
                set(&mut b, i, true);
            }
            self.save_gd(tx, g);
            self.put_bitmap(tx, g, false, b, true);
            let fg = self.gd.free_blocks(g).saturating_sub(len);
            self.gd.set_free_blocks(g, fg);
            self.gd_changed(tx, g);
            let (fb, fi) = (self.free_blocks() - len, self.free_inodes());
            self.set_free(tx, fb, fi);
            return Ok((self.geo.group_start(g) + first, len));
        }
        if self.jnl.as_ref().is_some_and(|j| !j.run.busy.is_empty()) {
            // Only blocks freed in the running journal transaction are left: commit it, then they are free.
            klog::event!(Info, "ext4", "alloc.commit_for_busy", want = want);
            self.jcommit()?;
            return self.alloc_blocks(tx, goal, want, keep);
        }
        klog::event!(Warn, "ext4", "alloc.nospace", free = self.free_blocks(); "free count says space, bitmaps none");
        Err(ENOSPC)
    }

    /// Frees blocks [first, first + n) (they may cross groups). Late in `tx`: after the pointers went.
    pub fn free_blocks_range(&mut self, tx: &mut Tx, first: u64, n: u64) -> Result<(), i64> {
        let mut b0 = first;
        let end = first.checked_add(n).ok_or(EUCLEAN)?;
        if first < self.geo.first_data_block || end > self.geo.blocks {
            klog::event!(Warn, "ext4", "free.out_of_range", first = first, n = n);
            return Err(EUCLEAN);
        }
        tx.freed.push((first, n));
        while b0 < end {
            let g = self.geo.group_of_block(b0);
            let start = self.geo.group_start(g);
            let stop = end.min(start + self.geo.blocks_in(g));
            let mut b = self.block_bitmap(Some(tx), g)?;
            let mut freed = 0;
            for x in b0..stop {
                if bit(&b, x - start) {
                    set(&mut b, x - start, false);
                    freed += 1;
                } else {
                    klog::event!(Warn, "ext4", "free.double", block = x);
                }
            }
            self.save_gd(tx, g);
            self.put_bitmap(tx, g, false, b, false);
            let fg = self.gd.free_blocks(g) + freed;
            self.gd.set_free_blocks(g, fg);
            self.gd_changed(tx, g);
            let (fb, fi) = (self.free_blocks() + freed, self.free_inodes());
            self.set_free(tx, fb, fi);
            b0 = stop;
        }
        Ok(())
    }

    /// A free inode: directories in the group with the most free inodes and blocks among those with at least
    /// the average free inodes, other types in the parent's group, then the next ones.
    pub fn alloc_inode(&mut self, tx: &mut Tx, parent: u32, dir: bool) -> Result<u32, i64> {
        if self.free_inodes() == 0 {
            return Err(ENOSPC);
        }
        let n = self.geo.groups;
        let pg = (parent.max(1) - 1) / self.geo.ipg;
        let start = if dir {
            let avg = self.free_inodes() as u64 / n as u64;
            (0..n).filter(|&g| self.gd.free_inodes(g) >= avg.max(1)).max_by_key(|&g| (self.gd.free_blocks(g), u32::MAX - g)).unwrap_or(pg)
        } else {
            pg
        };
        for k in 0..n {
            let g = (start + k) % n;
            if self.gd.free_inodes(g) == 0 {
                continue;
            }
            let mut b = self.inode_bitmap(Some(tx), g)?;
            let first = if g == 0 { self.geo.first_ino as u64 - 1 } else { 0 };
            let Some(i) = (first..self.geo.ipg as u64).find(|&i| !bit(&b, i)) else { continue };
            set(&mut b, i, true);
            let was_uninit = self.gd.flags(g) & INODE_UNINIT != 0;
            self.save_gd(tx, g);
            self.put_bitmap(tx, g, true, b, true);
            self.gd.set_free_inodes(g, self.gd.free_inodes(g).saturating_sub(1));
            if dir {
                self.gd.set_used_dirs(g, self.gd.used_dirs(g) + 1);
            }
            if gd::Table::has_csum(&self.geo) {
                // `ext4_new_inode`: the unused tail of the inode table starts after the highest used inode.
                let unused = if was_uninit { self.geo.ipg as u64 } else { self.gd.itable_unused(g) };
                let used_end = self.geo.ipg as u64 - unused;
                if i + 1 > used_end {
                    self.gd.set_itable_unused(g, self.geo.ipg as u64 - (i + 1));
                }
            }
            self.gd_changed(tx, g);
            let (fb, fi) = (self.free_blocks(), self.free_inodes() - 1);
            self.set_free(tx, fb, fi);
            return Ok(g * self.geo.ipg + i as u32 + 1);
        }
        klog::event!(Warn, "ext4", "ialloc.nospace", free = self.free_inodes(); "free count says inodes, bitmaps none");
        Err(ENOSPC)
    }

    pub fn free_inode(&mut self, tx: &mut Tx, ino: u32, dir: bool) -> Result<(), i64> {
        let g = (ino - 1) / self.geo.ipg;
        let i = ((ino - 1) % self.geo.ipg) as u64;
        let mut b = self.inode_bitmap(Some(tx), g)?;
        if !bit(&b, i) {
            klog::event!(Warn, "ext4", "ifree.double", ino = ino);
            return Ok(());
        }
        set(&mut b, i, false);
        self.save_gd(tx, g);
        self.put_bitmap(tx, g, true, b, false);
        self.gd.set_free_inodes(g, self.gd.free_inodes(g) + 1);
        if dir {
            self.gd.set_used_dirs(g, self.gd.used_dirs(g).saturating_sub(1));
        }
        self.gd_changed(tx, g);
        let (fb, fi) = (self.free_blocks(), self.free_inodes() + 1);
        self.set_free(tx, fb, fi);
        Ok(())
    }

    /// Whether inode `ino` is marked used.
    pub fn inode_in_use(&self, ino: u32) -> Result<bool, i64> {
        if ino == 0 || ino > self.geo.inodes {
            return Ok(false);
        }
        let g = (ino - 1) / self.geo.ipg;
        Ok(bit(&self.inode_bitmap(None, g)?, ((ino - 1) % self.geo.ipg) as u64))
    }
}

/// The first run of free bits in [from, end): the longest up to `want` starting at the first free bit.
impl State {
    /// Busy ranges overlapping [start, end): freed in the running journal transaction or in `tx`.
    fn busy_in(&self, tx: &Tx, start: u64, end: u64) -> Vec<(u64, u64)> {
        let mut v: Vec<(u64, u64)> = tx.freed.iter().map(|&(s, n)| (s, s + n)).filter(|&(s, e)| s < end && e > start).collect();
        if let Some(j) = self.jnl.as_ref() {
            v.extend(j.run.busy_in(start, end));
        }
        v
    }
}

fn find_run(b: &[u8], from: u64, end: u64, want: u64) -> Option<(u64, u64)> {
    let mut i = from;
    while i < end {
        if b.get((i / 8) as usize) == Some(&0xFF) && i % 8 == 0 {
            i += 8;
            continue;
        }
        if !bit(b, i) {
            let mut n = 1;
            while n < want && i + n < end && !bit(b, i + n) {
                n += 1;
            }
            return Some((i, n));
        }
        i += 1;
    }
    None
}
