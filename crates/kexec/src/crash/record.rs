//! The crash page's layout (RB-2, spec/core.md §10.3; the idea of Linux `fs/pstore/ram.c`, ramoops: a record in
//! RAM that a warm reset keeps). One page from bitsboot's `CRASH_PAGE` KEEP record:
//!
//! ```text
//! 0      crash record header (64 bytes)
//!          0 magic "BITSCRSH"   8 version u16   10 header bytes u16   12 body bytes u32
//!         16 checksum u32 (FNV-1a 32 over bytes 20..64 and the body)   20 flags u32
//!         24 boot id [16]   40 uptime ns u64   48 Reload counter u32   52 generation u32
//!         56 reporting CPU u32   60 CPUs asked u16   62 CPUs that answered u16
//! 64     body: the report as UTF-8 text lines (message, backtraces, registers, the newest log lines)
//! len-64 boot marker (64 bytes): "BITSLAST", boot id [16], generation u32, Reload counter u32, uptime ns u64,
//!          checksum u32 over bytes 8..40 of the marker; RB-11: 44 checksum u32 over bytes 8..40 and 48..56,
//!          48 the kernel id (FNV-1a 64 of the KERNEL file) a Reload hands to the next kernel (0: none). A marker
//!          without it (an older kernel's) parses as before, with no kernel id.
//! ```
//!
//! Every boot reads the marker the boot before left (kept = RAM survived the reset) and writes its own; a crash
//! record is reported once (flag `REPORTED`). Little endian; host-tested.

pub const MAGIC: [u8; 8] = *b"BITSCRSH";
pub const VERSION: u16 = 1;
pub const HEADER: usize = 64;
pub const MARKER: usize = 64;
pub const MARKER_MAGIC: [u8; 8] = *b"BITSLAST";

/// Smallest crash page that can hold a record (header, marker, a few lines).
pub const MIN_PAGE: usize = 1024;

pub mod flags {
    /// The next boot has logged this record already.
    pub const REPORTED: u32 = 1;
    /// A CPU exception (a fault the kernel had no handler for) rather than a Rust panic.
    pub const EXCEPTION: u32 = 2;
    /// Written by a ktest round trip, not by a crash.
    pub const TEST: u32 = 4;
    /// The report did not fit; the body is cut.
    pub const CUT: u32 = 8;
}

/// FNV-1a 32.
#[derive(Clone, Copy, Debug)]
pub struct Fnv32(u32);

impl Fnv32 {
    pub const fn new() -> Self {
        Fnv32(0x811c_9dc5)
    }
    pub fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.0 ^= b as u32;
            self.0 = self.0.wrapping_mul(0x0100_0193);
        }
    }
    pub fn finish(&self) -> u32 {
        self.0
    }
}

impl Default for Fnv32 {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub body_len: u32,
    pub flags: u32,
    pub boot_id: [u8; 16],
    pub time_ns: u64,
    pub reload: u32,
    pub generation: u32,
    pub cpu: u32,
    pub cpus_asked: u16,
    pub cpus_answered: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// No record (cold RAM, or never written).
    NoMagic,
    UnknownVersion(u16),
    /// Lengths that do not fit the page.
    BadLength,
    /// Magic right, contents damaged (a crash while writing, or RAM decayed).
    BadChecksum,
}

/// Bytes the body may take in a page of `page_len`.
pub fn body_capacity(page_len: usize) -> usize {
    page_len.saturating_sub(HEADER + MARKER)
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

impl Header {
    /// The 64 header bytes with `checksum` in place.
    pub fn bytes(&self, checksum: u32) -> [u8; HEADER] {
        let mut b = [0u8; HEADER];
        b[0..8].copy_from_slice(&MAGIC);
        b[8..10].copy_from_slice(&VERSION.to_le_bytes());
        b[10..12].copy_from_slice(&(HEADER as u16).to_le_bytes());
        b[12..16].copy_from_slice(&self.body_len.to_le_bytes());
        b[16..20].copy_from_slice(&checksum.to_le_bytes());
        b[20..24].copy_from_slice(&self.flags.to_le_bytes());
        b[24..40].copy_from_slice(&self.boot_id);
        b[40..48].copy_from_slice(&self.time_ns.to_le_bytes());
        b[48..52].copy_from_slice(&self.reload.to_le_bytes());
        b[52..56].copy_from_slice(&self.generation.to_le_bytes());
        b[56..60].copy_from_slice(&self.cpu.to_le_bytes());
        b[60..62].copy_from_slice(&self.cpus_asked.to_le_bytes());
        b[62..64].copy_from_slice(&self.cpus_answered.to_le_bytes());
        b
    }

    /// The checksum's start: the header bytes after the checksum field. Feed the body, then `finish`.
    pub fn sum_start(&self) -> Fnv32 {
        let mut h = Fnv32::new();
        h.update(&self.bytes(0)[20..]);
        h
    }

    /// The header from the first 64 bytes of a page of `page_len` (magic, version, lengths; not the checksum).
    pub fn parse(head: &[u8], page_len: usize) -> Result<(Header, u32), RecordError> {
        if head.len() < HEADER || head[0..8] != MAGIC {
            return Err(RecordError::NoMagic);
        }
        let v = le16(head, 8);
        if v != VERSION {
            return Err(RecordError::UnknownVersion(v));
        }
        let h = Header {
            body_len: le32(head, 12),
            flags: le32(head, 20),
            boot_id: head[24..40].try_into().unwrap_or([0; 16]),
            time_ns: le64(head, 40),
            reload: le32(head, 48),
            generation: le32(head, 52),
            cpu: le32(head, 56),
            cpus_asked: le16(head, 60),
            cpus_answered: le16(head, 62),
        };
        if le16(head, 10) as usize != HEADER || h.body_len as usize > body_capacity(page_len) {
            return Err(RecordError::BadLength);
        }
        Ok((h, le32(head, 16)))
    }
}

/// A whole record from a page: header and body, checksum verified.
pub fn decode(page: &[u8]) -> Result<(Header, &[u8]), RecordError> {
    let (h, sum) = Header::parse(page, page.len())?;
    let body = &page[HEADER..HEADER + h.body_len as usize];
    let mut f = h.sum_start();
    f.update(body);
    if f.finish() != sum {
        return Err(RecordError::BadChecksum);
    }
    Ok((h, body))
}

/// Writes a record into `page` (tests and the ktest round trip; the panic path streams the body itself).
pub fn encode(page: &mut [u8], h: &Header, body: &[u8]) -> Option<()> {
    if body.len() > body_capacity(page.len()) {
        return None;
    }
    let h = Header { body_len: body.len() as u32, ..*h };
    page[HEADER..HEADER + body.len()].copy_from_slice(body);
    let mut f = h.sum_start();
    f.update(body);
    page[..HEADER].copy_from_slice(&h.bytes(f.finish()));
    Some(())
}

/// What the boot before left at the page's end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marker {
    pub boot_id: [u8; 16],
    pub generation: u32,
    pub reload: u32,
    pub time_ns: u64,
    /// RB-11: the kernel id this boot's Reload is about to start (written right before the jump; 0: none).
    pub next_kernel: u64,
}

fn next_sum(b: &[u8]) -> u32 {
    let mut f = Fnv32::new();
    f.update(&b[8..40]);
    f.update(&b[48..56]);
    f.finish()
}

impl Marker {
    pub fn bytes(&self) -> [u8; MARKER] {
        let mut b = [0u8; MARKER];
        b[0..8].copy_from_slice(&MARKER_MAGIC);
        b[8..24].copy_from_slice(&self.boot_id);
        b[24..28].copy_from_slice(&self.generation.to_le_bytes());
        b[28..32].copy_from_slice(&self.reload.to_le_bytes());
        b[32..40].copy_from_slice(&self.time_ns.to_le_bytes());
        let mut f = Fnv32::new();
        f.update(&b[8..40]);
        b[40..44].copy_from_slice(&f.finish().to_le_bytes());
        if self.next_kernel != 0 {
            b[48..56].copy_from_slice(&self.next_kernel.to_le_bytes());
            let s = next_sum(&b);
            b[44..48].copy_from_slice(&s.to_le_bytes());
        }
        b
    }

    pub fn parse(b: &[u8]) -> Option<Marker> {
        if b.len() < MARKER || b[0..8] != MARKER_MAGIC {
            return None;
        }
        let mut f = Fnv32::new();
        f.update(&b[8..40]);
        if f.finish() != le32(b, 40) {
            return None;
        }
        let next_kernel = if le64(b, 48) != 0 && next_sum(b) == le32(b, 44) { le64(b, 48) } else { 0 };
        Some(Marker { boot_id: b[8..24].try_into().ok()?, generation: le32(b, 24), reload: le32(b, 28), time_ns: le64(b, 32), next_kernel })
    }
}

/// RB-11: the running kernel's id as the Reload that started it handed it on: only a marker of this same boot,
/// written by the kernel one Reload before (`my_reload` is this kernel's Reload counter), names this kernel.
pub fn handed_kernel(found: MarkerFound, m: Option<&Marker>, my_reload: u32) -> Option<u64> {
    let m = m?;
    (found == MarkerFound::SameBoot && my_reload > 0 && m.reload.checked_add(1) == Some(my_reload) && m.next_kernel != 0)
        .then_some(m.next_kernel)
}

/// What a boot concludes from the marker it found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerFound {
    /// None: a cold start (RAM lost its contents) or the first boot with this page.
    Absent,
    /// From this same boot (a Reload hands the boot id on): no reset happened.
    SameBoot,
    /// From an earlier boot: RAM kept the page across a reset.
    Kept,
}

pub fn judge(found: Option<&Marker>, my_boot_id: &[u8; 16]) -> MarkerFound {
    match found {
        None => MarkerFound::Absent,
        Some(m) if &m.boot_id == my_boot_id => MarkerFound::SameBoot,
        Some(_) => MarkerFound::Kept,
    }
}

impl MarkerFound {
    pub fn name(self) -> &'static str {
        match self {
            MarkerFound::Absent => "absent",
            MarkerFound::SameBoot => "same_boot",
            MarkerFound::Kept => "kept",
        }
    }
}
