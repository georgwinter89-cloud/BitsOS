//! Group descriptors (linux-6.6/fs/ext4/ext4.h:390 `struct ext4_group_desc`): the 32-byte and 64-byte layouts,
//! their checksums (`ext4_group_desc_csum`, linux-6.6/fs/ext4/super.c:3224) and where the table lives
//! (`descriptor_loc`, super.c:3530, including meta_bg).

use crate::crc::{crc16, crc32c};
use crate::le::*;
use crate::sb::{ro, Geo};
use alloc::vec::Vec;
use crate::errno::*;

pub const INODE_UNINIT: u16 = 1;
pub const BLOCK_UNINIT: u16 = 2;
pub const ITABLE_ZEROED: u16 = 4;

const CSUM: usize = 0x1E;

/// The whole table in memory, `desc_size` bytes per group.
pub struct Table {
    pub raw: Vec<u8>,
    pub ds: usize,
    pub is64: bool,
}

fn split(t: &Table, g: u32, lo: usize, hi: usize, w: usize) -> u64 {
    let d = t.desc(g);
    let l = if w == 4 { g32(d, lo) as u64 } else { g16(d, lo) as u64 };
    if !t.is64 {
        return l;
    }
    let h = if w == 4 { g32(d, hi) as u64 } else { g16(d, hi) as u64 };
    l | h << (w * 8)
}

fn put_split(t: &mut Table, g: u32, lo: usize, hi: usize, w: usize, v: u64) {
    let is64 = t.is64;
    let d = t.desc_mut(g);
    if w == 4 {
        p32(d, lo, v as u32);
        if is64 {
            p32(d, hi, (v >> 32) as u32);
        }
    } else {
        p16(d, lo, v as u16);
        if is64 {
            p16(d, hi, (v >> 16) as u16);
        }
    }
}

impl Table {
    pub fn desc(&self, g: u32) -> &[u8] {
        let o = g as usize * self.ds;
        self.raw.get(o..o + self.ds).unwrap_or(&[])
    }
    pub fn desc_mut(&mut self, g: u32) -> &mut [u8] {
        let o = g as usize * self.ds;
        let ds = self.ds;
        self.raw.get_mut(o..o + ds).unwrap_or(&mut [])
    }
    pub fn block_bitmap(&self, g: u32) -> u64 {
        split(self, g, 0x00, 0x20, 4)
    }
    pub fn inode_bitmap(&self, g: u32) -> u64 {
        split(self, g, 0x04, 0x24, 4)
    }
    pub fn inode_table(&self, g: u32) -> u64 {
        split(self, g, 0x08, 0x28, 4)
    }
    pub fn free_blocks(&self, g: u32) -> u64 {
        split(self, g, 0x0C, 0x2C, 2)
    }
    pub fn set_free_blocks(&mut self, g: u32, v: u64) {
        put_split(self, g, 0x0C, 0x2C, 2, v)
    }
    pub fn free_inodes(&self, g: u32) -> u64 {
        split(self, g, 0x0E, 0x2E, 2)
    }
    pub fn set_free_inodes(&mut self, g: u32, v: u64) {
        put_split(self, g, 0x0E, 0x2E, 2, v)
    }
    pub fn used_dirs(&self, g: u32) -> u64 {
        split(self, g, 0x10, 0x30, 2)
    }
    pub fn set_used_dirs(&mut self, g: u32, v: u64) {
        put_split(self, g, 0x10, 0x30, 2, v)
    }
    pub fn flags(&self, g: u32) -> u16 {
        g16(self.desc(g), 0x12)
    }
    pub fn set_flags(&mut self, g: u32, f: u16) {
        p16(self.desc_mut(g), 0x12, f)
    }
    pub fn itable_unused(&self, g: u32) -> u64 {
        split(self, g, 0x1C, 0x32, 2)
    }
    pub fn set_itable_unused(&mut self, g: u32, v: u64) {
        put_split(self, g, 0x1C, 0x32, 2, v)
    }
    /// Stores a bitmap checksum (`ext4_block_bitmap_csum_set`/`ext4_inode_bitmap_csum_set`, bitmap.c).
    pub fn set_bitmap_csum(&mut self, g: u32, inode: bool, c: u32) {
        let (lo, hi) = if inode { (0x1A, 0x3A) } else { (0x18, 0x38) };
        let wide = self.ds >= hi + 2;
        let d = self.desc_mut(g);
        p16(d, lo, c as u16);
        if wide {
            p16(d, hi, (c >> 16) as u16);
        }
    }
    pub fn bitmap_csum_ok(&self, g: u32, inode: bool, c: u32) -> bool {
        let (lo, hi) = if inode { (0x1A, 0x3A) } else { (0x18, 0x38) };
        let d = self.desc(g);
        if g16(d, lo) != c as u16 {
            return false;
        }
        self.ds < hi + 2 || g16(d, hi) == (c >> 16) as u16
    }
    pub fn checksum(&self, geo: &Geo, g: u32) -> u16 {
        let d = self.desc(g);
        let le = g.to_le_bytes();
        if let Some(seed) = geo.csum {
            let mut c = crc32c(seed, &le);
            c = crc32c(c, &d[..CSUM]);
            c = crc32c(c, &[0, 0]);
            c = crc32c(c, &d[CSUM + 2..]);
            return c as u16;
        }
        if !geo.has_ro(ro::GDT_CSUM) {
            return 0;
        }
        let mut c = crc16(!0, &geo.uuid);
        c = crc16(c, &le);
        c = crc16(c, &d[..CSUM]);
        if geo.is64() {
            c = crc16(c, &d[CSUM + 2..]);
        }
        c
    }
    pub fn has_csum(geo: &Geo) -> bool {
        geo.csum.is_some() || geo.has_ro(ro::GDT_CSUM)
    }
    pub fn csum_ok(&self, geo: &Geo, g: u32) -> bool {
        !Self::has_csum(geo) || g16(self.desc(g), CSUM) == self.checksum(geo, g)
    }
    pub fn seal(&mut self, geo: &Geo, g: u32) {
        if Self::has_csum(geo) {
            let c = self.checksum(geo, g);
            p16(self.desc_mut(g), CSUM, c);
        }
    }
}

/// The block holding descriptor block `nr` of the table (`descriptor_loc`).
pub fn desc_block(geo: &Geo, nr: u64) -> u64 {
    use crate::sb::incompat::META_BG;
    if !geo.has_in(META_BG) || nr < geo.first_meta_bg as u64 {
        return geo.first_data_block + nr + 1;
    }
    let bg = (geo.descs_per_block() * nr) as u32;
    geo.group_start(bg) + geo.has_super(bg) as u64
}

/// Inode-table blocks per group.
pub fn itb_per_group(geo: &Geo) -> u64 {
    (geo.ipg as u64 * geo.inode_size as u64).div_ceil(geo.bs)
}

/// Blocks at the start of group `g` that hold the superblock copy, descriptor blocks and reserved GDT
/// (`ext4_num_base_meta_blocks`, linux-6.6/fs/ext4/balloc.c:919).
pub fn base_meta_blocks(geo: &Geo, g: u32) -> u64 {
    use crate::sb::incompat::META_BG;
    let sup = geo.has_super(g) as u64;
    let dpb = geo.descs_per_block() as u32;
    if !geo.has_in(META_BG) || (g as u64) < geo.first_meta_bg as u64 * dpb as u64 {
        if sup == 0 {
            return 0;
        }
        let gdb = if geo.has_in(META_BG) { geo.first_meta_bg as u64 } else { geo.gdt_blocks() };
        sup + gdb + geo.reserved_gdt
    } else {
        let first = g / dpb * dpb;
        sup + (g == first || g == first + 1 || g == first + dpb - 1) as u64
    }
}

/// Checks that every group's bitmaps and inode table lie inside the volume (`ext4_check_descriptors`).
pub fn check(t: &Table, geo: &Geo) -> Result<(), i64> {
    let itb = itb_per_group(geo);
    for g in 0..geo.groups {
        if !t.csum_ok(geo, g) {
            klog::event!(Warn, "ext4", "gd.badcrc", group = g);
            return Err(EBADMSG);
        }
        let inside = |b: u64, n: u64| b >= geo.first_data_block && b.checked_add(n).is_some_and(|e| e <= geo.blocks);
        if !inside(t.block_bitmap(g), 1) || !inside(t.inode_bitmap(g), 1) || !inside(t.inode_table(g), itb) {
            klog::event!(Warn, "ext4", "gd.corrupt", group = g, bb = t.block_bitmap(g), ib = t.inode_bitmap(g), it = t.inode_table(g));
            return Err(EUCLEAN);
        }
    }
    Ok(())
}
