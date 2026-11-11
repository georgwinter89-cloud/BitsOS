//! The command builder (R8a, NV-1, NV-2p): the only way to make a submission entry. On the I/O side Flush, Read
//! and, only with a `WriteWindow` and only inside it, Write and Dataset Management (Write Uncorrectable 04h, Write
//! Zeroes 08h, Copy 19h, the zone sends 79h/7Dh do not exist here); on the admin side only queue set-up, Identify,
//! Abort (BL-1: a timed-out I/O command), Get Log Page, Get Features and Set Features for the queue count (Format NVM, Sanitize, namespace management,
//! firmware download/commit are not implemented). The window check is the last thing before a command exists, so
//! no bug above can write outside it.
//! Admin and I/O commands are different types, so an admin opcode (01h = Create I/O SQ) can never be placed
//! in an I/O queue, where the same byte means Write.
//! Opcodes: linux-6.6/include/linux/nvme.h:821-840 (I/O), :1136-1165 (admin).

/// Admin opcodes this driver can build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AdminOp {
    CreateSq = 0x01,
    GetLogPage = 0x02,
    CreateCq = 0x05,
    Abort = 0x08,
    Identify = 0x06,
    SetFeatures = 0x09,
    GetFeatures = 0x0A,
}

/// I/O opcodes this driver can build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum IoOp {
    Flush = 0x00,
    Write = 0x01,
    Read = 0x02,
    Dsm = 0x09,
}

pub const ADMIN_OPS: [AdminOp; 7] =
    [AdminOp::CreateSq, AdminOp::GetLogPage, AdminOp::CreateCq, AdminOp::Abort, AdminOp::Identify, AdminOp::SetFeatures, AdminOp::GetFeatures];
pub const IO_OPS: [IoOp; 4] = [IoOp::Flush, IoOp::Write, IoOp::Read, IoOp::Dsm];

/// I/O opcodes that change media (NVM command set): Write, Write Uncorrectable, Write Zeroes, Dataset
/// Management, Copy, Zone Management Send, Zone Append. Without an armed window none of them is ever placed in
/// a queue (the ktest and host tests assert it).
pub const MEDIA_WRITE_IO_OPS: [u8; 7] = [0x01, 0x04, 0x08, 0x09, 0x19, 0x79, 0x7D];
/// Of those, the ones the builder has no constructor for at all.
pub const NEVER_BUILT_IO_OPS: [u8; 5] = [0x04, 0x08, 0x19, 0x79, 0x7D];
/// CDW12 bit 30, Force Unit Access (linux-6.6/include/linux/nvme.h `NVME_RW_FUA` = 1 << 14 in the upper half).
pub const CDW12_FUA: u32 = 1 << 30;
/// CDW11 bit 2 of Dataset Management: deallocate (nvme.h `NVME_DSMGMT_AD`).
pub const DSM_DEALLOCATE: u32 = 1 << 2;
/// Identify Controller ONCS bit 2: Dataset Management supported (nvme.h `NVME_CTRL_ONCS_DSM`).
pub const ONCS_DSM: u16 = 1 << 2;
/// One Dataset Management range (nvme.h `struct nvme_dsm_range`).
pub const DSM_RANGE_BYTES: usize = 16;
/// Admin opcodes that change media or firmware (never built): FW commit/download, namespace management and
/// attachment, Format NVM, Security Send, Sanitize.
pub const MEDIA_WRITE_ADMIN_OPS: [u8; 7] = [0x10, 0x11, 0x0D, 0x15, 0x80, 0x81, 0x84];

/// Feature IDs (nvme.h:1205-1230): only Number of Queues may be set; Volatile Write Cache may be read.
pub const FEAT_VOLATILE_WC: u8 = 0x06;
pub const FEAT_NUM_QUEUES: u8 = 0x07;
/// Log page ids.
pub const LOG_ERROR: u8 = 0x01;
pub const LOG_SMART: u8 = 0x02;
/// Identify CNS values.
pub const CNS_NS: u8 = 0x00;
pub const CNS_CTRL: u8 = 0x01;
pub const CNS_NS_ACTIVE_LIST: u8 = 0x02;

/// A submission entry's dwords, private to this module.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sqe {
    opcode: u8,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw: [u32; 6],
}

impl Sqe {
    fn encode(&self, cid: u16) -> [u8; 64] {
        let mut b = [0u8; 64];
        // CDW0: opcode 7:0, FUSE 9:8 = 0, PSDT 15:14 = 0 (PRPs), CID 31:16.
        b[0] = self.opcode;
        b[2..4].copy_from_slice(&cid.to_le_bytes());
        b[4..8].copy_from_slice(&self.nsid.to_le_bytes());
        b[24..32].copy_from_slice(&self.prp1.to_le_bytes());
        b[32..40].copy_from_slice(&self.prp2.to_le_bytes());
        for (i, d) in self.cdw.iter().enumerate() {
            b[40 + 4 * i..44 + 4 * i].copy_from_slice(&d.to_le_bytes());
        }
        b
    }
}

/// What a queue accepts.
pub trait Encode {
    fn encode(&self, cid: u16) -> [u8; 64];
    fn opcode(&self) -> u8;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdminCmd(Sqe);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoCmd(Sqe);

impl Encode for AdminCmd {
    fn encode(&self, cid: u16) -> [u8; 64] {
        self.0.encode(cid)
    }
    fn opcode(&self) -> u8 {
        self.0.opcode
    }
}
impl Encode for IoCmd {
    fn encode(&self, cid: u16) -> [u8; 64] {
        self.0.encode(cid)
    }
    fn opcode(&self) -> u8 {
        self.0.opcode
    }
}

fn admin(op: AdminOp, nsid: u32, prp1: u64, cdw: [u32; 6]) -> AdminCmd {
    AdminCmd(Sqe { opcode: op as u8, nsid, prp1, prp2: 0, cdw })
}

impl AdminCmd {
    /// Identify (CNS in CDW10 7:0) into a 4 KiB page at `prp1`.
    pub fn identify(cns: u8, nsid: u32, prp1: u64) -> AdminCmd {
        admin(AdminOp::Identify, nsid, prp1, [cns as u32, 0, 0, 0, 0, 0])
    }
    /// Get Log Page of `dwords` dwords (NUMDL/NUMDU, 0-based) into `prp1` (one page: dwords <= 1024).
    pub fn get_log_page(lid: u8, nsid: u32, dwords: u32, prp1: u64) -> AdminCmd {
        let numd = dwords.max(1) - 1;
        admin(AdminOp::GetLogPage, nsid, prp1, [lid as u32 | (numd & 0xFFFF) << 16, numd >> 16, 0, 0, 0, 0])
    }
    pub fn get_features(fid: u8) -> AdminCmd {
        admin(AdminOp::GetFeatures, 0, 0, [fid as u32, 0, 0, 0, 0, 0])
    }
    /// Set Features, Number of Queues (0-based counts in CDW11; linux-6.6 core.c `nvme_set_queue_count`).
    pub fn set_num_queues(sq: u16, cq: u16) -> AdminCmd {
        let v = (sq.max(1) - 1) as u32 | ((cq.max(1) - 1) as u32) << 16;
        admin(AdminOp::SetFeatures, 0, 0, [FEAT_NUM_QUEUES as u32, v, 0, 0, 0, 0])
    }
    /// Abort the command `cid` of submission queue `sqid` (CDW10: SQID 15:0, CID 31:16; completion dword 0 bit 0
    /// set = not aborted). Linux `nvme_timeout`, linux-7/drivers/nvme/host/pci.c:1886.
    pub fn abort(sqid: u16, cid: u16) -> AdminCmd {
        admin(AdminOp::Abort, 0, 0, [sqid as u32 | (cid as u32) << 16, 0, 0, 0, 0, 0])
    }
    /// Create I/O CQ: physically contiguous (PC), interrupts enabled on `vector` when `ien`.
    pub fn create_cq(qid: u16, depth: u16, prp1: u64, vector: u16, ien: bool) -> AdminCmd {
        let cdw10 = qid as u32 | ((depth as u32 - 1) << 16);
        let cdw11 = 1 | (ien as u32) << 1 | (vector as u32) << 16;
        admin(AdminOp::CreateCq, 0, prp1, [cdw10, cdw11, 0, 0, 0, 0])
    }
    /// Create I/O SQ bound to `cqid`, physically contiguous, medium priority (ignored under round robin).
    pub fn create_sq(qid: u16, depth: u16, prp1: u64, cqid: u16) -> AdminCmd {
        let cdw10 = qid as u32 | ((depth as u32 - 1) << 16);
        let cdw11 = 1 | (2 << 1) | (cqid as u32) << 16;
        admin(AdminOp::CreateSq, 0, prp1, [cdw10, cdw11, 0, 0, 0, 0])
    }
}

/// The only blocks of namespace `nsid` a media-changing command may name (NV-2p): `gpt::window`'s checked window and,
/// while /boot lies there, the ESP `nvme_boot=` names (`with_boot`). Without one no such command can be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteWindow {
    nsid: u32,
    first: u64,
    blocks: u64,
    boot: Option<(u64, u64)>,
}

impl WriteWindow {
    pub(crate) fn new(nsid: u32, first: u64, blocks: u64) -> WriteWindow {
        WriteWindow { nsid, first, blocks, boot: None }
    }
    /// The same window with the boot ESP's beside it. None when the two overlap (a doubt refuses).
    pub(crate) fn with_boot(self, first: u64, blocks: u64) -> Option<WriteWindow> {
        let apart = first.checked_add(blocks).is_some_and(|e| e <= self.first) || self.first.saturating_add(self.blocks) <= first;
        (blocks > 0 && apart).then_some(WriteWindow { boot: Some((first, blocks)), ..self })
    }
    pub(crate) fn without_boot(self) -> WriteWindow {
        WriteWindow { boot: None, ..self }
    }
    pub fn range(&self) -> (u64, u64) {
        (self.first, self.blocks)
    }
    pub fn boot(&self) -> Option<(u64, u64)> {
        self.boot
    }
    pub fn nsid(&self) -> u32 {
        self.nsid
    }
    /// The request lies inside one of the two ranges (never across both: they are apart).
    pub fn range_holds(&self, nsid: u32, lba: u64, blocks: u64) -> bool {
        let inside = |w| block::gpt::window::inside(w, lba, blocks);
        nsid == self.nsid && (inside((self.first, self.blocks)) || self.boot.is_some_and(inside))
    }
}

/// One deallocate range's bytes (context attributes 0, NLB, SLBA; nvme.h `struct nvme_dsm_range`).
pub fn dsm_range(lba: u64, blocks: u32) -> [u8; DSM_RANGE_BYTES] {
    let mut b = [0u8; DSM_RANGE_BYTES];
    b[4..8].copy_from_slice(&blocks.to_le_bytes());
    b[8..16].copy_from_slice(&lba.to_le_bytes());
    b
}

impl IoCmd {
    /// Write `blocks` (1..=65536) from the PRPs to `lba`. None outside `w`: no such command exists.
    pub fn write(w: &WriteWindow, nsid: u32, lba: u64, blocks: u32, prp1: u64, prp2: u64, fua: bool) -> Option<IoCmd> {
        if !(1..=65536).contains(&blocks) || !w.range_holds(nsid, lba, blocks as u64) {
            return None;
        }
        let cdw12 = (blocks - 1) | if fua { CDW12_FUA } else { 0 };
        Some(IoCmd(Sqe { opcode: IoOp::Write as u8, nsid, prp1, prp2, cdw: [lba as u32, (lba >> 32) as u32, cdw12, 0, 0, 0] }))
    }
    /// Dataset Management, deallocate, one range whose bytes (`dsm_range(lba, blocks)`) lie in the page at
    /// `prp1`; the caller puts them there. None outside `w`.
    pub fn deallocate(w: &WriteWindow, nsid: u32, lba: u64, blocks: u32, prp1: u64) -> Option<IoCmd> {
        if blocks == 0 || !w.range_holds(nsid, lba, blocks as u64) {
            return None;
        }
        // CDW10: number of ranges, 0-based.
        Some(IoCmd(Sqe { opcode: IoOp::Dsm as u8, nsid, prp1, prp2: 0, cdw: [0, DSM_DEALLOCATE, 0, 0, 0, 0] }))
    }
    /// Read `blocks` (1..=65536) logical blocks from `lba` into the PRPs.
    pub fn read(nsid: u32, lba: u64, blocks: u32, prp1: u64, prp2: u64) -> IoCmd {
        let nlb = blocks.clamp(1, 65536) - 1;
        IoCmd(Sqe { opcode: IoOp::Read as u8, nsid, prp1, prp2, cdw: [lba as u32, (lba >> 32) as u32, nlb, 0, 0, 0] })
    }
    pub fn flush(nsid: u32) -> IoCmd {
        IoCmd(Sqe { opcode: IoOp::Flush as u8, nsid, ..Sqe::default() })
    }
}

/// A completion entry (NVMe 1.4 §4.6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cqe {
    pub dw0: u32,
    pub sq_head: u16,
    pub sq_id: u16,
    pub cid: u16,
    /// Status field without the phase bit: SC 7:0, SCT 10:8, CRD 12:11, M 13, DNR 14.
    pub status: u16,
}

impl Cqe {
    pub fn from_dwords(d: [u32; 4]) -> Cqe {
        Cqe { dw0: d[0], sq_head: d[2] as u16, sq_id: (d[2] >> 16) as u16, cid: d[3] as u16, status: ((d[3] >> 17) & 0x7FFF) as u16 }
    }
    pub fn ok(&self) -> bool {
        self.status & 0x7FF == 0
    }
    pub fn sc(&self) -> u8 {
        self.status as u8
    }
    pub fn sct(&self) -> u8 {
        ((self.status >> 8) & 7) as u8
    }
    pub fn dnr(&self) -> bool {
        self.status & (1 << 14) != 0
    }
}

/// Generic status codes that mean "the request was wrong", not "the medium failed" (nvme.h:1847-1890).
pub fn status_is_invalid(sct: u8, sc: u8) -> bool {
    // 02h invalid field, 0Bh invalid namespace, 80h LBA out of range, 81h capacity exceeded.
    sct == 0 && matches!(sc, 0x02 | 0x0B | 0x80 | 0x81)
}

/// Short text for a status (logs, MCP).
pub fn status_text(sct: u8, sc: u8) -> &'static str {
    match (sct, sc) {
        (0, 0x00) => "success",
        (0, 0x01) => "invalid opcode",
        (0, 0x02) => "invalid field",
        (0, 0x04) => "data transfer error",
        (0, 0x06) => "internal error",
        (0, 0x07) => "aborted by request",
        (0, 0x0B) => "invalid namespace",
        (0, 0x80) => "LBA out of range",
        (0, 0x81) => "capacity exceeded",
        (0, 0x82) => "namespace not ready",
        (1, _) => "command specific error",
        (2, 0x81) => "unrecovered read error",
        (2, _) => "media error",
        (3, _) => "path error",
        _ => "error",
    }
}
