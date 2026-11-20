//! The disk slot's layout (CRASHDISK; pure, host-tested). `\BITSOS2\CRASH\SLOTS.BIN` is `SLOTS` slots of
//! `SLOT_BYTES` each, a ring: a panic writes the slot after the newest record, with the next sequence number
//! (Linux 7.3.1 fs/pstore/zone.c keeps one zone per record kind with a signature and a checksum-free length;
//! here a whole report is one record and a checksum tells a torn write from a whole one). Little endian.
//!
//! ```text
//! 0   magic "BITSCRDK"   8 version u16   10 header bytes u16 (128)   12 kind u16 (0 empty, 1 record)
//! 14  slot index u16     16 sequence u64 (records count from 1)       24 body bytes u32   28 flags u32
//! 32  boot id [16]       48 uptime ns u64   56 wall clock (unix s, 0 unknown) i64   64 generation u32
//! 68  Reload counter u32 72 reporting CPU u32   76 CPUs asked u16   78 CPUs that answered u16
//! 80  preparation tag u64 (an empty slot's, kept by the record written over it)
//! 120 checksum u64 (FNV-1a 64 over bytes 0..120 and the body)
//! 128 body: the report as UTF-8 text lines
//! ```

pub const MAGIC: [u8; 8] = *b"BITSCRDK";
pub const VERSION: u16 = 1;
pub const HEADER: usize = 128;
pub const SLOTS: usize = 4;
pub const SLOT_BYTES: usize = 256 * 1024;
pub const KIND_EMPTY: u16 = 0;
pub const KIND_RECORD: u16 = 1;

/// Flags beyond `record::flags` (same bits there: REPORTED 1 unused here, EXCEPTION 2, TEST 4, CUT 8).
pub mod flags {
    pub use crate::crash::record::flags::{CUT, EXCEPTION, TEST};
    /// A hang the watchdog would have ended (fch-wdt turned it into this panic).
    pub const HANG: u32 = 16;
    /// The RAM page's record of the same panic was cut (this one may hold more).
    pub const RAM_CUT: u32 = 32;
}

/// FNV-1a 64.
#[derive(Clone, Copy, Debug)]
pub struct Fnv64(u64);

impl Fnv64 {
    pub const fn new() -> Self {
        Fnv64(0xcbf2_9ce4_8422_2325)
    }
    pub fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Fnv64 {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Head {
    pub kind: u16,
    pub slot: u16,
    pub seq: u64,
    pub body_len: u32,
    pub flags: u32,
    pub boot_id: [u8; 16],
    pub time_ns: u64,
    pub wall_s: i64,
    pub generation: u32,
    pub reload: u32,
    pub cpu: u32,
    pub cpus_asked: u16,
    pub cpus_answered: u16,
    pub tag: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotError {
    /// Never prepared (zeros) or something else.
    NoMagic,
    UnknownVersion(u16),
    BadLength,
    /// A torn or damaged write.
    BadChecksum,
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn le64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// Bytes a body may take in a slot.
pub const fn body_capacity() -> usize {
    SLOT_BYTES - HEADER
}

impl Head {
    /// The header with checksum 0 (the checksum covers bytes 0..120).
    pub fn bytes(&self) -> [u8; HEADER] {
        let mut b = [0u8; HEADER];
        b[0..8].copy_from_slice(&MAGIC);
        b[8..10].copy_from_slice(&VERSION.to_le_bytes());
        b[10..12].copy_from_slice(&(HEADER as u16).to_le_bytes());
        b[12..14].copy_from_slice(&self.kind.to_le_bytes());
        b[14..16].copy_from_slice(&self.slot.to_le_bytes());
        b[16..24].copy_from_slice(&self.seq.to_le_bytes());
        b[24..28].copy_from_slice(&self.body_len.to_le_bytes());
        b[28..32].copy_from_slice(&self.flags.to_le_bytes());
        b[32..48].copy_from_slice(&self.boot_id);
        b[48..56].copy_from_slice(&self.time_ns.to_le_bytes());
        b[56..64].copy_from_slice(&self.wall_s.to_le_bytes());
        b[64..68].copy_from_slice(&self.generation.to_le_bytes());
        b[68..72].copy_from_slice(&self.reload.to_le_bytes());
        b[72..76].copy_from_slice(&self.cpu.to_le_bytes());
        b[76..78].copy_from_slice(&self.cpus_asked.to_le_bytes());
        b[78..80].copy_from_slice(&self.cpus_answered.to_le_bytes());
        b[80..88].copy_from_slice(&self.tag.to_le_bytes());
        b
    }

    /// The checksum's start: feed the body, then `sealed`.
    pub fn sum_start(&self) -> Fnv64 {
        let mut f = Fnv64::new();
        f.update(&self.bytes()[..120]);
        f
    }

    /// The header bytes with `sum` in place.
    pub fn sealed(&self, sum: u64) -> [u8; HEADER] {
        let mut b = self.bytes();
        b[120..128].copy_from_slice(&sum.to_le_bytes());
        b
    }

    /// Header fields and the stored checksum (not verified).
    pub fn parse(b: &[u8]) -> Result<(Head, u64), SlotError> {
        if b.len() < HEADER || b[0..8] != MAGIC {
            return Err(SlotError::NoMagic);
        }
        let v = le16(b, 8);
        if v != VERSION {
            return Err(SlotError::UnknownVersion(v));
        }
        let h = Head {
            kind: le16(b, 12),
            slot: le16(b, 14),
            seq: le64(b, 16),
            body_len: le32(b, 24),
            flags: le32(b, 28),
            boot_id: b[32..48].try_into().unwrap_or([0; 16]),
            time_ns: le64(b, 48),
            wall_s: le64(b, 56) as i64,
            generation: le32(b, 64),
            reload: le32(b, 68),
            cpu: le32(b, 72),
            cpus_asked: le16(b, 76),
            cpus_answered: le16(b, 78),
            tag: le64(b, 80),
        };
        if le16(b, 10) as usize != HEADER || h.body_len as usize > body_capacity() || h.kind > KIND_RECORD {
            return Err(SlotError::BadLength);
        }
        Ok((h, le64(b, 120)))
    }
}

/// A whole slot (header and at least the body): header and body, checksum verified.
pub fn decode(slot: &[u8]) -> Result<(Head, &[u8]), SlotError> {
    let (h, sum) = Head::parse(slot)?;
    let body = slot.get(HEADER..HEADER + h.body_len as usize).ok_or(SlotError::BadLength)?;
    // Over the bytes as stored (the reserved ones too), not as re-encoded.
    let mut f = Fnv64::new();
    f.update(&slot[..120]);
    f.update(body);
    if f.finish() != sum {
        return Err(SlotError::BadChecksum);
    }
    Ok((h, body))
}

/// A slot holding `body` (tests, the ktests, an empty slot's preparation).
pub fn encode(h: &Head, body: &[u8]) -> Option<alloc::vec::Vec<u8>> {
    if body.len() > body_capacity() {
        return None;
    }
    let h = Head { body_len: body.len() as u32, ..*h };
    let mut f = h.sum_start();
    f.update(body);
    let mut out = alloc::vec::Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(&h.sealed(f.finish()));
    out.extend_from_slice(body);
    Some(out)
}

/// The tag of an empty slot prepared by boot `boot_id`: what the extent check finds at the slot's first block.
pub fn prep_tag(boot_id: &[u8; 16], slot: usize) -> u64 {
    let mut f = Fnv64::new();
    f.update(boot_id);
    f.update(&(slot as u64).to_le_bytes());
    f.finish()
}

/// What a slot's first block says, as the boot reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seen {
    /// Zeros or foreign bytes: prepare it.
    Unprepared,
    /// Prepared, holds no record.
    Empty { tag: u64 },
    /// A record whose header parses (its checksum is checked once the whole body is read).
    Record { seq: u64 },
    /// Magic right, header wrong.
    Damaged,
}

pub fn seen(first_block: &[u8], slot: usize) -> Seen {
    match Head::parse(first_block) {
        Ok((h, _)) if h.slot as usize != slot => Seen::Damaged,
        Ok((h, _)) if h.kind == KIND_EMPTY => Seen::Empty { tag: h.tag },
        Ok((h, _)) => Seen::Record { seq: h.seq },
        Err(SlotError::NoMagic) => Seen::Unprepared,
        Err(_) => Seen::Damaged,
    }
}

/// The slot the next panic writes and its sequence number: after the newest record (by sequence), else slot 0.
pub fn next_slot(seen: &[Seen]) -> (usize, u64) {
    let newest = seen.iter().enumerate().filter_map(|(i, s)| match s {
        Seen::Record { seq } => Some((i, *seq)),
        _ => None,
    });
    match newest.max_by_key(|x| x.1) {
        Some((i, seq)) => ((i + 1) % seen.len().max(1), seq.saturating_add(1)),
        None => (0, 1),
    }
}
