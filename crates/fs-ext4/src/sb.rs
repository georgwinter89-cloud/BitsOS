//! The superblock (linux-6.6/fs/ext4/ext4.h:1289 `struct ext4_super_block`): geometry, features, and what V2
//! may do with the volume. The 1024 bytes are kept as read and changed in place, so fields V2 does not know
//! survive every write.

use crate::crc::crc32c;
use crate::le::*;
use alloc::vec::Vec;
use crate::errno::*;

pub const MAGIC: u16 = 0xEF53;
pub const OFFSET: u64 = 1024;
pub const SIZE: usize = 1024;

// Field offsets.
pub const INODES_COUNT: usize = 0x00;
pub const BLOCKS_LO: usize = 0x04;
pub const R_BLOCKS_LO: usize = 0x08;
pub const FREE_BLOCKS_LO: usize = 0x0C;
pub const FREE_INODES: usize = 0x10;
pub const MTIME: usize = 0x2C;
pub const WTIME: usize = 0x30;
pub const DEF_RESUID: usize = 0x50;
pub const DEF_RESGID: usize = 0x52;
pub const MNT_COUNT: usize = 0x34;
pub const STATE: usize = 0x3A;
pub const FEATURE_COMPAT: usize = 0x5C;
pub const FEATURE_INCOMPAT: usize = 0x60;
pub const FEATURE_RO_COMPAT: usize = 0x64;
pub const UUID: usize = 0x68;
pub const JOURNAL_INUM: usize = 0xE0;
pub const LAST_ORPHAN: usize = 0xE8;
pub const BLOCKS_HI: usize = 0x150;
pub const R_BLOCKS_HI: usize = 0x154;
pub const FREE_BLOCKS_HI: usize = 0x158;
pub const KBYTES_WRITTEN: usize = 0x178;
pub const CHECKSUM_SEED: usize = 0x270;
pub const CHECKSUM: usize = 0x3FC;

pub const STATE_VALID: u16 = 1;
pub const STATE_ERROR: u16 = 2;

pub mod compat {
    pub const HAS_JOURNAL: u32 = 0x4;
    pub const EXT_ATTR: u32 = 0x8;
    pub const RESIZE_INODE: u32 = 0x10;
    pub const DIR_INDEX: u32 = 0x20;
    pub const SPARSE_SUPER2: u32 = 0x200;
    pub const FAST_COMMIT: u32 = 0x400;
    pub const ORPHAN_FILE: u32 = 0x1000;
}
pub mod ro {
    pub const SPARSE_SUPER: u32 = 0x1;
    pub const LARGE_FILE: u32 = 0x2;
    pub const BTREE_DIR: u32 = 0x4;
    pub const HUGE_FILE: u32 = 0x8;
    pub const GDT_CSUM: u32 = 0x10;
    pub const DIR_NLINK: u32 = 0x20;
    pub const EXTRA_ISIZE: u32 = 0x40;
    pub const QUOTA: u32 = 0x100;
    pub const BIGALLOC: u32 = 0x200;
    pub const METADATA_CSUM: u32 = 0x400;
    pub const READONLY: u32 = 0x1000;
    pub const PROJECT: u32 = 0x2000;
    pub const VERITY: u32 = 0x8000;
    pub const ORPHAN_PRESENT: u32 = 0x10000;
}
pub mod incompat {
    pub const COMPRESSION: u32 = 0x1;
    pub const FILETYPE: u32 = 0x2;
    pub const RECOVER: u32 = 0x4;
    pub const JOURNAL_DEV: u32 = 0x8;
    pub const META_BG: u32 = 0x10;
    pub const EXTENTS: u32 = 0x40;
    pub const BIT64: u32 = 0x80;
    pub const MMP: u32 = 0x100;
    pub const FLEX_BG: u32 = 0x200;
    pub const EA_INODE: u32 = 0x400;
    pub const DIRDATA: u32 = 0x1000;
    pub const CSUM_SEED: u32 = 0x2000;
    pub const LARGEDIR: u32 = 0x4000;
    pub const INLINE_DATA: u32 = 0x8000;
    pub const ENCRYPT: u32 = 0x10000;
    pub const CASEFOLD: u32 = 0x20000;
}

/// Features V2 cannot even read correctly: the mount is refused.
const REFUSE_INCOMPAT: u32 = incompat::COMPRESSION | incompat::JOURNAL_DEV | incompat::DIRDATA | incompat::ENCRYPT | incompat::CASEFOLD;
/// Features V2 reads but does not write yet (FS-8a): the mount is read-only. needs_recovery is replayed at
/// mount (FS-8b, `Volume::recover`).
const RO_INCOMPAT: u32 = incompat::MMP | incompat::EA_INODE | incompat::INLINE_DATA;
const KNOWN_INCOMPAT: u32 = REFUSE_INCOMPAT | RO_INCOMPAT | incompat::RECOVER | incompat::FILETYPE | incompat::META_BG | incompat::EXTENTS | incompat::BIT64 | incompat::FLEX_BG | incompat::CSUM_SEED | incompat::LARGEDIR;
/// ro_compat features whose metadata V2 keeps right when it writes.
const RW_RO_COMPAT: u32 = ro::SPARSE_SUPER | ro::LARGE_FILE | ro::BTREE_DIR | ro::HUGE_FILE | ro::GDT_CSUM | ro::DIR_NLINK | ro::EXTRA_ISIZE | ro::METADATA_CSUM | ro::PROJECT;

/// What V2 may do with a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadWrite,
    /// Readable; the reason it is not writable.
    ReadOnly(&'static str),
}

/// The geometry and features, decoded and checked once at mount.
#[derive(Clone, Debug)]
pub struct Geo {
    pub bs: u64,
    pub blocks: u64,
    pub first_data_block: u64,
    pub bpg: u64,
    pub ipg: u32,
    pub groups: u32,
    pub inodes: u32,
    pub first_ino: u32,
    pub inode_size: usize,
    pub desc_size: usize,
    pub want_extra_isize: u16,
    pub compat: u32,
    pub incompat: u32,
    pub ro_compat: u32,
    /// metadata_csum: the crc32c seed (`s_csum_seed`).
    pub csum: Option<u32>,
    pub uuid: [u8; 16],
    pub reserved_gdt: u64,
    pub first_meta_bg: u32,
    pub log_groups_per_flex: u8,
}

impl Geo {
    pub fn has_ro(&self, f: u32) -> bool {
        self.ro_compat & f != 0
    }
    pub fn has_in(&self, f: u32) -> bool {
        self.incompat & f != 0
    }
    pub fn has_compat(&self, f: u32) -> bool {
        self.compat & f != 0
    }
    pub fn is64(&self) -> bool {
        self.has_in(incompat::BIT64)
    }
    pub fn descs_per_block(&self) -> u64 {
        self.bs / self.desc_size as u64
    }
    /// Blocks in group `g` (the last one may be short).
    pub fn blocks_in(&self, g: u32) -> u64 {
        let start = self.group_start(g);
        (self.blocks - start).min(self.bpg)
    }
    pub fn group_start(&self, g: u32) -> u64 {
        self.first_data_block + g as u64 * self.bpg
    }
    pub fn group_of_block(&self, b: u64) -> u32 {
        ((b - self.first_data_block) / self.bpg) as u32
    }
    /// Whether group `g` holds a superblock backup (`ext4_bg_has_super`, linux-6.6/fs/ext4/balloc.c:782).
    pub fn has_super(&self, g: u32) -> bool {
        if g == 0 {
            return true;
        }
        if !self.has_ro(ro::SPARSE_SUPER) {
            return true;
        }
        if g == 1 {
            return true;
        }
        if g & 1 == 0 {
            return false;
        }
        is_power(g, 3) || is_power(g, 5) || is_power(g, 7)
    }
    /// Blocks of the group descriptor table.
    pub fn gdt_blocks(&self) -> u64 {
        (self.groups as u64).div_ceil(self.descs_per_block())
    }
    /// Whether a sector size `ss` device can carry this volume.
    pub fn fits(&self, dev_bytes: u64) -> bool {
        self.blocks.checked_mul(self.bs).is_some_and(|b| b <= dev_bytes)
    }
}

fn is_power(mut g: u32, base: u32) -> bool {
    while g > 1 && g % base == 0 {
        g /= base;
    }
    g == 1
}

/// `ext4_superblock_csum` (linux-6.6/fs/ext4/super.c:276).
pub fn checksum(raw: &[u8]) -> u32 {
    crc32c(!0, &raw[..CHECKSUM])
}

pub fn set_checksum(raw: &mut [u8], geo: &Geo) {
    if geo.csum.is_some() {
        let c = checksum(raw);
        p32(raw, CHECKSUM, c);
    }
}

/// Decodes and checks a superblock; EINVAL for something that is not ext2/3/4 or that V2 cannot read,
/// EUCLEAN for impossible geometry, EBADMSG for a wrong checksum (Linux's EFSCORRUPTED/EFSBADCRC).
pub fn parse(raw: &[u8]) -> Result<(Geo, Access), i64> {
    if raw.len() < SIZE || g16(raw, 0x38) != MAGIC {
        return Err(EINVAL);
    }
    let rev = g32(raw, 0x4C);
    let (compat, incompat, ro_compat) = if rev >= 1 { (g32(raw, FEATURE_COMPAT), g32(raw, FEATURE_INCOMPAT), g32(raw, FEATURE_RO_COMPAT)) } else { (0, 0, 0) };
    if incompat & REFUSE_INCOMPAT != 0 || incompat & !KNOWN_INCOMPAT != 0 {
        klog::event!(Info, "ext4", "mount.refused", incompat = incompat; "ext4 features V2 cannot read");
        return Err(EINVAL);
    }
    if ro_compat & ro::BIGALLOC != 0 {
        klog::event!(Info, "ext4", "mount.refused", feature = "bigalloc");
        return Err(EINVAL);
    }
    let log = g32(raw, 0x18);
    if log > 2 {
        // Blocks above 4 KiB do not fit V2's 4 KiB pages one block per page.
        klog::event!(Info, "ext4", "mount.refused", log_block_size = log; "block size above 4096");
        return Err(EINVAL);
    }
    let bs = 1024u64 << log;
    if g32(raw, 0x1C) != log && ro_compat & ro::BIGALLOC == 0 && g32(raw, 0x1C) != 0 {
        return Err(EUCLEAN);
    }
    let is64 = incompat & incompat::BIT64 != 0;
    let blocks = lohi(g32(raw, BLOCKS_LO), if is64 { g32(raw, BLOCKS_HI) } else { 0 });
    let first_data_block = g32(raw, 0x14) as u64;
    let bpg = g32(raw, 0x20) as u64;
    let ipg = g32(raw, 0x28);
    let inodes = g32(raw, INODES_COUNT);
    let (inode_size, first_ino) = if rev >= 1 { (g16(raw, 0x58) as usize, g32(raw, 0x54)) } else { (128, 11) };
    let desc_size = if is64 { g16(raw, 0xFE) as usize } else { 32 };
    let sane = bpg > 0
        && bpg <= 8 * bs
        && ipg > 0
        && ipg as u64 <= 8 * bs
        && blocks > first_data_block
        && first_data_block == (bs == 1024) as u64
        && inode_size >= 128
        && inode_size.is_power_of_two()
        && inode_size as u64 <= bs
        && desc_size >= 32
        && desc_size.is_power_of_two()
        && desc_size as u64 <= bs
        && (!is64 || desc_size >= 64)
        && first_ino > 2
        && first_ino <= inodes;
    if !sane {
        klog::event!(Warn, "ext4", "sb.corrupt", bpg = bpg, ipg = ipg, blocks = blocks, inode_size = inode_size, desc_size = desc_size);
        return Err(EUCLEAN);
    }
    let groups = (blocks - first_data_block).div_ceil(bpg);
    if groups > u32::MAX as u64 || groups * ipg as u64 != inodes as u64 {
        klog::event!(Warn, "ext4", "sb.corrupt", groups = groups, ipg = ipg, inodes = inodes; "inode count does not match the groups");
        return Err(EUCLEAN);
    }
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&raw[UUID..UUID + 16]);
    let csum = if ro_compat & ro::METADATA_CSUM != 0 {
        if raw[0x175] != 1 {
            return Err(EINVAL);
        }
        if checksum(raw) != g32(raw, CHECKSUM) {
            klog::event!(Warn, "ext4", "sb.badcrc");
            return Err(EBADMSG);
        }
        Some(if incompat & incompat::CSUM_SEED != 0 { g32(raw, CHECKSUM_SEED) } else { crc32c(!0, &uuid) })
    } else {
        None
    };
    let geo = Geo {
        bs,
        blocks,
        first_data_block,
        bpg,
        ipg,
        groups: groups as u32,
        inodes,
        first_ino,
        inode_size,
        desc_size,
        want_extra_isize: if inode_size > 128 { g16(raw, 0x15E).max(g16(raw, 0x15C)).max(32).min(inode_size as u16 - 128) } else { 0 },
        compat,
        incompat,
        ro_compat,
        csum,
        uuid,
        reserved_gdt: g16(raw, 0xCE) as u64,
        first_meta_bg: g32(raw, 0x104),
        log_groups_per_flex: raw[0x174],
    };
    let access = if compat & compat::SPARSE_SUPER2 != 0 {
        // Linux 6.6 does not know where sparse_super2's two backups sit for writing either (resize refuses it).
        return Err(EINVAL);
    } else if incompat & RO_INCOMPAT != 0 {
        Access::ReadOnly("inline_data, mmp or ea_inode (read only in FS-8a)")
    } else if ro_compat & !RW_RO_COMPAT != 0 {
        Access::ReadOnly("a read-only-compatible feature V2 does not write (quota, verity, orphan_present, readonly)")
    } else if incompat & incompat::EXTENTS == 0 {
        Access::ReadOnly("no extents (ext2/ext3 block maps are read only)")
    } else if g16(raw, STATE) & STATE_ERROR != 0 {
        Access::ReadOnly("the volume has recorded errors: e2fsck first")
    } else {
        Access::ReadWrite
    };
    Ok((geo, access))
}

/// A fresh superblock buffer for writing back: `raw` with the counters of `free_blocks`/`free_inodes`.
pub fn with_counts(raw: &[u8], geo: &Geo, free_blocks: u64, free_inodes: u32) -> Vec<u8> {
    let mut r = raw.to_vec();
    p32(&mut r, FREE_BLOCKS_LO, free_blocks as u32);
    if geo.is64() {
        p32(&mut r, FREE_BLOCKS_HI, (free_blocks >> 32) as u32);
    }
    p32(&mut r, FREE_INODES, free_inodes);
    set_checksum(&mut r, geo);
    r
}

pub fn reserved_blocks(raw: &[u8], geo: &Geo) -> u64 {
    lohi(g32(raw, R_BLOCKS_LO), if geo.is64() { g32(raw, R_BLOCKS_HI) } else { 0 })
}
