//! A fresh ext4 volume on a `Dev` (FS-8c: `\BITSOS2\data.img` made on the stick; NV-3's `bitsdisk` reuses
//! `format` for the BitsOS SSD). The layout follows `mke2fs` (e2fsprogs, no kernel counterpart) with a fixed
//! feature set V2 reads and writes fully: 4 KiB blocks, 64bit, extents, metadata_csum, an internal jbd2
//! journal (csum v3), dir_index, huge_file, dir_nlink, extra_isize, sparse_super, large_file, filetype.
//!
//! Not used (each a choice, not a gap): flex_bg (every group keeps its own bitmaps and inode table, so a
//! group's metadata never depends on another's), resize_inode (V2 never grows a mounted volume online),
//! orphan_file (the classic orphan list is what FS-8a writes), uninit_bg lazy flags (the inode tables are
//! zero, either written or known zero: a fresh FAT image is zero-filled, a fresh thin volume reads zero).
//!
//! Write order: tables and data first, the group descriptors, the backup superblocks, the primary superblock
//! last, then a flush; a cut before the end leaves no valid primary superblock (nothing mounts a half volume).

use crate::crc::crc32c;
use crate::dev::Dev;
use crate::dir;
use crate::errno::*;
use crate::gd::{self, Table};
use crate::inode::{DiskInode, S_IFDIR, S_IFREG};
use crate::jbd2::{self, Jsb};
use crate::le::*;
use crate::sb::{self, compat, incompat, ro};
use alloc::vec;
use alloc::vec::Vec;
use vfs::abi::Time;

const BS: u64 = 4096;
const BPG: u64 = 32768;
const INODE_SIZE: u64 = 256;
const EXTRA_ISIZE: u16 = 32;
const JOURNAL_INO: u32 = 8;
const LPF_INO: u32 = 11;
/// jbd2 refuses a journal below this (`JBD2_MIN_JOURNAL_BLOCKS`, linux-6.6/include/linux/jbd2.h:69).
const MIN_JOURNAL: u64 = 1024;
/// Largest journal made here: one extent inside one group (a group's free run is below 32768 blocks).
const MAX_JOURNAL: u64 = 16384;
/// Bytes zeroed per write when the device is not known to read zero.
const ZERO_RUN: usize = 256 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct MkfsOptions {
    pub label: [u8; 16],
    pub uuid: [u8; 16],
    /// dir_index hash seed (`s_hash_seed`).
    pub hash_seed: [u8; 16],
    pub now: Time,
    /// The device reads zeros everywhere (a fresh zero-filled image, a fresh thin volume): the inode tables
    /// and the journal are not written.
    pub zeroed: bool,
    /// Bytes per inode (mke2fs `-i`, default 16384).
    pub bytes_per_inode: u64,
    /// Journal size in blocks (None: mke2fs's table, at most 64 MiB).
    pub journal_blocks: Option<u64>,
    /// Percent of the blocks reserved for root (mke2fs `-m`, default 5, at most 50).
    pub reserved_percent: u8,
}

impl MkfsOptions {
    pub fn new(label: &[u8], uuid: [u8; 16], hash_seed: [u8; 16], now: Time) -> MkfsOptions {
        let mut l = [0u8; 16];
        let n = label.len().min(16);
        l[..n].copy_from_slice(&label[..n]);
        MkfsOptions { label: l, uuid, hash_seed, now, zeroed: false, bytes_per_inode: 16384, journal_blocks: None, reserved_percent: 5 }
    }
}

/// What `format` made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Made {
    pub blocks: u64,
    pub groups: u32,
    pub inodes: u32,
    pub journal_blocks: u64,
    pub free_blocks: u64,
}

/// `ext2fs_default_journal_size` (e2fsprogs lib/ext2fs/mkjournal.c) up to `MAX_JOURNAL`.
fn default_journal(blocks: u64) -> u64 {
    let j = if blocks < 32768 {
        1024
    } else if blocks < 256 * 1024 {
        4096
    } else if blocks < 512 * 1024 {
        8192
    } else {
        16384
    };
    j.min(MAX_JOURNAL)
}

struct Layout {
    blocks: u64,
    groups: u32,
    ipg: u32,
    itb: u64,
    gdt: u64,
}

impl Layout {
    fn start(&self, g: u32) -> u64 {
        g as u64 * BPG
    }
    fn len(&self, g: u32) -> u64 {
        (self.blocks - self.start(g)).min(BPG)
    }
    fn has_super(g: u32) -> bool {
        fn power(mut g: u32, b: u32) -> bool {
            while g > 1 && g % b == 0 {
                g /= b;
            }
            g == 1
        }
        g <= 1 || (g & 1 == 1 && (power(g, 3) || power(g, 5) || power(g, 7)))
    }
    /// Superblock copy and descriptor table at the group's start.
    fn base(&self, g: u32) -> u64 {
        if Self::has_super(g) { 1 + self.gdt } else { 0 }
    }
    fn block_bitmap(&self, g: u32) -> u64 {
        self.start(g) + self.base(g)
    }
    fn inode_table(&self, g: u32) -> u64 {
        self.block_bitmap(g) + 2
    }
    /// First block after the group's own metadata.
    fn meta_end(&self, g: u32) -> u64 {
        self.inode_table(g) + self.itb
    }

    fn plan(dev_blocks: u64, bytes_per_inode: u64) -> Result<Layout, i64> {
        let mut blocks = dev_blocks;
        if blocks < 2048 {
            return Err(EINVAL);
        }
        for _ in 0..2 {
            let groups = blocks.div_ceil(BPG);
            if groups > u32::MAX as u64 {
                return Err(EFBIG);
            }
            let want = (blocks * BS / bytes_per_inode.max(BS)).max(64);
            let per_block = BS / INODE_SIZE;
            let ipg = want.div_ceil(groups).div_ceil(per_block) * per_block;
            let ipg = ipg.clamp(per_block, BPG).min((u32::MAX as u64 / groups) / per_block * per_block);
            let l = Layout { blocks, groups: groups as u32, ipg: ipg as u32, itb: ipg * INODE_SIZE / BS, gdt: (groups * 64).div_ceil(BS) };
            let last = l.groups - 1;
            // A last group too short for its metadata and a little data is dropped (mke2fs does the same).
            if groups > 1 && l.len(last) < l.meta_end(last) - l.start(last) + 64 {
                blocks = l.start(last);
                continue;
            }
            if l.meta_end(0) + 2 + MIN_JOURNAL > l.start(0) + l.len(0) {
                return Err(EINVAL);
            }
            return Ok(l);
        }
        Err(EINVAL)
    }
}

fn set_bits(b: &mut [u8], from: u64, to: u64) {
    for i in from..to {
        if let Some(x) = b.get_mut((i / 8) as usize) {
            *x |= 1 << (i % 8);
        }
    }
}

fn put_extent(di: &mut DiskInode, start: u64, len: u64) {
    let ib = di.i_block_mut();
    crate::extent::init_root(ib);
    p16(ib, 2, 1);
    p32(ib, 12, 0);
    p16(ib, 16, len as u16);
    p16(ib, 18, (start >> 32) as u16);
    p32(ib, 20, start as u32);
}

fn put_desc_block(t: &mut Table, g: u32, off_lo: usize, off_hi: usize, v: u64) {
    let d = t.desc_mut(g);
    p32(d, off_lo, v as u32);
    p32(d, off_hi, (v >> 32) as u32);
}

fn zero(dev: &dyn Dev, first: u64, n: u64) -> Result<(), i64> {
    let buf = vec![0u8; ZERO_RUN];
    let (mut at, end) = (first * BS, (first + n) * BS);
    while at < end {
        let k = ((end - at) as usize).min(ZERO_RUN);
        dev.write(at, &buf[..k])?;
        at += k as u64;
    }
    Ok(())
}

/// The superblock before the counts (and its checksum) are known.
fn superblock(l: &Layout, o: &MkfsOptions, jb: u64, jsb_extent: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; sb::SIZE];
    let inodes = l.ipg as u64 * l.groups as u64;
    p32(&mut r, sb::INODES_COUNT, inodes as u32);
    p32(&mut r, sb::BLOCKS_LO, l.blocks as u32);
    p32(&mut r, sb::BLOCKS_HI, (l.blocks >> 32) as u32);
    // mke2fs `-m`: blocks only root may take (V2 holds them back from other users, state.rs).
    let reserved = l.blocks * o.reserved_percent.min(50) as u64 / 100;
    p32(&mut r, sb::R_BLOCKS_LO, reserved as u32);
    p32(&mut r, sb::R_BLOCKS_HI, (reserved >> 32) as u32);
    p32(&mut r, 0x14, 0);
    p32(&mut r, 0x18, 2);
    p32(&mut r, 0x1C, 2);
    p32(&mut r, 0x20, BPG as u32);
    p32(&mut r, 0x24, BPG as u32);
    p32(&mut r, 0x28, l.ipg);
    let now = o.now.sec.max(0) as u32;
    p32(&mut r, sb::WTIME, now);
    p16(&mut r, 0x36, 0xFFFF);
    p16(&mut r, 0x38, sb::MAGIC);
    p16(&mut r, sb::STATE, sb::STATE_VALID);
    // errors=remount-ro: what V2's ext4 does on an error (Linux's default for a volume without the setting).
    p16(&mut r, 0x3C, 2);
    p32(&mut r, 0x40, now);
    p32(&mut r, 0x4C, 1);
    p32(&mut r, 0x54, 11);
    p16(&mut r, 0x58, INODE_SIZE as u16);
    p32(&mut r, sb::FEATURE_COMPAT, compat::HAS_JOURNAL | compat::EXT_ATTR | compat::DIR_INDEX);
    p32(&mut r, sb::FEATURE_INCOMPAT, incompat::FILETYPE | incompat::EXTENTS | incompat::BIT64);
    p32(&mut r, sb::FEATURE_RO_COMPAT, ro::SPARSE_SUPER | ro::LARGE_FILE | ro::HUGE_FILE | ro::DIR_NLINK | ro::EXTRA_ISIZE | ro::METADATA_CSUM);
    r[sb::UUID..sb::UUID + 16].copy_from_slice(&o.uuid);
    r[0x78..0x88].copy_from_slice(&o.label);
    p32(&mut r, sb::JOURNAL_INUM, JOURNAL_INO);
    r[0xEC..0xFC].copy_from_slice(&o.hash_seed);
    // half_md4, the default of mke2fs; signed hash as on x86 (`EXT2_FLAGS_SIGNED_HASH`).
    r[0xFC] = 1;
    r[0xFD] = 1;
    p16(&mut r, 0xFE, 64);
    // user_xattr, acl: mke2fs's default mount options.
    p32(&mut r, 0x100, 0x000C);
    p32(&mut r, 0x108, now);
    // s_jnl_blocks: the journal inode's i_block, i_size_high, i_size (EXT3_JNL_BACKUP_BLOCKS).
    r[0x10C..0x10C + 60].copy_from_slice(jsb_extent);
    let jbytes = jb * BS;
    p32(&mut r, 0x10C + 15 * 4, (jbytes >> 32) as u32);
    p32(&mut r, 0x10C + 16 * 4, jbytes as u32);
    p16(&mut r, 0x15C, EXTRA_ISIZE);
    p16(&mut r, 0x15E, EXTRA_ISIZE);
    p32(&mut r, 0x160, 1);
    r[0x175] = 1;
    let c = sb::checksum(&r);
    p32(&mut r, sb::CHECKSUM, c);
    r
}

fn journal_sb(jb: u64, uuid: &[u8; 16]) -> Vec<u8> {
    let mut raw = vec![0u8; jbd2::SB_SIZE];
    jbd2::pb32(&mut raw, 0, jbd2::MAGIC);
    jbd2::pb32(&mut raw, jbd2::S_BLOCKSIZE, BS as u32);
    jbd2::pb32(&mut raw, jbd2::S_MAXLEN, jb as u32);
    jbd2::pb32(&mut raw, jbd2::S_FIRST, 1);
    raw[jbd2::S_UUID..jbd2::S_UUID + 16].copy_from_slice(uuid);
    jbd2::pb32(&mut raw, jbd2::S_NR_USERS, 1);
    // s_users[0]: the one file system of an internal journal (`ext2fs_create_journal_superblock`).
    raw[0x100..0x110].copy_from_slice(uuid);
    let incompat = jbd2::INCOMPAT_REVOKE | jbd2::INCOMPAT_64BIT | jbd2::INCOMPAT_CSUM_V3;
    let mut j = Jsb { raw, bs: BS as u32, maxlen: jb as u32, first: 1, sequence: 1, start: 0, compat: 0, incompat, uuid: *uuid };
    j.serialise(0, 1, 0)
}

/// Makes a fresh ext4 volume over the whole of `dev` (its size rounded down to 4 KiB blocks).
pub fn format(dev: &dyn Dev, o: &MkfsOptions) -> Result<Made, i64> {
    let l = Layout::plan(dev.size() / BS, o.bytes_per_inode)?;
    let free0 = l.start(0) + l.len(0) - l.meta_end(0) - 2;
    let jb = o.journal_blocks.unwrap_or_else(|| default_journal(l.blocks)).clamp(MIN_JOURNAL, MAX_JOURNAL.min(free0));
    let (root_blk, lpf_blk, j0) = (l.meta_end(0), l.meta_end(0) + 1, l.meta_end(0) + 2);

    let mut jext = DiskInode { ino: JOURNAL_INO, raw: vec![0u8; INODE_SIZE as usize] };
    put_extent(&mut jext, j0, jb);
    let raw = superblock(&l, o, jb, jext.i_block());
    let (geo, _) = sb::parse(&raw)?;
    klog::event!(Info, "ext4", "mkfs.start", blocks = l.blocks, groups = l.groups, ipg = l.ipg, journal_blocks = jb, zeroed = o.zeroed);

    if !o.zeroed {
        for g in 0..l.groups {
            zero(dev, l.inode_table(g), l.itb)?;
        }
        zero(dev, j0, jb)?;
    }

    // Bitmaps and descriptors.
    let mut t = Table { raw: vec![0u8; l.groups as usize * 64], ds: 64, is64: true };
    let mut free_total = 0u64;
    for g in 0..l.groups {
        let start = l.start(g);
        let mut bb = vec![0u8; BS as usize];
        let mut used = l.meta_end(g) - start;
        if g == 0 {
            used += 2 + jb;
        }
        set_bits(&mut bb, 0, used);
        set_bits(&mut bb, l.len(g), BS * 8);
        let mut ib = vec![0u8; BS as usize];
        let inodes_used = if g == 0 { LPF_INO as u64 } else { 0 };
        set_bits(&mut ib, 0, inodes_used);
        set_bits(&mut ib, l.ipg as u64, BS * 8);
        dev.write(l.block_bitmap(g) * BS, &bb)?;
        dev.write((l.block_bitmap(g) + 1) * BS, &ib)?;
        let free = l.len(g) - used;
        free_total += free;
        put_desc_block(&mut t, g, 0x00, 0x20, l.block_bitmap(g));
        put_desc_block(&mut t, g, 0x04, 0x24, l.block_bitmap(g) + 1);
        put_desc_block(&mut t, g, 0x08, 0x28, l.inode_table(g));
        t.set_free_blocks(g, free);
        t.set_free_inodes(g, l.ipg as u64 - inodes_used);
        t.set_used_dirs(g, if g == 0 { 2 } else { 0 });
        t.set_itable_unused(g, l.ipg as u64 - inodes_used);
        t.set_flags(g, gd::ITABLE_ZEROED);
        let seed = geo.csum.unwrap_or(0);
        t.set_bitmap_csum(g, false, crc32c(seed, &bb[..(BPG / 8) as usize]));
        t.set_bitmap_csum(g, true, crc32c(seed, &ib[..(l.ipg / 8) as usize]));
        t.seal(&geo, g);
    }

    // Root, lost+found and the journal inode (all in the first inode-table block of group 0).
    let now = o.now;
    let mut root = DiskInode::fresh(&geo, 2, S_IFDIR | 0o755, 0, 0, 0, 0, now);
    let mut lpf = DiskInode::fresh(&geo, LPF_INO, S_IFDIR | 0o700, 0, 0, 0, 0, now);
    let mut jnl = DiskInode::fresh(&geo, JOURNAL_INO, S_IFREG | 0o600, 0, 0, 0, 0, now);
    for (di, blk, n, links) in [(&mut root, root_blk, 1u64, 3u16), (&mut lpf, lpf_blk, 1, 2), (&mut jnl, j0, jb, 1)] {
        put_extent(di, blk, n);
        di.set_size(n * BS);
        di.set_links(links);
        di.set_blocks512(&geo, n * (BS / 512))?;
        di.seal(&geo);
    }
    let bsz = BS as usize;
    let mut rb = dir::first_block(bsz, root.csum_seed(&geo), 2, 2);
    if !dir::insert(&mut rb, dir::area(bsz, true), b"lost+found", LPF_INO, dir::FT_DIR)? {
        return Err(EINVAL);
    }
    dir::seal(&mut rb, bsz, root.csum_seed(&geo));
    let lb = dir::first_block(bsz, lpf.csum_seed(&geo), LPF_INO, 2);
    let mut itb0 = vec![0u8; bsz];
    for di in [&root, &lpf, &jnl] {
        let o = (di.ino as usize - 1) * INODE_SIZE as usize;
        itb0[o..o + INODE_SIZE as usize].copy_from_slice(&di.raw);
    }
    dev.write(l.inode_table(0) * BS, &itb0)?;
    dev.write(root_blk * BS, &rb)?;
    dev.write(lpf_blk * BS, &lb)?;
    let mut jblk = journal_sb(jb, &o.uuid);
    jblk.resize(bsz, 0);
    dev.write(j0 * BS, &jblk)?;

    // Descriptor tables, backup superblocks, the primary superblock last.
    let free_inodes = (l.ipg as u64 * l.groups as u64 - LPF_INO as u64) as u32;
    let base = sb::with_counts(&raw, &geo, free_total, free_inodes);
    let mut gdt = t.raw.clone();
    gdt.resize((l.gdt * BS) as usize, 0);
    for g in (0..l.groups).filter(|&g| Layout::has_super(g)) {
        dev.write((l.start(g) + 1) * BS, &gdt)?;
    }
    for g in (1..l.groups).filter(|&g| Layout::has_super(g)) {
        let mut s = base.clone();
        p16(&mut s, 0x5A, g as u16);
        sb::set_checksum(&mut s, &geo);
        let mut blk = s;
        blk.resize(bsz, 0);
        dev.write(l.start(g) * BS, &blk)?;
    }
    let mut b0 = vec![0u8; bsz];
    b0[sb::OFFSET as usize..sb::OFFSET as usize + sb::SIZE].copy_from_slice(&base);
    dev.write(0, &b0)?;
    dev.flush()?;
    let made = Made { blocks: l.blocks, groups: l.groups, inodes: l.ipg * l.groups, journal_blocks: jb, free_blocks: free_total };
    klog::event!(Info, "ext4", "mkfs.done", blocks = made.blocks, groups = made.groups, inodes = made.inodes, free_blocks = made.free_blocks);
    Ok(made)
}
