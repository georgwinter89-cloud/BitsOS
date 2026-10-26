//! Controller registers (NVMe 1.4 §3.1; linux-6.6/include/linux/nvme.h:121-240) and their fields.

pub const CAP: usize = 0x00;
pub const VS: usize = 0x08;
pub const INTMS: usize = 0x0C;
pub const CC: usize = 0x14;
pub const CSTS: usize = 0x1C;
pub const AQA: usize = 0x24;
pub const ASQ: usize = 0x28;
pub const ACQ: usize = 0x30;
/// Doorbells start here; queue `q`'s SQ tail at `DOORBELLS + 2q * stride`, its CQ head one stride later.
pub const DOORBELLS: usize = 0x1000;

pub const CC_EN: u32 = 1;
/// CSS = NVM command set (000b), bits 6:4.
pub const CC_CSS_NVM: u32 = 0;
pub const CC_MPS_SHIFT: u32 = 7;
/// Round robin arbitration (000b), bits 13:11.
pub const CC_AMS_RR: u32 = 0;
pub const CC_SHN_MASK: u32 = 3 << 14;
pub const CC_SHN_NORMAL: u32 = 1 << 14;
/// I/O SQ entry 64 bytes (2^6), CQ entry 16 bytes (2^4) (nvme.h:211-212).
pub const CC_IOSQES: u32 = 6 << 16;
pub const CC_IOCQES: u32 = 4 << 20;

pub const CSTS_RDY: u32 = 1;
pub const CSTS_CFS: u32 = 1 << 1;
pub const CSTS_SHST_MASK: u32 = 3 << 2;
pub const CSTS_SHST_CMPLT: u32 = 2 << 2;

/// The host page size this driver uses (Linux's NVME_CTRL_PAGE_SIZE): CC.MPS = 0.
pub const PAGE: usize = 4096;
pub const SQE: usize = 64;
pub const CQE: usize = 16;
/// Admin queue depth (Linux NVME_AQ_DEPTH, pci.c).
pub const ADMIN_DEPTH: u16 = 32;
/// I/O queue depth wanted (Linux's io_queue_depth default), bounded by CAP.MQES + 1.
pub const IO_DEPTH: u16 = 1024;

/// CAP decoded (nvme.h:155-160).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cap {
    pub raw: u64,
}

impl Cap {
    /// Maximum queue entries supported (1-based; MQES is 0-based).
    pub fn mqes(self) -> u32 {
        (self.raw & 0xFFFF) as u32 + 1
    }
    /// CAP.TO in 500 ms units.
    pub fn to_units(self) -> u32 {
        ((self.raw >> 24) & 0xFF) as u32
    }
    /// The ready timeout in ns: CAP.TO x 500 ms, at least 500 ms (TO = 0 seen on broken firmware).
    pub fn ready_timeout_ns(self) -> u64 {
        (self.to_units().max(1) as u64) * 500_000_000
    }
    pub fn dstrd(self) -> u32 {
        ((self.raw >> 32) & 0xF) as u32
    }
    /// Bytes between doorbells (4 << DSTRD).
    pub fn stride(self) -> usize {
        4usize << self.dstrd()
    }
    /// NVM command set supported (CSS bit 0).
    pub fn css_nvm(self) -> bool {
        (self.raw >> 37) & 1 != 0
    }
    pub fn mpsmin_shift(self) -> u32 {
        ((self.raw >> 48) & 0xF) as u32 + 12
    }
    pub fn mpsmax_shift(self) -> u32 {
        ((self.raw >> 52) & 0xF) as u32 + 12
    }
}

/// Offset of queue `qid`'s SQ tail doorbell (`cq` = false) or CQ head doorbell.
pub fn doorbell(qid: u16, cq: bool, stride: usize) -> usize {
    DOORBELLS + (2 * qid as usize + cq as usize) * stride
}

/// The enable value (linux-6.6/drivers/nvme/host/core.c:2223-2252 `nvme_enable_ctrl`) without EN.
pub fn cc_config() -> u32 {
    CC_CSS_NVM | ((PAGE.trailing_zeros() - 12) << CC_MPS_SHIFT) | CC_AMS_RR | CC_IOSQES | CC_IOCQES
}

/// "major.minor.tertiary" of VS.
pub fn version_text(vs: u32) -> alloc::string::String {
    alloc::format!("{}.{}.{}", vs >> 16, (vs >> 8) & 0xFF, vs & 0xFF)
}
