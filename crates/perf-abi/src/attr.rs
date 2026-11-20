//! `struct perf_event_attr` (linux-7.3.1/include/uapi/linux/perf_event.h:386) as `perf_copy_attr` reads it
//! (kernel/events/core.c:13579): a short attribute is zero-extended, a longer one must be zero past what this
//! kernel knows (`copy_struct_from_user`), the reserved bits must be clear, unknown sample and read bits are
//! EINVAL.

/// The attribute size of linux-7.3.1 (`PERF_ATTR_SIZE_VER9`, with config4).
pub const SIZE: usize = 144;
pub const SIZE_VER0: u32 = 64;

/// Why the attribute was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrError {
    /// E2BIG: a size outside [VER0, page] or non-zero bytes past `SIZE`; the caller writes `SIZE` back.
    TooBig,
    /// EINVAL: reserved bits, unknown sample or read bits, inherit_thread without inherit, and the like.
    Invalid,
}

/// The fields of the attribute (only the ones the kernel looks at get names; the rest keep their raw value).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attr {
    pub kind: u32,
    pub size: u32,
    pub config: u64,
    /// `sample_period` or `sample_freq` (with `freq`).
    pub sample: u64,
    pub sample_type: u64,
    pub read_format: u64,
    pub flags: u64,
    /// `wakeup_events` or `wakeup_watermark` (with `watermark`).
    pub wakeup: u32,
    pub bp_type: u32,
    pub config1: u64,
    pub config2: u64,
    pub branch_sample_type: u64,
    pub sample_regs_user: u64,
    pub sample_stack_user: u32,
    pub clockid: i32,
    pub sample_regs_intr: u64,
    pub aux_watermark: u32,
    pub sample_max_stack: u16,
    pub aux_sample_size: u32,
    pub aux_action: u32,
    pub sig_data: u64,
    pub config3: u64,
    pub config4: u64,
}

/// Bits of `flags`.
pub mod flag {
    pub const DISABLED: u64 = 1 << 0;
    pub const INHERIT: u64 = 1 << 1;
    pub const PINNED: u64 = 1 << 2;
    pub const EXCLUSIVE: u64 = 1 << 3;
    pub const EXCLUDE_USER: u64 = 1 << 4;
    pub const EXCLUDE_KERNEL: u64 = 1 << 5;
    pub const EXCLUDE_HV: u64 = 1 << 6;
    pub const EXCLUDE_IDLE: u64 = 1 << 7;
    pub const MMAP: u64 = 1 << 8;
    pub const COMM: u64 = 1 << 9;
    pub const FREQ: u64 = 1 << 10;
    pub const INHERIT_STAT: u64 = 1 << 11;
    pub const ENABLE_ON_EXEC: u64 = 1 << 12;
    pub const TASK: u64 = 1 << 13;
    pub const WATERMARK: u64 = 1 << 14;
    pub const MMAP_DATA: u64 = 1 << 17;
    pub const SAMPLE_ID_ALL: u64 = 1 << 18;
    pub const EXCLUDE_CALLCHAIN_KERNEL: u64 = 1 << 21;
    pub const EXCLUDE_CALLCHAIN_USER: u64 = 1 << 22;
    pub const MMAP2: u64 = 1 << 23;
    pub const COMM_EXEC: u64 = 1 << 24;
    pub const USE_CLOCKID: u64 = 1 << 25;
    pub const CONTEXT_SWITCH: u64 = 1 << 26;
    pub const WRITE_BACKWARD: u64 = 1 << 27;
    pub const NAMESPACES: u64 = 1 << 28;
    pub const AUX_OUTPUT: u64 = 1 << 31;
    pub const TEXT_POKE: u64 = 1 << 33;
    pub const BUILD_ID: u64 = 1 << 34;
    pub const INHERIT_THREAD: u64 = 1 << 35;
    pub const REMOVE_ON_EXEC: u64 = 1 << 36;
    pub const SIGTRAP: u64 = 1 << 37;
    /// `__reserved_1`: bits 40..63.
    pub const RESERVED: u64 = !((1 << 40) - 1);
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

impl Attr {
    pub fn has(&self, f: u64) -> bool {
        self.flags & f != 0
    }

    /// The size field as the caller gave it (`size` 0 is VER0, the ABI quirk); E2BIG outside [VER0, page].
    pub fn effective_size(raw: u32, page: u32) -> Result<u32, AttrError> {
        let size = if raw == 0 { SIZE_VER0 } else { raw };
        if size < SIZE_VER0 || size > page { Err(AttrError::TooBig) } else { Ok(size) }
    }

    /// Parses the `size` bytes the caller handed in (`bytes.len() == size`), in `perf_copy_attr`'s order.
    pub fn parse(bytes: &[u8]) -> Result<Attr, AttrError> {
        if bytes.len() > SIZE && bytes[SIZE..].iter().any(|&b| b != 0) {
            return Err(AttrError::TooBig);
        }
        let mut b = [0u8; SIZE];
        let n = bytes.len().min(SIZE);
        b[..n].copy_from_slice(&bytes[..n]);
        let a = Attr {
            kind: u32_at(&b, 0),
            size: bytes.len() as u32,
            config: u64_at(&b, 8),
            sample: u64_at(&b, 16),
            sample_type: u64_at(&b, 24),
            read_format: u64_at(&b, 32),
            flags: u64_at(&b, 40),
            wakeup: u32_at(&b, 48),
            bp_type: u32_at(&b, 52),
            config1: u64_at(&b, 56),
            config2: u64_at(&b, 64),
            branch_sample_type: u64_at(&b, 72),
            sample_regs_user: u64_at(&b, 80),
            sample_stack_user: u32_at(&b, 88),
            clockid: u32_at(&b, 92) as i32,
            sample_regs_intr: u64_at(&b, 96),
            aux_watermark: u32_at(&b, 104),
            sample_max_stack: u16_at(&b, 108),
            aux_sample_size: u32_at(&b, 112),
            aux_action: u32_at(&b, 116),
            sig_data: u64_at(&b, 120),
            config3: u64_at(&b, 128),
            config4: u64_at(&b, 136),
        };
        let reserved_2 = u16_at(&b, 110);
        // aux_action: bits 3..31 are `__reserved_3`.
        if a.flags & flag::RESERVED != 0 || reserved_2 != 0 || a.aux_action >> 3 != 0 {
            return Err(AttrError::Invalid);
        }
        if a.sample_type & !(crate::SAMPLE_MAX - 1) != 0 || a.read_format & !(crate::FORMAT_MAX - 1) != 0 {
            return Err(AttrError::Invalid);
        }
        if a.sample_type & crate::SAMPLE_STACK_USER != 0 && (a.sample_stack_user >= u16::MAX as u32 || a.sample_stack_user % 8 != 0) {
            return Err(AttrError::Invalid);
        }
        if a.sample_type & crate::SAMPLE_WEIGHT != 0 && a.sample_type & crate::SAMPLE_WEIGHT_STRUCT != 0 {
            return Err(AttrError::Invalid);
        }
        // No cgroup support (Linux without CONFIG_CGROUP_PERF answers the same).
        if a.sample_type & crate::SAMPLE_CGROUP != 0 {
            return Err(AttrError::Invalid);
        }
        if !a.has(flag::INHERIT) && a.has(flag::INHERIT_THREAD) {
            return Err(AttrError::Invalid);
        }
        if a.has(flag::REMOVE_ON_EXEC) && a.has(flag::ENABLE_ON_EXEC) {
            return Err(AttrError::Invalid);
        }
        if a.has(flag::SIGTRAP) && !a.has(flag::REMOVE_ON_EXEC) {
            return Err(AttrError::Invalid);
        }
        Ok(a)
    }

    /// `sample_max_stack`, 0 meaning the sysctl's value.
    pub fn max_stack(&self, sysctl: u64) -> u64 {
        if self.sample_max_stack == 0 { sysctl } else { self.sample_max_stack as u64 }
    }

    /// A sampling event (`is_sampling_event`: a non-zero period or frequency).
    pub fn sampling(&self) -> bool {
        self.sample != 0
    }

    /// The bytes of the fields the sample-id trailer (`sample_id_all`) appends to non-sample records.
    pub fn id_trailer_size(&self) -> usize {
        let t = self.sample_type;
        [crate::SAMPLE_TID, crate::SAMPLE_TIME, crate::SAMPLE_ID, crate::SAMPLE_STREAM_ID, crate::SAMPLE_CPU, crate::SAMPLE_IDENTIFIER]
            .iter()
            .filter(|&&b| t & b != 0)
            .count()
            * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(size: usize) -> [u8; 256] {
        let mut b = [0u8; 256];
        b[4..8].copy_from_slice(&(size as u32).to_le_bytes());
        b
    }

    #[test]
    fn sizes() {
        assert_eq!(Attr::effective_size(0, 4096), Ok(64));
        assert_eq!(Attr::effective_size(63, 4096), Err(AttrError::TooBig));
        assert_eq!(Attr::effective_size(4097, 4096), Err(AttrError::TooBig));
        assert_eq!(Attr::effective_size(200, 4096), Ok(200));
    }

    #[test]
    fn longer_than_known_must_be_zero() {
        let mut b = raw(200);
        assert!(Attr::parse(&b[..200]).is_ok());
        b[150] = 1;
        assert_eq!(Attr::parse(&b[..200]), Err(AttrError::TooBig));
    }

    #[test]
    fn fields_and_reserved() {
        let mut b = raw(SIZE);
        b[0] = 1;
        b[8] = 9;
        b[16..24].copy_from_slice(&1000u64.to_le_bytes());
        b[24] = 0x87;
        b[40..48].copy_from_slice(&(flag::FREQ | flag::DISABLED).to_le_bytes());
        b[108] = 5;
        let a = Attr::parse(&b[..SIZE]).unwrap();
        assert_eq!((a.kind, a.config, a.sample, a.sample_type, a.sample_max_stack), (1, 9, 1000, 0x87, 5));
        assert!(a.has(flag::FREQ) && a.has(flag::DISABLED) && !a.has(flag::INHERIT));
        // VER0: the fields past 64 bytes read as zero.
        let v0 = Attr::parse(&b[..64]).unwrap();
        assert_eq!(v0.sample_max_stack, 0);
        let mut r = b;
        r[47] = 0x80;
        assert_eq!(Attr::parse(&r[..SIZE]), Err(AttrError::Invalid));
        let mut r = b;
        r[110] = 1;
        assert_eq!(Attr::parse(&r[..SIZE]), Err(AttrError::Invalid));
        let mut r = b;
        r[27] = 0x02; // sample bit 25
        assert_eq!(Attr::parse(&r[..SIZE]), Err(AttrError::Invalid));
        let mut r = b;
        r[32] = 0x20; // read bit 5
        assert_eq!(Attr::parse(&r[..SIZE]), Err(AttrError::Invalid));
        let mut r = b;
        r[40..48].copy_from_slice(&flag::INHERIT_THREAD.to_le_bytes());
        assert_eq!(Attr::parse(&r[..SIZE]), Err(AttrError::Invalid));
    }

    #[test]
    fn trailer_size() {
        let a = Attr { sample_type: crate::SAMPLE_TID | crate::SAMPLE_TIME | crate::SAMPLE_CPU | crate::SAMPLE_IP, ..Default::default() };
        assert_eq!(a.id_trailer_size(), 24);
    }
}
