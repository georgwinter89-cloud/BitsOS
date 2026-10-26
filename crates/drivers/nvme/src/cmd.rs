//! The command builder (R8a, NV-1): the only way to make a submission entry. Its opcode sets hold no command
//! that changes media or the controller's configuration beyond its own queues: on the I/O side only Flush and
//! Read (Write 01h, Write Uncorrectable 04h, Write Zeroes 08h, Dataset Management 09h, Copy 19h, the zone
//! sends 79h/7Dh do not exist
//! here), on the admin side only queue set-up, Identify, Get Log Page, Get Features and Set Features for the
//! queue count (Format NVM, Sanitize, namespace management, firmware download/commit are not implemented).
//! Admin and I/O commands are different types, so an admin opcode (01h = Create I/O SQ) can never be placed
//! in an I/O queue, where the same byte means Write.
//! Opcodes: linux-6.6/include/linux/nvme.h:821-840 (I/O), :1136-1165 (admin).

/// Admin opcodes this driver can build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AdminOp {
    DeleteSq = 0x00,
    CreateSq = 0x01,
    GetLogPage = 0x02,
    DeleteCq = 0x04,
    CreateCq = 0x05,
    Identify = 0x06,
    SetFeatures = 0x09,
    GetFeatures = 0x0A,
}

/// I/O opcodes this driver can build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum IoOp {
    Flush = 0x00,
    Read = 0x02,
}

pub const ADMIN_OPS: [AdminOp; 8] =
    [AdminOp::DeleteSq, AdminOp::CreateSq, AdminOp::GetLogPage, AdminOp::DeleteCq, AdminOp::CreateCq, AdminOp::Identify, AdminOp::SetFeatures, AdminOp::GetFeatures];
pub const IO_OPS: [IoOp; 2] = [IoOp::Flush, IoOp::Read];

/// I/O opcodes that change media (NVM command set): Write, Write Uncorrectable, Write Zeroes, Dataset
/// Management, Copy, Zone Management Send, Zone Append. The ktest and host tests assert none of them was
/// ever placed in a queue.
pub const MEDIA_WRITE_IO_OPS: [u8; 7] = [0x01, 0x04, 0x08, 0x09, 0x19, 0x79, 0x7D];
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
    pub fn delete_sq(qid: u16) -> AdminCmd {
        admin(AdminOp::DeleteSq, 0, 0, [qid as u32, 0, 0, 0, 0, 0])
    }
    pub fn delete_cq(qid: u16) -> AdminCmd {
        admin(AdminOp::DeleteCq, 0, 0, [qid as u32, 0, 0, 0, 0, 0])
    }
}

impl IoCmd {
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
