//! One on-disk inode (linux-6.6/fs/ext4/ext4.h:766 `struct ext4_inode`), kept as its raw bytes so fields V2
//! does not use survive a rewrite: owner, times with nanoseconds and epoch bits, sizes above 4 GiB, block
//! counts (huge_file), flags, the i_version counter and the checksum (`ext4_inode_csum`, inode.c:52).

use crate::crc::crc32c;
use crate::le::*;
use crate::sb::{ro, Geo};
use alloc::vec::Vec;
use vfs::abi::Time;

pub const ROOT: u32 = 2;

pub const S_IFMT: u16 = 0o170000;
pub const S_IFREG: u16 = 0o100000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFLNK: u16 = 0o120000;
pub const S_IFCHR: u16 = 0o020000;
pub const S_IFBLK: u16 = 0o060000;
pub const S_IFIFO: u16 = 0o010000;
pub const S_IFSOCK: u16 = 0o140000;

pub const FL_INDEX: u32 = 0x1000;
pub const FL_HUGE_FILE: u32 = 0x40000;
pub const FL_EXTENTS: u32 = 0x80000;
pub const FL_EA_INODE: u32 = 0x200000;
pub const FL_INLINE_DATA: u32 = 0x1000_0000;
pub const FL_PROJINHERIT: u32 = 0x2000_0000;
const FL_SYNC: u32 = 0x8;
const FL_NODUMP: u32 = 0x40;
const FL_NOATIME: u32 = 0x80;
const FL_DIRSYNC: u32 = 0x1_0000;
/// Flags a new inode takes from its directory (`EXT4_FL_INHERITED`, ext4.h:533), without those V2 does not
/// implement the meaning of (compression, data journaling, casefold, DAX); masked per type as
/// `ext4_mask_flags` does (ext4.h:555: directories all, files without DIRSYNC/PROJINHERIT, others two).
const FL_INHERITED: u32 = FL_SYNC | FL_NODUMP | FL_NOATIME | FL_DIRSYNC | FL_PROJINHERIT;

pub const MODE: usize = 0x00;
pub const UID: usize = 0x02;
pub const SIZE_LO: usize = 0x04;
pub const ATIME: usize = 0x08;
pub const CTIME: usize = 0x0C;
pub const MTIME: usize = 0x10;
pub const DTIME: usize = 0x14;
pub const GID: usize = 0x18;
pub const LINKS: usize = 0x1A;
pub const BLOCKS_LO: usize = 0x1C;
pub const FLAGS: usize = 0x20;
pub const VERSION: usize = 0x24;
pub const BLOCK: usize = 0x28;
pub const GENERATION: usize = 0x64;
pub const FILE_ACL_LO: usize = 0x68;
pub const SIZE_HI: usize = 0x6C;
pub const BLOCKS_HI: usize = 0x74;
pub const FILE_ACL_HI: usize = 0x76;
pub const UID_HI: usize = 0x78;
pub const GID_HI: usize = 0x7A;
pub const CSUM_LO: usize = 0x7C;
pub const EXTRA_ISIZE: usize = 0x80;
pub const CSUM_HI: usize = 0x82;
pub const CTIME_X: usize = 0x84;
pub const MTIME_X: usize = 0x88;
pub const ATIME_X: usize = 0x8C;
pub const CRTIME: usize = 0x90;
pub const CRTIME_X: usize = 0x94;
pub const VERSION_HI: usize = 0x98;
pub const PROJID: usize = 0x9C;

#[derive(Clone)]
pub struct DiskInode {
    pub ino: u32,
    pub raw: Vec<u8>,
}

/// The time fields: (seconds, extra) offsets.
#[derive(Clone, Copy)]
pub enum Stamp {
    A,
    M,
    C,
    Cr,
}

impl Stamp {
    fn at(self) -> (usize, usize) {
        match self {
            Stamp::A => (ATIME, ATIME_X),
            Stamp::M => (MTIME, MTIME_X),
            Stamp::C => (CTIME, CTIME_X),
            Stamp::Cr => (CRTIME, CRTIME_X),
        }
    }
}

impl DiskInode {
    pub fn mode(&self) -> u16 {
        g16(&self.raw, MODE)
    }
    pub fn ftype(&self) -> u16 {
        self.mode() & S_IFMT
    }
    pub fn is_dir(&self) -> bool {
        self.ftype() == S_IFDIR
    }
    pub fn is_reg(&self) -> bool {
        self.ftype() == S_IFREG
    }
    pub fn is_symlink(&self) -> bool {
        self.ftype() == S_IFLNK
    }
    pub fn set_mode(&mut self, m: u16) {
        p16(&mut self.raw, MODE, m)
    }
    pub fn uid(&self) -> u32 {
        g16(&self.raw, UID) as u32 | (g16(&self.raw, UID_HI) as u32) << 16
    }
    pub fn gid(&self) -> u32 {
        g16(&self.raw, GID) as u32 | (g16(&self.raw, GID_HI) as u32) << 16
    }
    pub fn set_owner(&mut self, uid: u32, gid: u32) {
        p16(&mut self.raw, UID, uid as u16);
        p16(&mut self.raw, UID_HI, (uid >> 16) as u16);
        p16(&mut self.raw, GID, gid as u16);
        p16(&mut self.raw, GID_HI, (gid >> 16) as u16);
    }
    pub fn size(&self) -> u64 {
        lohi(g32(&self.raw, SIZE_LO), g32(&self.raw, SIZE_HI))
    }
    pub fn set_size(&mut self, s: u64) {
        p32(&mut self.raw, SIZE_LO, s as u32);
        p32(&mut self.raw, SIZE_HI, (s >> 32) as u32);
    }
    pub fn links(&self) -> u16 {
        g16(&self.raw, LINKS)
    }
    pub fn set_links(&mut self, n: u16) {
        p16(&mut self.raw, LINKS, n)
    }
    pub fn flags(&self) -> u32 {
        g32(&self.raw, FLAGS)
    }
    pub fn set_flags(&mut self, f: u32) {
        p32(&mut self.raw, FLAGS, f)
    }
    pub fn dtime(&self) -> u32 {
        g32(&self.raw, DTIME)
    }
    pub fn set_dtime(&mut self, t: u32) {
        p32(&mut self.raw, DTIME, t)
    }
    pub fn generation(&self) -> u32 {
        g32(&self.raw, GENERATION)
    }
    pub fn file_acl(&self) -> u64 {
        g32(&self.raw, FILE_ACL_LO) as u64 | (g16(&self.raw, FILE_ACL_HI) as u64) << 32
    }
    pub fn set_file_acl(&mut self, b: u64) {
        p32(&mut self.raw, FILE_ACL_LO, b as u32);
        p16(&mut self.raw, FILE_ACL_HI, (b >> 32) as u16);
    }
    pub fn extra_isize(&self) -> usize {
        if self.raw.len() > 128 { g16(&self.raw, EXTRA_ISIZE) as usize } else { 0 }
    }
    /// Whether a field ending at `end` lies inside the inode's extra part (`EXT4_FITS_IN_INODE`).
    pub fn fits(&self, end: usize) -> bool {
        self.raw.len() > 128 && end <= 128 + self.extra_isize()
    }
    pub fn i_block(&self) -> &[u8] {
        &self.raw[BLOCK..BLOCK + 60]
    }
    pub fn i_block_mut(&mut self) -> &mut [u8] {
        &mut self.raw[BLOCK..BLOCK + 60]
    }
    /// 512-byte sectors held (`ext4_inode_blocks`, inode.c:4588): fs blocks when HUGE_FILE_FL is set.
    pub fn blocks512(&self, geo: &Geo) -> u64 {
        let lo = g32(&self.raw, BLOCKS_LO) as u64;
        if !geo.has_ro(ro::HUGE_FILE) {
            return lo;
        }
        let n = lo | (g16(&self.raw, BLOCKS_HI) as u64) << 32;
        if self.flags() & FL_HUGE_FILE != 0 { n * (geo.bs / 512) } else { n }
    }
    /// Stores a sector count (`ext4_inode_blocks_set`, inode.c:4202): 32 bits without huge_file, 48 bits in
    /// sectors, else in fs blocks with HUGE_FILE_FL.
    pub fn set_blocks512(&mut self, geo: &Geo, n: u64) -> Result<(), i64> {
        let mut f = self.flags() & !FL_HUGE_FILE;
        let v = if n <= u32::MAX as u64 {
            n
        } else if !geo.has_ro(ro::HUGE_FILE) {
            return Err(crate::errno::EFBIG);
        } else if n < 1 << 48 {
            n
        } else {
            f |= FL_HUGE_FILE;
            n / (geo.bs / 512)
        };
        p32(&mut self.raw, BLOCKS_LO, v as u32);
        p16(&mut self.raw, BLOCKS_HI, (v >> 32) as u16);
        self.set_flags(f);
        Ok(())
    }
    /// A time field: seconds (signed, plus epoch bits) and nanoseconds where the inode has the extra field.
    pub fn time(&self, s: Stamp) -> Time {
        let (b, x) = s.at();
        let mut sec = g32(&self.raw, b) as i32 as i64;
        let mut nsec = 0;
        if self.fits(x + 4) {
            let e = g32(&self.raw, x);
            sec += ((e & 3) as i64) << 32;
            nsec = (e >> 2).min(999_999_999);
        } else if matches!(s, Stamp::Cr) {
            return Time { sec: 0, nsec: 0 };
        }
        Time { sec, nsec }
    }
    /// `ext4_encode_extra_time` (ext4.h:868).
    pub fn set_time(&mut self, s: Stamp, t: Time) {
        let (b, x) = s.at();
        if matches!(s, Stamp::Cr) && !self.fits(x + 4) {
            return;
        }
        p32(&mut self.raw, b, t.sec as i32 as u32);
        if self.fits(x + 4) {
            let epoch = ((t.sec - t.sec as i32 as i64) >> 32) as u32 & 3;
            p32(&mut self.raw, x, epoch | t.nsec.min(999_999_999) << 2);
        }
    }
    /// The i_version change counter (osd1 low, i_version_hi high), bumped on every change V2 writes.
    pub fn bump_version(&mut self) {
        let hi = self.fits(VERSION_HI + 4);
        let v = g32(&self.raw, VERSION) as u64 | if hi { (g32(&self.raw, VERSION_HI) as u64) << 32 } else { 0 };
        let v = v.wrapping_add(1);
        p32(&mut self.raw, VERSION, v as u32);
        if hi {
            p32(&mut self.raw, VERSION_HI, (v >> 32) as u32);
        }
    }
    pub fn projid(&self) -> u32 {
        if self.fits(PROJID + 4) { g32(&self.raw, PROJID) } else { 0 }
    }
    /// The seed for this inode's checksums and those of its extent and directory blocks (`i_csum_seed`).
    pub fn csum_seed(&self, geo: &Geo) -> Option<u32> {
        let s = geo.csum?;
        Some(crc32c(crc32c(s, &self.ino.to_le_bytes()), &self.generation().to_le_bytes()))
    }
    fn checksum(&self, geo: &Geo) -> Option<u32> {
        let seed = self.csum_seed(geo)?;
        let mut r = self.raw.clone();
        p16(&mut r, CSUM_LO, 0);
        if self.fits(CSUM_HI + 2) {
            p16(&mut r, CSUM_HI, 0);
        }
        Some(crc32c(seed, &r))
    }
    pub fn csum_ok(&self, geo: &Geo) -> bool {
        let Some(c) = self.checksum(geo) else { return true };
        if g16(&self.raw, CSUM_LO) != c as u16 {
            return false;
        }
        !self.fits(CSUM_HI + 2) || g16(&self.raw, CSUM_HI) == (c >> 16) as u16
    }
    pub fn seal(&mut self, geo: &Geo) {
        if let Some(c) = self.checksum(geo) {
            p16(&mut self.raw, CSUM_LO, c as u16);
            if self.fits(CSUM_HI + 2) {
                p16(&mut self.raw, CSUM_HI, (c >> 16) as u16);
            }
        }
    }
    /// A fresh inode (`__ext4_new_inode` + `ext4_init_new_inode`): owner, mode, all times now, extents root.
    pub fn fresh(geo: &Geo, ino: u32, mode: u16, uid: u32, gid: u32, parent_flags: u32, generation: u32, now: Time) -> DiskInode {
        let mut d = DiskInode { ino, raw: alloc::vec![0u8; geo.inode_size] };
        d.set_mode(mode);
        d.set_owner(uid, gid);
        if geo.inode_size > 128 {
            p16(&mut d.raw, EXTRA_ISIZE, geo.want_extra_isize);
        }
        for s in [Stamp::A, Stamp::M, Stamp::C, Stamp::Cr] {
            d.set_time(s, now);
        }
        p32(&mut d.raw, GENERATION, generation);
        let mut flags = parent_flags & FL_INHERITED;
        match mode & S_IFMT {
            S_IFDIR => {}
            S_IFREG => flags &= !(FL_DIRSYNC | FL_PROJINHERIT),
            _ => flags &= FL_NODUMP | FL_NOATIME,
        }
        if matches!(mode & S_IFMT, S_IFREG | S_IFDIR) && geo.has_in(crate::sb::incompat::EXTENTS) {
            flags |= FL_EXTENTS;
            crate::extent::init_root(d.i_block_mut());
        }
        d.set_flags(flags);
        d
    }
}
