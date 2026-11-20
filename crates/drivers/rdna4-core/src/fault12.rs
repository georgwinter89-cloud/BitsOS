//! Piece 10, fault decode: a page fault of one of the card's two hubs as Linux takes it from the IH and words it.
//! A: `gmc_v12_0.c` (the two interrupt sources :865-893, `gmc_v12_0_process_interrupt` :91-157),
//! `gfxhub_v12_0.c` (the status' words :81-109, the clients :37-59), `mmhub_v4_1_0.c` (:88-119, the clients
//! :37-65 by client id and read/write, `amdgpu_mmhub_client_name` amdgpu_mmhub.h:67-75), `amdgpu_gmc.h:92-95`
//! (the entry's source data).
//!
//! Linux picks the hub whose registers it reads by the entry's CLIENT (VMC is MMHUB, anything else GFXHUB,
//! :107-110) and names the hub in its line by the entry's `vmid_src` bit (:139); the extra read of the status
//! before the real one is made when `vmid_src` is 0 (:126-127). On this card the two agree; an entry where they
//! do not is worded as Linux would word it, with both said.

use crate::hub41::{self, Kind};
use crate::ih7::Iv;
use crate::nbreg::Field;
use core::fmt;

/// `SOC21_IH_CLIENTID_GFX` and `SOC21_IH_CLIENTID_VMC` (include/soc15_ih_clientid.h:93, :99).
pub const CLIENT_GFX: u8 = 0x0A;
pub const CLIENT_VMC: u8 = 0x12;
/// `UTCL2_1_0__SRCID__FAULT` and `VMC_1_0__SRCID__VM_FAULT` (include/ivsrcid/vmc/irqsrcs_vmc_1_0.h:30, :35).
pub const SRC_FAULT: u8 = 0;

/// The entry's second data word (amdgpu_gmc.h:92-95); its low four bits are bits 44 to 47 of the address.
pub const DATA_RETRY: u32 = 0x80;
pub const DATA_READ: u32 = 0x40;
pub const DATA_WRITE: u32 = 0x20;
pub const DATA_EXE: u32 = 0x10;

/// `GCVM_L2_PROTECTION_FAULT_STATUS_LO32` and `MMVM_...` (gc_12_0_0_sh_mask.h:9014-9025,
/// mmhub_4_1_0_sh_mask.h:4629-4640): the same bits on both hubs.
pub mod st {
    use super::Field;
    pub const MORE_FAULTS: Field = Field::new(0, 0x0000_0001);
    pub const WALKER_ERROR: Field = Field::new(1, 0x0000_000E);
    pub const PERMISSION_FAULTS: Field = Field::new(4, 0x0000_00F0);
    pub const MAPPING_ERROR: Field = Field::new(8, 0x0000_0100);
    pub const CID: Field = Field::new(9, 0x0003_FE00);
    pub const RW: Field = Field::new(18, 0x0004_0000);
    pub const ATOMIC: Field = Field::new(19, 0x0008_0000);
    pub const VMID: Field = Field::new(20, 0x00F0_0000);
    pub const VF: Field = Field::new(24, 0x0100_0000);
    pub const VFID: Field = Field::new(25, 0x3E00_0000);
    pub const PRT: Field = Field::new(30, 0x4000_0000);
    pub const UCE: Field = Field::new(31, 0x8000_0000);
}

/// `gfxhub_client_ids` (gfxhub_v12_0.c:37-59).
pub const GFX_CLIENTS: [&str; 21] = ["CB", "DB", "GE1", "GE2", "CPF", "CPC", "CPG", "RLC", "TCP", "SQC (inst)", "SQC (data)", "SQG/PC/SC", "Reserved", "SDMA0", "SDMA1", "GCR", "Reserved", "Reserved", "WGS", "DSM", "PA"];

/// `mmhub_client_ids_v4_1_0` (mmhub_v4_1_0.c:37-65): by client id and whether the access was a write; an id past
/// the table (56 rows) or a row the table leaves empty has no name.
pub const fn mm_client(cid: u32, write: bool) -> Option<&'static str> {
    Some(match (cid, write) {
        (0, false) => "VMC",
        (4, _) => "DCEDMC",
        (6, _) => "MP0",
        (7, _) => "MP1",
        (8, _) => "MPIO",
        (16, _) => "LSDMA",
        (17, _) => "JPEG",
        (19, _) => "VCNU",
        (22, _) => "VSCH",
        (23, _) => "HDP",
        (55, false) => "VCNRD",
        (3, true) => "DCEDWB",
        (10, true) => "DBGU0",
        (11, true) => "DBGU1",
        (12, true) => "DBGUNBIO",
        (14, true) => "XDP",
        (15, true) => "OSSSYS",
        (18, true) => "VCNWR",
        _ => return None,
    })
}

/// The hub whose fault an IH entry reports, or `None` for an entry that is no page fault: the two sources
/// `gmc_v12_0_sw_init` registers for GC 12.0.x (gmc_v12_0.c:865-893).
pub const fn taker(client_id: u8, src_id: u8) -> Option<Kind> {
    match (client_id, src_id) {
        (CLIENT_VMC, SRC_FAULT) => Some(Kind::Mm),
        (CLIENT_GFX, SRC_FAULT) => Some(Kind::Gc),
        _ => None,
    }
}

/// What the handler does to the hub before it decodes (gmc_v12_0.c:120-131): the status read, once more first
/// when the entry's `vmid_src` says GFXHUB, then the clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reads {
    /// "Issue a dummy read to wait for the status register to be updated".
    pub dummy_first: bool,
}

pub const fn reads(iv: &Iv) -> Reads {
    Reads { dummy_first: !iv.vmid_src }
}

/// One page fault as Linux's handler has it after its reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    /// The hub whose status was read (by the entry's client).
    pub hub: Kind,
    /// The hub Linux's line names (by the entry's `vmid_src`).
    pub named: Kind,
    pub client_id: u8,
    pub src_id: u8,
    pub ring_id: u8,
    pub vmid: u8,
    pub pasid: u16,
    /// `src_data[0] << 12 | (src_data[1] & 0xf) << 44`: the page's address in the faulting VM.
    pub addr: u64,
    /// The entry's own flags (retry faults are not built: `noretry` is on for this card).
    pub retry: bool,
    pub write: bool,
    /// The hub's L2_PROTECTION_FAULT_STATUS_LO32; 0 when it held nothing (Linux then prints no status).
    pub status: u32,
}

impl Fault {
    pub fn new(iv: &Iv, hub: Kind, status: u32) -> Fault {
        Fault {
            hub,
            named: if iv.vmid_src { Kind::Mm } else { Kind::Gc },
            client_id: iv.client_id,
            src_id: iv.src_id,
            ring_id: iv.ring_id,
            vmid: iv.vmid,
            pasid: iv.pasid,
            addr: u64::from(iv.src_data[0]) << 12 | (u64::from(iv.src_data[1]) & 0xF) << 44,
            retry: iv.src_data[1] & DATA_RETRY != 0,
            write: iv.src_data[1] & DATA_WRITE != 0,
            status,
        }
    }

    pub const fn cid(&self) -> u32 {
        st::CID.get(self.status)
    }

    /// The status' RW bit: the faulting access was a write.
    pub const fn status_write(&self) -> bool {
        st::RW.get(self.status) != 0
    }

    /// The engine that faulted, as the print function of the hub that was read names it; "unknown" as Linux says
    /// for an id without a name.
    pub fn client(&self) -> &'static str {
        let cid = self.cid();
        match self.hub {
            Kind::Gc => GFX_CLIENTS.get(cid as usize).copied().unwrap_or("unknown"),
            Kind::Mm => mm_client(cid, self.status_write()).unwrap_or("unknown"),
        }
    }

    /// Linux's lines for this fault (gmc_v12_0.c:136-153 and the hub's print function), one per line.
    pub fn linux(&self) -> Linux<'_> {
        Linux(self)
    }
}

fn hub_word(k: Kind) -> &'static str {
    hub41::of(k).name
}

/// The fault in plain words: which hub, which engine, which address, read or write, which kind of fault.
impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} page fault at {:#018x} (vmid {}, pasid {})", hub_word(self.named), self.addr, self.vmid, self.pasid)?;
        if self.named != self.hub {
            write!(f, "; the entry's client {} is {}'s, whose status was read", self.client_id, hub_word(self.hub))?;
        }
        if self.status == 0 {
            // Linux prints no status then (gmc_v12_0.c:148-152); the entry's own flag is all there is.
            return write!(f, ": a {} (the entry's flag), the hub's status read 0: no engine and no kind known", if self.write { "write" } else { "read" });
        }
        write!(f, ": {} (client id {:#x}) tried to {}", self.client(), self.cid(), if self.status_write() { "write" } else { "read" })?;
        let (walker, perm) = (st::WALKER_ERROR.get(self.status), st::PERMISSION_FAULTS.get(self.status));
        let mut sep = "; ";
        let mut kind = |f: &mut fmt::Formatter<'_>, text: fmt::Arguments<'_>| {
            let r = write!(f, "{sep}{text}");
            sep = ", ";
            r
        };
        if st::MAPPING_ERROR.get(self.status) != 0 {
            kind(f, format_args!("no page is mapped there (mapping error)"))?;
        }
        if perm != 0 {
            kind(f, format_args!("the page does not allow it (permission faults {perm:#x})"))?;
        }
        if walker != 0 {
            kind(f, format_args!("the page table walk failed (walker error {walker:#x})"))?;
        }
        if st::MAPPING_ERROR.get(self.status) == 0 && perm == 0 && walker == 0 {
            kind(f, format_args!("the status names no kind of fault"))?;
        }
        if st::MORE_FAULTS.get(self.status) != 0 {
            kind(f, format_args!("more faults followed it"))?;
        }
        Ok(())
    }
}

/// `Fault::linux`.
pub struct Linux<'a>(&'a Fault);

impl fmt::Display for Linux<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let x = self.0;
        writeln!(f, "[{}] page fault (src_id:{} ring:{} vmid:{} pasid:{})", hub_word(x.named), x.src_id, x.ring_id, x.vmid, x.pasid)?;
        write!(f, "  in page starting at address {:#018x} from client {}", x.addr, x.client_id)?;
        if x.status == 0 {
            return Ok(());
        }
        // gfxhub_v12_0.c:87-89 prints the register's name without its _LO32, mmhub_v4_1_0.c:99-101 with it.
        let name = hub41::of(x.hub).fault_status.name;
        let name = if x.hub == Kind::Gc { name.trim_end_matches("_LO32") } else { name };
        write!(f, "\n{name}:0x{:08X}", x.status)?;
        write!(f, "\n\t Faulty UTCL2 client ID: {} ({:#x})", x.client(), x.cid())?;
        write!(f, "\n\t MORE_FAULTS: {:#x}", st::MORE_FAULTS.get(x.status))?;
        write!(f, "\n\t WALKER_ERROR: {:#x}", st::WALKER_ERROR.get(x.status))?;
        write!(f, "\n\t PERMISSION_FAULTS: {:#x}", st::PERMISSION_FAULTS.get(x.status))?;
        write!(f, "\n\t MAPPING_ERROR: {:#x}", st::MAPPING_ERROR.get(x.status))?;
        write!(f, "\n\t RW: {:#x}", st::RW.get(x.status))
    }
}

/// `printk_ratelimit()` (kernel/printk/printk.c:4739, `DEFAULT_RATELIMIT_INTERVAL` 5 s and
/// `DEFAULT_RATELIMIT_BURST` 10, include/linux/ratelimit_types.h:9-10): ten lines' worth in five seconds, then
/// silence until the five seconds are over. It gates the printing only: every fault is still read, cleared and
/// counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateLimit {
    begin_us: Option<u64>,
    printed: u32,
    /// Faults not printed in the running interval, and in all.
    pub missed: u32,
    pub missed_total: u64,
}

pub const RATE_INTERVAL_US: u64 = 5_000_000;
pub const RATE_BURST: u32 = 10;

impl RateLimit {
    pub const fn new() -> RateLimit {
        RateLimit { begin_us: None, printed: 0, missed: 0, missed_total: 0 }
    }

    /// Whether a fault at `now_us` is printed (`___ratelimit`, lib/ratelimit.c:27-103). `Some(n)` in the second
    /// place when an interval ended that suppressed `n` faults (Linux's "callbacks suppressed" line).
    pub fn allow(&mut self, now_us: u64) -> (bool, Option<u32>) {
        let mut ended = None;
        match self.begin_us {
            // A new interval begins once the old one is over (`time_is_before_jiffies(rs->begin + interval)`).
            Some(b) if now_us.saturating_sub(b) <= RATE_INTERVAL_US => {}
            _ => {
                ended = Some(self.missed).filter(|n| *n != 0);
                (self.begin_us, self.printed, self.missed) = (Some(now_us), 0, 0);
            }
        }
        if self.printed < RATE_BURST {
            self.printed += 1;
            (true, ended)
        } else {
            self.missed += 1;
            self.missed_total += 1;
            (false, ended)
        }
    }
}
