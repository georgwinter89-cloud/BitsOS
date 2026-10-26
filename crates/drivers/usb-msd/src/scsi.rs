//! SCSI commands (SPC-4, SBC-3) the driver sends and the answers it reads. Pure byte work, no I/O: command
//! descriptor blocks, INQUIRY / READ CAPACITY / MODE SENSE / VPD parsing, and what a sense code means for the
//! request that got it (`classify`, after Linux `scsi_io_completion` and V1 `usb/msdio.asm:128-216`).
//! Every parser takes bytes from a device and never panics on short or odd answers.

/// Operation codes.
pub mod op {
    pub const TEST_UNIT_READY: u8 = 0x00;
    pub const REQUEST_SENSE: u8 = 0x03;
    pub const INQUIRY: u8 = 0x12;
    pub const MODE_SENSE_6: u8 = 0x1A;
    pub const READ_CAPACITY_10: u8 = 0x25;
    pub const READ_10: u8 = 0x28;
    pub const WRITE_10: u8 = 0x2A;
    pub const SYNCHRONIZE_CACHE_10: u8 = 0x35;
    pub const UNMAP: u8 = 0x42;
    pub const READ_16: u8 = 0x88;
    pub const WRITE_16: u8 = 0x8A;
    pub const SERVICE_ACTION_IN_16: u8 = 0x9E;
    pub const SAI_READ_CAPACITY_16: u8 = 0x10;
}

/// Sense keys (SPC-4 Table 51).
pub mod key {
    pub const NO_SENSE: u8 = 0x0;
    pub const RECOVERED_ERROR: u8 = 0x1;
    pub const NOT_READY: u8 = 0x2;
    pub const MEDIUM_ERROR: u8 = 0x3;
    pub const HARDWARE_ERROR: u8 = 0x4;
    pub const ILLEGAL_REQUEST: u8 = 0x5;
    pub const UNIT_ATTENTION: u8 = 0x6;
    pub const DATA_PROTECT: u8 = 0x7;
    pub const ABORTED_COMMAND: u8 = 0xB;
}

/// A command descriptor block: up to 16 bytes, `len` of them used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cdb {
    pub bytes: [u8; 16],
    pub len: u8,
}

impl Cdb {
    fn of(b: &[u8]) -> Cdb {
        let mut bytes = [0u8; 16];
        let n = b.len().min(16);
        bytes[..n].copy_from_slice(&b[..n]);
        Cdb { bytes, len: n as u8 }
    }
    pub fn opcode(&self) -> u8 {
        self.bytes[0]
    }
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

pub fn test_unit_ready() -> Cdb {
    Cdb::of(&[op::TEST_UNIT_READY, 0, 0, 0, 0, 0])
}

/// Fixed-format sense of `len` bytes (18 is what every device knows; Linux `US_SENSE_SIZE`).
pub fn request_sense(len: u8) -> Cdb {
    Cdb::of(&[op::REQUEST_SENSE, 0, 0, 0, len, 0])
}

/// Standard INQUIRY data, `len` bytes (36 = the part every device has; Linux asks 36 first too).
pub fn inquiry(len: u8) -> Cdb {
    Cdb::of(&[op::INQUIRY, 0, 0, 0, len, 0])
}

/// INQUIRY of a vital product data page.
pub fn inquiry_vpd(page: u8, len: u16) -> Cdb {
    let l = len.to_be_bytes();
    Cdb::of(&[op::INQUIRY, 1, page, l[0], l[1], 0])
}

/// MODE SENSE(6) of all pages (0x3F), `len` bytes (Linux sd asks 192 first for USB devices).
pub fn mode_sense6(len: u8) -> Cdb {
    Cdb::of(&[op::MODE_SENSE_6, 0, 0x3F, 0, len, 0])
}

pub fn read_capacity10() -> Cdb {
    Cdb::of(&[op::READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0])
}

/// READ CAPACITY(16): 32 bytes of answer.
pub fn read_capacity16() -> Cdb {
    Cdb::of(&[op::SERVICE_ACTION_IN_16, op::SAI_READ_CAPACITY_16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 0, 0])
}

pub fn synchronize_cache10() -> Cdb {
    Cdb::of(&[op::SYNCHRONIZE_CACHE_10, 0, 0, 0, 0, 0, 0, 0, 0, 0])
}

/// UNMAP with a parameter list of `param_len` bytes (`unmap_params`).
pub fn unmap(param_len: u16) -> Cdb {
    let l = param_len.to_be_bytes();
    Cdb::of(&[op::UNMAP, 0, 0, 0, 0, 0, 0, l[0], l[1], 0])
}

/// Most descriptors one UNMAP parameter list can carry: the list length (CDB bytes 7-8) and the block
/// descriptor data length (header bytes 2-3) are 16-bit fields, and 8 + 16 * 4095 = 65528 is the largest
/// list where all three lengths fit (4096 descriptors make 65544 bytes, which wraps).
pub const UNMAP_MAX_DESCRIPTORS: usize = (u16::MAX as usize - 8) / 16;

/// The UNMAP parameter list for `ranges` (LBA, blocks): header + 16 bytes per descriptor (SBC-3 5.28.2).
/// `None` for more than `UNMAP_MAX_DESCRIPTORS` ranges (a length would not fit its 16-bit field).
pub fn unmap_params(ranges: &[(u64, u32)]) -> Option<alloc::vec::Vec<u8>> {
    if ranges.len() > UNMAP_MAX_DESCRIPTORS {
        return None;
    }
    let n = ranges.len() * 16;
    let mut v = alloc::vec::Vec::with_capacity(8 + n);
    v.extend_from_slice(&u16::try_from(n + 6).ok()?.to_be_bytes());
    v.extend_from_slice(&u16::try_from(n).ok()?.to_be_bytes());
    v.extend_from_slice(&[0; 4]);
    for &(lba, blocks) in ranges {
        v.extend_from_slice(&lba.to_be_bytes());
        v.extend_from_slice(&blocks.to_be_bytes());
        v.extend_from_slice(&[0; 4]);
    }
    Some(v)
}

/// READ or WRITE of `blocks` blocks at `lba`: the 10-byte form while LBA and count fit (32-bit LBA, 16-bit
/// count), else the 16-byte form; `force16` for disks past 2^32 blocks (Linux `use_16_for_rw`). `fua` sets
/// Force Unit Access (byte 1 bit 3) on writes.
pub fn rw(write: bool, lba: u64, blocks: u32, fua: bool, force16: bool) -> Cdb {
    let fua_bit = if write && fua { 0x08 } else { 0 };
    let end = lba.saturating_add(blocks as u64);
    if !force16 && blocks <= 0xFFFF && end <= 0x1_0000_0000 {
        let l = (lba as u32).to_be_bytes();
        let n = (blocks as u16).to_be_bytes();
        let opc = if write { op::WRITE_10 } else { op::READ_10 };
        Cdb::of(&[opc, fua_bit, l[0], l[1], l[2], l[3], 0, n[0], n[1], 0])
    } else {
        let l = lba.to_be_bytes();
        let n = blocks.to_be_bytes();
        let opc = if write { op::WRITE_16 } else { op::READ_16 };
        Cdb::of(&[opc, fua_bit, l[0], l[1], l[2], l[3], l[4], l[5], l[6], l[7], n[0], n[1], n[2], n[3], 0, 0])
    }
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Sense key, additional sense code and qualifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
}

impl Sense {
    /// Fixed (70h/71h) or descriptor (72h/73h) format; `None` if the bytes are neither (or too short).
    pub fn parse(b: &[u8]) -> Option<Sense> {
        let code = b.first()? & 0x7F;
        match code {
            0x70 | 0x71 => {
                let key = b.get(2)? & 0x0F;
                // ASC/ASCQ only if the additional length reaches them (byte 7 = bytes after byte 7).
                let extra = *b.get(7).unwrap_or(&0) as usize;
                let (asc, ascq) = if extra >= 6 { (*b.get(12).unwrap_or(&0), *b.get(13).unwrap_or(&0)) } else { (0, 0) };
                Some(Sense { key, asc, ascq })
            }
            0x72 | 0x73 => Some(Sense { key: b.get(1)? & 0x0F, asc: *b.get(2).unwrap_or(&0), ascq: *b.get(3).unwrap_or(&0) }),
            _ => None,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.key == 0 && self.asc == 0 && self.ascq == 0
    }
}

/// What a request does after a CHECK CONDITION with this sense.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// RECOVERED ERROR: the command worked.
    Done,
    /// Try the same command again (UNIT ATTENTION other than a medium change, ABORTED COMMAND, empty sense).
    Retry,
    /// NOT READY, becoming ready (04h/01h) or in progress: wait a little and try again.
    WaitRetry,
    /// UNIT ATTENTION 28h: the medium may have changed (new generation; size read again).
    MediumChanged,
    /// NOT READY 3Ah: no medium.
    NoMedium,
    /// DATA PROTECT: the medium is write-protected.
    WriteProtected,
    /// ILLEGAL REQUEST: the command or a field is not supported; no retry.
    Illegal,
    /// MEDIUM ERROR, HARDWARE ERROR, NOT READY that will not change, anything else: fails.
    Fail,
}

pub fn classify(s: Sense) -> Action {
    match s.key {
        key::NO_SENSE => Action::Retry,
        key::RECOVERED_ERROR => Action::Done,
        key::NOT_READY => match (s.asc, s.ascq) {
            (0x3A, _) => Action::NoMedium,
            // 04/01 becoming ready, 04/04 format in progress, 04/07..08 operation in progress, 04/00 cause not
            // reportable (cheap sticks after power-up): wait.
            (0x04, 0x00 | 0x01 | 0x04 | 0x07 | 0x08) => Action::WaitRetry,
            // 04/02 "initializing command required" (a START STOP UNIT that sticks never need): Linux sends it;
            // treated as a wait here, bounded by the caller's readiness window.
            (0x04, 0x02) => Action::WaitRetry,
            _ => Action::Fail,
        },
        key::UNIT_ATTENTION => match s.asc {
            0x28 => Action::MediumChanged,
            _ => Action::Retry,
        },
        key::DATA_PROTECT => Action::WriteProtected,
        key::ILLEGAL_REQUEST => Action::Illegal,
        key::ABORTED_COMMAND => Action::Retry,
        _ => Action::Fail,
    }
}

/// The part of standard INQUIRY data the driver uses.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Inquiry {
    pub qualifier: u8,
    pub device_type: u8,
    pub removable: bool,
    /// SPC version (5 = SPC-3, 6 = SPC-4; sticks often say 0, 2 or 4).
    pub version: u8,
    pub vendor: alloc::string::String,
    pub product: alloc::string::String,
    pub revision: alloc::string::String,
}

fn text(b: &[u8], r: core::ops::Range<usize>) -> alloc::string::String {
    let s = b.get(r).unwrap_or(&[]);
    let mut t: alloc::string::String = s.iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { ' ' }).collect();
    while t.ends_with(' ') {
        t.pop();
    }
    t
}

impl Inquiry {
    /// At least 5 bytes (qualifier/type, RMB, version); the strings may be missing.
    pub fn parse(b: &[u8]) -> Option<Inquiry> {
        if b.len() < 5 {
            return None;
        }
        Some(Inquiry {
            qualifier: b[0] >> 5,
            device_type: b[0] & 0x1F,
            removable: b[1] & 0x80 != 0,
            version: b[2],
            vendor: text(b, 8..16),
            product: text(b, 16..32),
            revision: text(b, 32..36),
        })
    }
    /// A direct-access block device that is connected (qualifier 0, type 0).
    pub fn is_disk(&self) -> bool {
        self.qualifier == 0 && self.device_type == 0
    }
}

/// READ CAPACITY(10)/(16) answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Capacity {
    pub last_lba: u64,
    pub block_size: u32,
    /// (16) only: logical block provisioning enabled (thin, UNMAP may work).
    pub lbpme: bool,
}

impl Capacity {
    pub fn parse10(b: &[u8]) -> Option<Capacity> {
        Some(Capacity { last_lba: be32(b, 0)? as u64, block_size: be32(b, 4)?, lbpme: false })
    }
    pub fn parse16(b: &[u8]) -> Option<Capacity> {
        Some(Capacity { last_lba: be64(b, 0)?, block_size: be32(b, 8)?, lbpme: b.get(14).is_some_and(|x| x & 0x80 != 0) })
    }
    /// READ CAPACITY(10) says the disk is too big for it (SBC-3: last LBA 0xFFFFFFFF -> use (16)).
    pub fn needs16(&self) -> bool {
        self.last_lba == 0xFFFF_FFFF
    }
    /// Logical block sizes the driver accepts (Linux sd: 512, 1024, 2048, 4096; and powers of two up to 64 KiB
    /// would work the same, but no device uses them).
    pub fn block_size_ok(&self) -> bool {
        matches!(self.block_size, 512 | 1024 | 2048 | 4096)
    }
    pub fn blocks(&self) -> u64 {
        self.last_lba.saturating_add(1)
    }
}

/// MODE SENSE(6) header: the device-specific byte (SBC-3 6.4.1: bit 7 WP, bit 4 DPOFUA).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ModeHeader {
    pub write_protected: bool,
    pub dpofua: bool,
}

impl ModeHeader {
    pub fn parse6(b: &[u8]) -> Option<ModeHeader> {
        let d = *b.get(2)?;
        Some(ModeHeader { write_protected: d & 0x80 != 0, dpofua: d & 0x10 != 0 })
    }
}

/// Block Limits VPD page (B0h): the UNMAP limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct BlockLimits {
    pub max_unmap_lba_count: u32,
    pub max_unmap_descriptors: u32,
}

impl BlockLimits {
    pub fn parse(b: &[u8]) -> Option<BlockLimits> {
        if b.get(1) != Some(&0xB0) {
            return None;
        }
        Some(BlockLimits { max_unmap_lba_count: be32(b, 20).unwrap_or(0), max_unmap_descriptors: be32(b, 24).unwrap_or(0) })
    }
}

/// Logical Block Provisioning VPD page (B2h): LBPU = UNMAP supported.
pub fn lbp_unmap(b: &[u8]) -> bool {
    b.get(1) == Some(&0xB2) && b.get(5).is_some_and(|x| x & 0x80 != 0)
}

/// Supported VPD pages (00h): whether `page` is listed.
pub fn vpd_lists(b: &[u8], page: u8) -> bool {
    if b.get(1) != Some(&0x00) {
        return false;
    }
    let n = u16::from_be_bytes([*b.get(2).unwrap_or(&0), *b.get(3).unwrap_or(&0)]) as usize;
    b.iter().skip(4).take(n).any(|&p| p == page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_pick_the_short_form_while_it_fits() {
        let c = rw(false, 0x1234_5678, 64, false, false);
        assert_eq!(c.as_slice(), &[0x28, 0, 0x12, 0x34, 0x56, 0x78, 0, 0, 64, 0]);
        let c = rw(true, 7, 1, true, false);
        assert_eq!(c.as_slice(), &[0x2A, 0x08, 0, 0, 0, 7, 0, 0, 1, 0], "FUA bit on writes");
        assert_eq!(rw(false, 7, 1, true, false).bytes[1], 0, "no FUA on reads");
        // Past 32-bit LBAs, more than 65535 blocks, or a big disk: the 16-byte form.
        let c = rw(false, 0x1_0000_0000, 8, false, false);
        assert_eq!(c.as_slice(), &[0x88, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0]);
        assert_eq!(rw(false, 0xFFFF_FFFF, 2, false, false).opcode(), 0x88, "the end crosses 2^32");
        assert_eq!(rw(false, 0xFFFF_FFFE, 2, false, false).opcode(), 0x28, "ends exactly at 2^32");
        assert_eq!(rw(true, 0, 0x1_0000, false, false).opcode(), 0x8A);
        let c = rw(true, 3, 2, true, true);
        assert_eq!((c.opcode(), c.bytes[1], c.len), (0x8A, 0x08, 16));
    }

    #[test]
    fn sense_formats_and_what_they_mean() {
        let mut f = [0u8; 18];
        f[0] = 0x70;
        f[2] = 0x06;
        f[7] = 10;
        f[12] = 0x29;
        assert_eq!(Sense::parse(&f), Some(Sense { key: 6, asc: 0x29, ascq: 0 }));
        assert_eq!(classify(Sense::parse(&f).unwrap()), Action::Retry, "power on / reset: retry");
        f[12] = 0x28;
        assert_eq!(classify(Sense::parse(&f).unwrap()), Action::MediumChanged);
        f[2] = 0x02;
        f[12] = 0x3A;
        assert_eq!(classify(Sense::parse(&f).unwrap()), Action::NoMedium);
        f[12] = 0x04;
        f[13] = 0x01;
        assert_eq!(classify(Sense::parse(&f).unwrap()), Action::WaitRetry);
        f[13] = 0x03;
        assert_eq!(classify(Sense::parse(&f).unwrap()), Action::Fail, "manual intervention required");
        f[2] = 0x07;
        assert_eq!(classify(Sense::parse(&f).unwrap()), Action::WriteProtected);
        f[2] = 0x05;
        assert_eq!(classify(Sense::parse(&f).unwrap()), Action::Illegal);
        f[2] = 0x01;
        assert_eq!(classify(Sense::parse(&f).unwrap()), Action::Done);
        // A short additional length hides ASC/ASCQ.
        let short = [0x70, 0, 0x06, 0, 0, 0, 0, 0];
        assert_eq!(Sense::parse(&short), Some(Sense { key: 6, asc: 0, ascq: 0 }));
        // Descriptor format.
        assert_eq!(Sense::parse(&[0x72, 0x02, 0x3A, 0x00]), Some(Sense { key: 2, asc: 0x3A, ascq: 0 }));
        // Garbage and empty answers never panic.
        assert_eq!(Sense::parse(&[]), None);
        assert_eq!(Sense::parse(&[0x00; 18]), None);
        assert_eq!(Sense::parse(&[0x70]), None);
        assert!(Sense::default().is_empty());
    }

    #[test]
    fn inquiry_capacity_mode_and_vpd_parsers() {
        let mut q = [0u8; 36];
        q[1] = 0x80;
        q[2] = 0x06;
        q[8..16].copy_from_slice(b"QEMU    ");
        q[16..32].copy_from_slice(b"QEMU HARDDISK   ");
        q[32..36].copy_from_slice(b"2.5+");
        let i = Inquiry::parse(&q).unwrap();
        assert!(i.is_disk() && i.removable && i.version == 6);
        assert_eq!((i.vendor.as_str(), i.product.as_str(), i.revision.as_str()), ("QEMU", "QEMU HARDDISK", "2.5+"));
        assert!(Inquiry::parse(&[0x05, 0x80, 0, 0, 0]).is_some_and(|i| !i.is_disk()), "CD-ROM is no disk");
        assert!(Inquiry::parse(&[0x7F, 0, 0, 0, 0]).is_some_and(|i| !i.is_disk()), "no unit connected");
        assert!(Inquiry::parse(&[0; 4]).is_none());

        let c = Capacity::parse10(&[0, 0, 0x7F, 0xFF, 0, 0, 2, 0]).unwrap();
        assert_eq!((c.blocks(), c.block_size, c.needs16(), c.block_size_ok()), (0x8000, 512, false, true));
        assert!(Capacity::parse10(&[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 2, 0]).unwrap().needs16());
        let mut c16 = [0u8; 32];
        c16[0..8].copy_from_slice(&0x1_7FFF_FFFFu64.to_be_bytes());
        c16[8..12].copy_from_slice(&4096u32.to_be_bytes());
        c16[14] = 0x80;
        let c = Capacity::parse16(&c16).unwrap();
        assert_eq!((c.blocks(), c.block_size, c.lbpme), (0x1_8000_0000, 4096, true));
        assert!(Capacity::parse10(&[0; 7]).is_none() && Capacity::parse16(&[0; 11]).is_none());
        assert!(!Capacity { last_lba: 0, block_size: 520, lbpme: false }.block_size_ok());
        assert_eq!(Capacity { last_lba: u64::MAX, block_size: 512, lbpme: false }.blocks(), u64::MAX, "no overflow");

        assert_eq!(ModeHeader::parse6(&[3, 0, 0x90, 0]), Some(ModeHeader { write_protected: true, dpofua: true }));
        assert_eq!(ModeHeader::parse6(&[3, 0]), None);

        let mut b0 = [0u8; 64];
        b0[1] = 0xB0;
        b0[20..24].copy_from_slice(&0x40_0000u32.to_be_bytes());
        b0[24..28].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(BlockLimits::parse(&b0), Some(BlockLimits { max_unmap_lba_count: 0x40_0000, max_unmap_descriptors: 1 }));
        assert_eq!(BlockLimits::parse(&b0[..8]).map(|l| l.max_unmap_lba_count), Some(0), "short page");
        assert!(BlockLimits::parse(&[0, 0xB2]).is_none());
        assert!(lbp_unmap(&[0, 0xB2, 0, 4, 0, 0x80, 0, 0]) && !lbp_unmap(&[0, 0xB2, 0, 4, 0, 0x40]));
        assert!(vpd_lists(&[0, 0, 0, 3, 0x00, 0x80, 0xB0], 0xB0) && !vpd_lists(&[0, 0, 0, 2, 0x00, 0x80, 0xB0], 0xB0));
    }

    #[test]
    fn unmap_parameter_list() {
        let p = unmap_params(&[(0x10, 8), (0x1_0000_0000, 1)]).unwrap();
        assert_eq!(p.len(), 8 + 32);
        assert_eq!(&p[0..8], &[0, 38, 0, 32, 0, 0, 0, 0]);
        assert_eq!(&p[8..24], &[0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 8, 0, 0, 0, 0]);
        assert_eq!(unmap(40).as_slice(), &[0x42, 0, 0, 0, 0, 0, 0, 0, 40, 0]);
    }

    /// The largest list still has exact 16-bit lengths; 4096 descriptors (65544 bytes) are refused.
    #[test]
    fn unmap_parameter_list_limit() {
        assert_eq!(UNMAP_MAX_DESCRIPTORS, 4095);
        let max: alloc::vec::Vec<(u64, u32)> = (0..4095u64).map(|i| (i * 8, 8)).collect();
        let p = unmap_params(&max).unwrap();
        assert_eq!(p.len(), 65528);
        assert_eq!(u16::from_be_bytes([p[0], p[1]]) as usize, 65528 - 2);
        assert_eq!(u16::from_be_bytes([p[2], p[3]]) as usize, 65528 - 8);
        assert_eq!(unmap(u16::try_from(p.len()).unwrap()).as_slice()[7..9], [0xFF, 0xF8]);
        let over: alloc::vec::Vec<(u64, u32)> = (0..4096u64).map(|i| (i * 8, 8)).collect();
        assert!(unmap_params(&over).is_none());
        assert_eq!(unmap_params(&[]).unwrap(), [0, 6, 0, 0, 0, 0, 0, 0]);
    }
}
