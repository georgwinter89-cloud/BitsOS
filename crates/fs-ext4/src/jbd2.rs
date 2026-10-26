//! The jbd2 on-disk format (linux-6.6/include/linux/jbd2.h): big-endian headers, the journal superblock,
//! descriptor tags, revoke records, commit blocks and their crc32c checksums (v2/v3). Everything here is
//! pure: building and checking blocks; `journal` writes them and `recovery` reads them back.

use crate::crc::crc32c;
use alloc::vec;
use alloc::vec::Vec;

pub const MAGIC: u32 = 0xC03B_3998;
pub const DESCRIPTOR: u32 = 1;
pub const COMMIT: u32 = 2;
pub const SB_V1: u32 = 3;
pub const SB_V2: u32 = 4;
pub const REVOKE: u32 = 5;

pub const FLAG_ESCAPE: u16 = 1;
pub const FLAG_SAME_UUID: u16 = 2;
pub const FLAG_LAST_TAG: u16 = 8;

pub const COMPAT_CHECKSUM: u32 = 0x1;
pub const INCOMPAT_REVOKE: u32 = 0x1;
pub const INCOMPAT_64BIT: u32 = 0x2;
pub const INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
pub const INCOMPAT_CSUM_V2: u32 = 0x8;
pub const INCOMPAT_CSUM_V3: u32 = 0x10;
pub const INCOMPAT_FAST_COMMIT: u32 = 0x20;
/// `JBD2_CRC32C_CHKSUM`.
pub const CSUM_CRC32C: u8 = 4;
/// The journal superblock's size (`journal_superblock_t`).
pub const SB_SIZE: usize = 1024;
/// `journal_header_t`.
pub const HEADER: usize = 12;
/// `jbd2_journal_revoke_header_t`.
pub const REVOKE_HEADER: usize = 16;

// Journal superblock fields.
pub(crate) const S_BLOCKSIZE: usize = 0x0C;
pub(crate) const S_MAXLEN: usize = 0x10;
pub(crate) const S_FIRST: usize = 0x14;
pub(crate) const S_SEQUENCE: usize = 0x18;
pub(crate) const S_START: usize = 0x1C;
pub(crate) const S_ERRNO: usize = 0x20;
pub(crate) const S_COMPAT: usize = 0x24;
pub(crate) const S_INCOMPAT: usize = 0x28;
pub(crate) const S_RO_COMPAT: usize = 0x2C;
pub(crate) const S_UUID: usize = 0x30;
pub(crate) const S_NR_USERS: usize = 0x40;
pub(crate) const S_CSUM_TYPE: usize = 0x50;
pub(crate) const S_NUM_FC: usize = 0x54;
pub(crate) const S_HEAD: usize = 0x58;
pub(crate) const S_CHECKSUM: usize = 0xFC;

pub fn gb32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}
pub fn gb64(b: &[u8], o: usize) -> u64 {
    (gb32(b, o) as u64) << 32 | gb32(b, o + 4) as u64
}
pub fn gb16(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map_or(0, |s| u16::from_be_bytes([s[0], s[1]]))
}
pub fn pb32(b: &mut [u8], o: usize, v: u32) {
    if let Some(s) = b.get_mut(o..o + 4) {
        s.copy_from_slice(&v.to_be_bytes());
    }
}
pub fn pb16(b: &mut [u8], o: usize, v: u16) {
    if let Some(s) = b.get_mut(o..o + 2) {
        s.copy_from_slice(&v.to_be_bytes());
    }
}

/// `tid_gt`: sequence numbers wrap.
pub fn tid_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Which checksums the journal carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Csum {
    None,
    V2,
    V3,
}

/// The decoded journal superblock (the raw 1024 bytes are kept so unknown fields survive a rewrite).
#[derive(Clone, Debug)]
pub struct Jsb {
    pub raw: Vec<u8>,
    pub bs: u32,
    pub maxlen: u32,
    pub first: u32,
    pub sequence: u32,
    pub start: u32,
    pub compat: u32,
    pub incompat: u32,
    pub uuid: [u8; 16],
}

/// Why a journal superblock cannot be used.
pub type Refusal = &'static str;

impl Jsb {
    /// `journal_load_superblock` / `jbd2_journal_check_used_features`: magic, version, block size, lengths,
    /// known features and the superblock checksum. `blocks` is the journal inode's size in blocks.
    pub fn parse(b: &[u8], fs_bs: u32, blocks: u64) -> Result<Jsb, Refusal> {
        let raw = b.get(..SB_SIZE).ok_or("journal superblock short")?.to_vec();
        if gb32(&raw, 0) != MAGIC {
            return Err("journal superblock magic");
        }
        let kind = gb32(&raw, 4);
        let (compat, incompat, ro) = match kind {
            SB_V1 => (0, 0, 0),
            SB_V2 => (gb32(&raw, S_COMPAT), gb32(&raw, S_INCOMPAT), gb32(&raw, S_RO_COMPAT)),
            _ => return Err("journal superblock version"),
        };
        let j = Jsb {
            bs: gb32(&raw, S_BLOCKSIZE),
            maxlen: gb32(&raw, S_MAXLEN),
            first: gb32(&raw, S_FIRST),
            sequence: gb32(&raw, S_SEQUENCE),
            start: gb32(&raw, S_START),
            compat,
            incompat,
            uuid: raw[S_UUID..S_UUID + 16].try_into().unwrap_or([0; 16]),
            raw,
        };
        if j.bs != fs_bs {
            return Err("journal block size differs from the volume's");
        }
        if j.maxlen as u64 > blocks || j.maxlen < 8 || j.first == 0 || j.first >= j.maxlen {
            return Err("journal length or first block impossible");
        }
        if j.start != 0 && (j.start < j.first || j.start >= j.log_end()) {
            return Err("journal start outside the log");
        }
        if ro != 0 || incompat & !(INCOMPAT_REVOKE | INCOMPAT_64BIT | INCOMPAT_ASYNC_COMMIT | INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3 | INCOMPAT_FAST_COMMIT) != 0 {
            return Err("journal features unknown");
        }
        if incompat & INCOMPAT_CSUM_V2 != 0 && incompat & INCOMPAT_CSUM_V3 != 0 {
            return Err("journal says csum v2 and v3");
        }
        if j.csum() != Csum::None {
            if j.raw[S_CSUM_TYPE] != CSUM_CRC32C {
                return Err("journal checksum type");
            }
            if sb_checksum(&j.raw) != gb32(&j.raw, S_CHECKSUM) {
                return Err("journal superblock checksum");
            }
        }
        if j.nr_users() > 1 {
            return Err("journal shared by several file systems");
        }
        Ok(j)
    }
    pub fn csum(&self) -> Csum {
        if self.incompat & INCOMPAT_CSUM_V3 != 0 {
            Csum::V3
        } else if self.incompat & INCOMPAT_CSUM_V2 != 0 {
            Csum::V2
        } else {
            Csum::None
        }
    }
    pub fn is64(&self) -> bool {
        self.incompat & INCOMPAT_64BIT != 0
    }
    fn nr_users(&self) -> u32 {
        if gb32(&self.raw, 4) == SB_V2 { gb32(&self.raw, S_NR_USERS) } else { 1 }
    }
    /// One past the last log block (`j_last`): the fast-commit area at the end is not log.
    pub fn log_end(&self) -> u32 {
        let fc = if self.incompat & INCOMPAT_FAST_COMMIT != 0 { match gb32(&self.raw, S_NUM_FC) {
                0 => 256,
                n => n,
            } } else { 0 };
        self.maxlen.saturating_sub(fc).max(self.first + 1)
    }
    /// `j_csum_seed`.
    pub fn seed(&self) -> u32 {
        crc32c(!0, &self.uuid)
    }
    /// The bytes for writing: V2 superblock, the features, `s_start`/`s_sequence`, the checksum.
    pub fn serialise(&mut self, start: u32, sequence: u32, head: u32) -> Vec<u8> {
        self.start = start;
        self.sequence = sequence;
        let r = &mut self.raw;
        pb32(r, 4, SB_V2);
        pb32(r, S_SEQUENCE, sequence);
        pb32(r, S_START, start);
        pb32(r, S_ERRNO, 0);
        pb32(r, S_COMPAT, self.compat);
        pb32(r, S_INCOMPAT, self.incompat);
        pb32(r, S_RO_COMPAT, 0);
        if gb32(r, S_NR_USERS) == 0 {
            pb32(r, S_NR_USERS, 1);
        }
        pb32(r, S_HEAD, if start == 0 { head } else { 0 });
        if self.incompat & (INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3) != 0 {
            r[S_CSUM_TYPE] = CSUM_CRC32C;
            let c = sb_checksum(r);
            pb32(r, S_CHECKSUM, c);
        } else {
            r[S_CSUM_TYPE] = 0;
            pb32(r, S_CHECKSUM, 0);
        }
        self.raw.clone()
    }
}

/// `jbd2_superblock_csum`.
fn sb_checksum(raw: &[u8]) -> u32 {
    let mut b = raw[..SB_SIZE].to_vec();
    pb32(&mut b, S_CHECKSUM, 0);
    crc32c(!0, &b)
}

/// The tag layout of a journal (`journal_tag_bytes`) and what fits in one descriptor block.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub bs: usize,
    pub csum: Csum,
    pub is64: bool,
    pub seed: u32,
}

impl Layout {
    pub fn tag_bytes(&self) -> usize {
        match (self.csum, self.is64) {
            (Csum::V3, _) => 16,
            (Csum::V2, true) => 14,
            (Csum::V2, false) => 10,
            (Csum::None, true) => 12,
            (Csum::None, false) => 8,
        }
    }
    /// The block tail of descriptor and revoke blocks (`jbd2_journal_block_tail`).
    pub fn tail(&self) -> usize {
        if self.csum == Csum::None { 0 } else { 4 }
    }
    /// Tags per descriptor block, packed as `jbd2_journal_commit_transaction` packs them (commit.c:663-700).
    pub fn tags_per_desc(&self) -> usize {
        let tb = self.tag_bytes();
        let mut space = self.bs - HEADER;
        let mut n = 0;
        loop {
            space -= tb + if n == 0 { 16 } else { 0 };
            n += 1;
            if space < tb + 16 + self.tail() {
                return n;
            }
        }
    }
    pub fn revokes_per_block(&self) -> usize {
        (self.bs - REVOKE_HEADER - self.tail()) / self.rec()
    }
    pub fn rec(&self) -> usize {
        if self.is64 { 8 } else { 4 }
    }
    fn header(&self, kind: u32, seq: u32) -> Vec<u8> {
        let mut b = vec![0u8; self.bs];
        pb32(&mut b, 0, MAGIC);
        pb32(&mut b, 4, kind);
        pb32(&mut b, 8, seq);
        b
    }
    /// `jbd2_descriptor_block_csum_set` (also for revoke blocks).
    pub fn seal_tail(&self, b: &mut [u8]) {
        if self.csum == Csum::None {
            return;
        }
        let at = self.bs - 4;
        pb32(b, at, 0);
        let c = crc32c(self.seed, b);
        pb32(b, at, c);
    }
    pub fn tail_ok(&self, b: &[u8]) -> bool {
        if self.csum == Csum::None {
            return true;
        }
        let at = self.bs - 4;
        let mut c = b.to_vec();
        pb32(&mut c, at, 0);
        crc32c(self.seed, &c) == gb32(b, at)
    }
    /// `jbd2_block_tag_csum_set`: crc32c over the sequence (big-endian) and the logged block.
    pub fn block_csum(&self, seq: u32, data: &[u8]) -> u32 {
        crc32c(crc32c(self.seed, &seq.to_be_bytes()), data)
    }
    /// A descriptor block for `blocks` (home block numbers; `escaped[i]` when the logged copy is escaped)
    /// with their logged copies' checksums (`csums`).
    pub fn descriptor(&self, seq: u32, uuid: &[u8; 16], blocks: &[(u64, bool, u32)]) -> Vec<u8> {
        let mut b = self.header(DESCRIPTOR, seq);
        let tb = self.tag_bytes();
        let mut at = HEADER;
        for (i, &(blk, esc, csum)) in blocks.iter().enumerate() {
            let mut flags = if esc { FLAG_ESCAPE } else { 0 };
            if i > 0 {
                flags |= FLAG_SAME_UUID;
            }
            if i + 1 == blocks.len() {
                flags |= FLAG_LAST_TAG;
            }
            pb32(&mut b, at, blk as u32);
            pb16(&mut b, at + 6, flags);
            if self.is64 {
                pb32(&mut b, at + 8, (blk >> 32) as u32);
            }
            match self.csum {
                Csum::V3 => pb32(&mut b, at + 12, csum),
                Csum::V2 => pb16(&mut b, at + 4, csum as u16),
                Csum::None => {}
            }
            at += tb;
            if i == 0 {
                b[at..at + 16].copy_from_slice(uuid);
                at += 16;
            }
        }
        self.seal_tail(&mut b);
        b
    }
    /// Revoke blocks for `blocks` (`write_one_revoke_record`, revoke.c:571).
    pub fn revoke_blocks(&self, seq: u32, blocks: &[u64]) -> Vec<Vec<u8>> {
        let per = self.revokes_per_block().max(1);
        blocks
            .chunks(per)
            .map(|c| {
                let mut b = self.header(REVOKE, seq);
                let mut at = REVOKE_HEADER;
                for &blk in c {
                    if self.is64 {
                        pb32(&mut b, at, (blk >> 32) as u32);
                        pb32(&mut b, at + 4, blk as u32);
                    } else {
                        pb32(&mut b, at, blk as u32);
                    }
                    at += self.rec();
                }
                pb32(&mut b, HEADER, at as u32);
                self.seal_tail(&mut b);
                b
            })
            .collect()
    }
    /// A commit block (`journal_submit_commit_record`, `jbd2_commit_block_csum_set`).
    pub fn commit(&self, seq: u32, sec: u64, nsec: u32) -> Vec<u8> {
        let mut b = self.header(COMMIT, seq);
        b[0x30..0x38].copy_from_slice(&sec.to_be_bytes());
        pb32(&mut b, 0x38, nsec);
        if self.csum != Csum::None {
            let c = crc32c(self.seed, &b);
            pb32(&mut b, 0x10, c);
        }
        b
    }
    pub fn commit_ok(&self, b: &[u8]) -> bool {
        if self.csum == Csum::None {
            return true;
        }
        let mut c = b.to_vec();
        pb32(&mut c, 0x10, 0);
        crc32c(self.seed, &c) == gb32(b, 0x10)
    }
}

/// One tag read from a descriptor block.
#[derive(Clone, Copy, Debug)]
pub struct Tag {
    pub blk: u64,
    pub flags: u16,
    pub csum: u32,
}

/// The tags of a descriptor block (`count_tags` and the replay loop, recovery.c:202/619), never past the
/// block or its tail.
pub fn tags(l: &Layout, b: &[u8]) -> Vec<Tag> {
    let tb = l.tag_bytes();
    let size = l.bs - l.tail();
    let mut at = HEADER;
    let mut out = Vec::new();
    while at + tb <= size {
        let flags = gb16(b, at + 6);
        let hi = if l.is64 { gb32(b, at + 8) as u64 } else { 0 };
        let csum = match l.csum {
            Csum::V3 => gb32(b, at + 12),
            Csum::V2 => gb16(b, at + 4) as u32,
            Csum::None => 0,
        };
        out.push(Tag { blk: gb32(b, at) as u64 | hi << 32, flags, csum });
        at += tb;
        if flags & FLAG_SAME_UUID == 0 {
            at += 16;
        }
        if flags & FLAG_LAST_TAG != 0 {
            break;
        }
    }
    out
}

/// The home block numbers of a revoke block (`scan_revoke_records`, recovery.c:850); None when its count
/// is impossible.
pub fn revokes(l: &Layout, b: &[u8]) -> Option<Vec<u64>> {
    let count = gb32(b, HEADER) as usize;
    if count > l.bs - l.tail() {
        return None;
    }
    let mut at = REVOKE_HEADER;
    let mut out = Vec::new();
    while at + l.rec() <= count {
        out.push(if l.is64 { (gb32(b, at) as u64) << 32 | gb32(b, at + 4) as u64 } else { gb32(b, at) as u64 });
        at += l.rec();
    }
    Some(out)
}
