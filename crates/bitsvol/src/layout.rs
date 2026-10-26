//! On-disk format, version 1. All integers little endian.
//!
//! The first chunk(s) hold the two superblock slots (pages 0 and 1: two chunks when a chunk is 4 KiB); every other chunk is free, a data chunk or a
//! metadata chunk cut into 4 KiB pages. Page number p lives at byte p * 4096, so page number 0 never names a
//! metadata page and serves as "none"; chunk number 0 likewise means "unmapped" in a leaf.
//!
//! Superblock (one 4 KiB slot; generation g is written to slot g % 2, the valid one with the higher generation
//! wins): magic "BITSVOL1", version, block size, chunk size, chunk count, generation, volume table root and
//! depth, next volume id, pool time, CRC-32C of bytes 0..4092 at 4092.
//!
//! Page header (32 bytes): magic, kind, level, tree, own page number (catches misdirected writes), generation
//! that wrote it, CRC-32C of the page with the CRC field zero. Inner pages hold 508 child page numbers; leaves
//! hold 254 entries of (value, time): for a volume's mapping tree (chunk, write time), for the volume table
//! (record page, 0). A record page holds one volume or snapshot.

use crate::crc::crc32c;
use crate::error::{Result, VolError};
use alloc::string::String;
use alloc::vec::Vec;

pub const PAGE: usize = 4096;
pub const HDR: usize = 32;
pub const INNER_FANOUT: u64 = ((PAGE - HDR) / 8) as u64;
pub const LEAF_FANOUT: u64 = ((PAGE - HDR) / 16) as u64;
/// Bound against hostile input, not a count: 254 * 508^15 keys exceed u64.
pub const DEPTH_BOUND: u32 = 16;

const SB_MAGIC: &[u8; 8] = b"BITSVOL1";
const PAGE_MAGIC: u32 = 0x4256_5047;
pub const VERSION: u32 = 1;

pub const KIND_INNER: u8 = 1;
pub const KIND_LEAF: u8 = 2;
pub const KIND_RECORD: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tree {
    Map = 0,
    Table = 1,
}

pub fn get64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}
pub fn get32(b: &[u8], off: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[off..off + 4]);
    u32::from_le_bytes(a)
}
pub fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
pub fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Super {
    pub block_size: u32,
    pub chunk_size: u32,
    pub chunks: u64,
    pub generation: u64,
    pub table_root: u64,
    pub table_depth: u32,
    pub next_id: u64,
    pub time: u64,
}

impl Super {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = alloc::vec![0u8; PAGE];
        b[0..8].copy_from_slice(SB_MAGIC);
        put32(&mut b, 8, VERSION);
        put32(&mut b, 12, self.block_size);
        put32(&mut b, 16, self.chunk_size);
        put64(&mut b, 24, self.chunks);
        put64(&mut b, 32, self.generation);
        put64(&mut b, 40, self.table_root);
        put32(&mut b, 48, self.table_depth);
        put64(&mut b, 56, self.next_id);
        put64(&mut b, 64, self.time);
        let c = crc32c(&b[..PAGE - 4]);
        put32(&mut b, PAGE - 4, c);
        b
    }

    /// None for a slot that is empty, torn or not ours.
    pub fn decode(b: &[u8]) -> Option<Super> {
        if b.len() < PAGE || &b[0..8] != SB_MAGIC || get32(b, 8) != VERSION || crc32c(&b[..PAGE - 4]) != get32(b, PAGE - 4) {
            return None;
        }
        Some(Super {
            block_size: get32(b, 12),
            chunk_size: get32(b, 16),
            chunks: get64(b, 24),
            generation: get64(b, 32),
            table_root: get64(b, 40),
            table_depth: get32(b, 48),
            next_id: get64(b, 56),
            time: get64(b, 64),
        })
    }
}

pub fn init_page(b: &mut [u8], kind: u8, level: u8, tree: Tree) {
    b.fill(0);
    put32(b, 0, PAGE_MAGIC);
    b[4] = kind;
    b[5] = level;
    b[6] = tree as u8;
}

/// Stamps the page with its location and generation and seals it with the checksum, right before it is written.
pub fn seal(b: &mut [u8], pno: u64, generation: u64) {
    put64(b, 8, pno);
    put64(b, 16, generation);
    put32(b, 24, 0);
    let c = crc32c(b);
    put32(b, 24, c);
}

/// Checks a page read from the device: magic, checksum, own number.
pub fn verify(b: &[u8], pno: u64) -> Result<()> {
    if get32(b, 0) != PAGE_MAGIC {
        return Err(VolError::Corrupt("page magic"));
    }
    let c = crate::crc::update(crate::crc::update(crate::crc::update(!0, &b[..24]), &[0; 4]), &b[28..]);
    if !c != get32(b, 24) {
        return Err(VolError::Corrupt("page checksum"));
    }
    if get64(b, 8) != pno {
        return Err(VolError::Corrupt("page at the wrong place"));
    }
    Ok(())
}

pub fn kind(b: &[u8]) -> (u8, u8, u8) {
    (b[4], b[5], b[6])
}

pub fn inner_get(b: &[u8], i: u64) -> u64 {
    get64(b, HDR + i as usize * 8)
}
pub fn inner_set(b: &mut [u8], i: u64, v: u64) {
    put64(b, HDR + i as usize * 8, v)
}
pub fn leaf_get(b: &[u8], i: u64) -> (u64, u64) {
    let o = HDR + i as usize * 16;
    (get64(b, o), get64(b, o + 8))
}
pub fn leaf_set(b: &mut [u8], i: u64, v: (u64, u64)) {
    let o = HDR + i as usize * 16;
    put64(b, o, v.0);
    put64(b, o + 8, v.1);
}
pub fn entries_zero(b: &[u8]) -> bool {
    b[HDR..].iter().all(|&x| x == 0)
}

pub const FLAG_SNAPSHOT: u32 = 1;
pub const FLAG_READ_ONLY: u32 = 2;
/// Taken by the scheduler (VS-2): dropped oldest first when the pool runs out of space.
pub const FLAG_AUTO: u32 = 4;
/// On a snapshot (VS-3): its origin goes back to it when the origin is next opened (`Pool::schedule_rollback`,
/// applied by `PoolHandle::open_volume`, as LVM merges a snapshot into an origin that is in use at its next
/// activation). Such a snapshot is never deleted, not even to make room. Older code ignores the bit.
pub const FLAG_ROLLBACK: u32 = 8;

/// One volume or snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub id: u64,
    /// The volume a snapshot was taken of (0 for a volume).
    pub origin: u64,
    pub flags: u32,
    pub depth: u32,
    pub size: u64,
    pub root: u64,
    /// Pool time of the last snapshot that shares this tree: a mapping written before it may be shared.
    pub snap_time: u64,
    pub created_ns: u64,
    pub label: String,
}

const REC_LABEL: usize = 90;
/// Bound of one record page, not a count.
pub const LABEL_BOUND: usize = PAGE - REC_LABEL;

impl Record {
    pub fn encode(&self, b: &mut [u8]) {
        init_page(b, KIND_RECORD, 0, Tree::Table);
        put64(b, 32, self.id);
        put64(b, 40, self.origin);
        put32(b, 48, self.flags);
        put32(b, 52, self.depth);
        put64(b, 56, self.size);
        put64(b, 64, self.root);
        put64(b, 72, self.snap_time);
        put64(b, 80, self.created_ns);
        let l = self.label.as_bytes();
        b[88..90].copy_from_slice(&(l.len() as u16).to_le_bytes());
        b[REC_LABEL..REC_LABEL + l.len()].copy_from_slice(l);
    }

    pub fn decode(b: &[u8]) -> Result<Record> {
        if kind(b) != (KIND_RECORD, 0, Tree::Table as u8) {
            return Err(VolError::Corrupt("record page kind"));
        }
        let len = u16::from_le_bytes([b[88], b[89]]) as usize;
        if len > LABEL_BOUND {
            return Err(VolError::Corrupt("record label length"));
        }
        let label = core::str::from_utf8(&b[REC_LABEL..REC_LABEL + len]).map_err(|_| VolError::Corrupt("record label"))?;
        let mut s = String::new();
        s.try_reserve_exact(len).map_err(|_| VolError::NoMemory)?;
        s.push_str(label);
        Ok(Record {
            id: get64(b, 32),
            origin: get64(b, 40),
            flags: get32(b, 48),
            depth: get32(b, 52),
            size: get64(b, 56),
            root: get64(b, 64),
            snap_time: get64(b, 72),
            created_ns: get64(b, 80),
            label: s,
        })
    }

    pub fn is_snapshot(&self) -> bool {
        self.flags & FLAG_SNAPSHOT != 0
    }
    pub fn read_only(&self) -> bool {
        self.flags & FLAG_READ_ONLY != 0
    }
    pub fn auto(&self) -> bool {
        self.flags & FLAG_AUTO != 0
    }
    pub fn rollback_pending(&self) -> bool {
        self.flags & FLAG_ROLLBACK != 0
    }
}

/// Keys a tree of `depth` levels holds (saturating).
pub fn capacity(depth: u32) -> u64 {
    if depth == 0 {
        return 0;
    }
    let mut c = LEAF_FANOUT;
    for _ in 1..depth {
        c = c.saturating_mul(INNER_FANOUT);
    }
    c
}

/// Slot of `key` in the page at `level` (0 = leaf).
pub fn slot(key: u64, level: u32) -> u64 {
    if level == 0 {
        return key % LEAF_FANOUT;
    }
    let mut k = key / LEAF_FANOUT;
    for _ in 1..level {
        k /= INNER_FANOUT;
    }
    k % INNER_FANOUT
}

/// First key covered by the subtree below `slot` of a page at `level` whose first key is `base`.
pub fn child_base(base: u64, level: u32, slot: u64) -> u64 {
    if level == 0 {
        return base + slot;
    }
    let mut span = LEAF_FANOUT;
    for _ in 1..level {
        span = span.saturating_mul(INNER_FANOUT);
    }
    base.saturating_add(slot.saturating_mul(span))
}
