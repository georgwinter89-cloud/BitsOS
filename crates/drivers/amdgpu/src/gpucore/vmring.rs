//! GX-7m, `until=vm`: a GPU virtual memory in the rings (ladder rung 4, gx-scout-vm-rings.md sections 4 and 10).
//! It continues a passed `until=gfx_mes` (MES running, the compute ring and GFX ring 0 mapped as legacy kernel
//! queues) and the SDMA 0 queue a kept `until=sdma` left running, and runs on each queue what Linux's
//! amdgpu_vm_flush emits before a job of a non-zero VMID, then the job:
//!
//! 1. one test VM: GR-1's 4-level tables (`amdgpu_fmt::vm`) built by the CPU in carve-out pages of the MES
//!    stage's buffer (`CarveTables`), ten pages of the same buffer mapped at `VA_BASE` (uncached; the process
//!    queue's ring and queue page among them), a VMID from the kernel's range 1-7 (`VmidMgr`) and a PASID
//!    (`PasidAlloc`, 16 bits as amdgpu_mes_self_test);
//! 2. per queue the VM flush in the ring: the context's page-table base (lo, hi) written by the ring, then the
//!    invalidate request on the ring's own engine and the wait for its VMID's ack bit (gmc_v11_0_emit_flush_gpu_tlb
//!    :381-427; no semaphore on GFXHUB), PFP_SYNC_ME on the GFX ring (gfx_v11_0_ring_emit_vm_flush :5426-5436),
//!    then the IH_VMID_n_LUT entry = PASID (gmc_v11_0_emit_pasid_mapping :429-445); a fence after it. The CPU then
//!    reads the context's base and the LUT entry back (the watcher never sees a ring's register write);
//! 3. an IB in the VM (its own address a VM address: the CP and SDMA fetch it through VMID n) writing a VM
//!    address, checked by the CPU through the carve-out;
//! 4. an intended fault: an IB writing an unmapped VM address of the same page table. GFX ring 0's fence after it
//!    must still land (CPG drops the write: CP_DEBUG.CPG_UTCL1_ERROR_HALT_DISABLE, as Linux sets it up). GFXHUB's L2
//!    protection fault status is polled (there is no interrupt path yet), decoded and told apart from a stray fault
//!    (VMID, page and direction must be the intended ones), cleared as gmc_v11_0_process_interrupt does
//!    (L2_PROTECTION_FAULT_CNTL bit 0, :114-119) and read back 0. GX-7m3: a compute queue halts at the fault by
//!    hardware design (CPC has no such bit; `vmrecover` has the evidence), SDMA may: a queue that stalled with the
//!    intended fault recorded passes this rung, and the next rung brings it back as Linux 7.1 does;
//! 5. one more IB in the VM: the ring survived the fault, or came back from it.
//! 6. the same VM through a MES process queue (`vmproc`): ADD_QUEUE with the VM's page table, PASID, process and
//!    gang context (mes_v11_0_add_hw_queue, as amdgpu_mes_self_test adds its queues), MES picks a VMID of its own
//!    (8-15) and flushes it itself; then a ring test, rungs 3-5 on that queue (the fault must name a MES VMID) and
//!    REMOVE_QUEUE at the teardown.
//!
//! The ack wait. Linux's single WAIT_REG_MEM write-wait packet (`AckWait::Linux`, the default since INT-W28, when
//! GR's interpreter learned to mask the reference too) compares the ack register with the masked request.
//! `AckWait::Helper` is Linux's own two-packet form for rings without the combined packet,
//! amdgpu_ring_emit_reg_write_reg_wait_helper (amdgpu_ring.c:405: the request written, then the ack bit polled),
//! the same register effects. SDMA uses sdma_v6_0's
//! reg_write_reg_wait (sdma_v6_0.c:1231-1238: SRBM_WRITE, a poll of the request register, the ack poll): Linux's
//! VM flush on SDMA uses the VM_INVALIDATION form of POLL_REGMEM, which GR-1 has no encoder and no interpreter for
//! (a GR change request).
//!
//! After a pass the VM stays (its tables in the MES buffer, kept for the boot; the base in context n, the LUT
//! entry set) until `until=stop` or the next `until=vm` tears it down (`teardown`: base and LUT entry 0 by the
//! CPU, then a CPU flush of VMID n on engine 17, bounded). A failed run tears it down at once, after every queue has
//! had its turn: a queue that fails is reported and the next one still runs. GX-D9: GFXHUB, OSSSYS and GC's CP and
//! SDMA registers only; no MMHUB register, no DCN.

use super::bringup::{Bringup, Outcome as WriteOutcome};
use super::carvetable::CarveTables;
use super::disc::Discovery;
use super::gfxring;
use super::gmc::{self, FaultRead, HubAddrs};
use super::hwid;
use super::mes::{self, MesAddrs, MesBo, PmQueue, Rung, Sparse, hx, hx32};
use super::psp::Hw;
use super::sdma::{self, SdmaAddrs};
use super::vmproc;
use super::vmrecover;
use super::watch::Watch;
use crate::ipmod::gfx11;
use crate::ipmod::gmc11::{self, CPU_ENGINE, Fault, Hub, R};
use crate::ipmod::ih6;
use crate::ipmod::mes11::{self, M};
use crate::ipmod::sdma6;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::addr::Carveout;
use amdgpu_fmt::fence::Timeline;
use amdgpu_fmt::ids::{Owner, PasidAlloc, VmidMgr};
use amdgpu_fmt::pm4::{self, WaitRegMem};
use amdgpu_fmt::pte::{MapFlags, Mtype};
use amdgpu_fmt::ring::{Kind, Ring, RingError};
use amdgpu_fmt::sdma as spkt;
use amdgpu_fmt::vm::{Backing, Vm};

/// The kernel's VMIDs on GFXHUB: 1-7. Linux gives amdgpu graphics 1-7 and amdkfd 8-15 (gmc_v11_0.c:850-854,
/// first_kfd_vmid 8), and MES is told 0xff00 for both hubs (the 780M's dmesg, `HwMasks::LINUX_780M`): VMIDs 8-15
/// are MES's to program for its own process queues, so a CPU- or ring-programmed context there would race MES.
pub const FIRST_VMID: u32 = 1;
pub const VMIDS: u32 = 7;
/// amdgpu_pasid_alloc(16) (amdgpu_mes_self_test).
pub const PASID_BITS: u32 = 16;

/// The stage's part of the MES stage's buffer (free there: after the compute ring, before the KIQ ring).
pub mod lay {
    /// Page tables (one per 4 KiB page; the test VM needs four).
    pub const TABLES: u64 = 0x4_0000;
    pub const TABLES_END: u64 = 0x6_0000;
    /// The mapped pages: per queue an IB page and a target page (four queues), then the MES process queue's ring
    /// page and its queue page (rptr, wptr, fence, EOP): the GPU reaches them by VA, the CPU by MC.
    pub const DATA: u64 = 0x6_0000;
    pub const PROC_RING: u64 = 0x6_8000;
    pub const PROC_PAGE: u64 = 0x6_9000;
    pub const DATA_END: u64 = 0x6_A000;
    /// The kernel rings' fences (VMID 0, MC addresses), 64 bytes per queue.
    pub const FENCES: u64 = 0x6_A000;
    /// The process queue's MQD and MES's process and gang context pages (VMID 0, MC addresses; Linux: GTT BOs).
    pub const PROC_MQD: u64 = 0x6_B000;
    pub const PROC_CTX: u64 = 0x6_C000;
    pub const GANG_CTX: u64 = 0x6_D000;
    pub const END: u64 = 0x6_E000;
}
const _: () = assert!(lay::END <= mes::lay::KIQ_RING && lay::TABLES >= mes::lay::COMPUTE_RING + mes::COMPUTE_RING_BYTES);

/// The test VM's addresses: the mapping, and the unmapped page of each queue's intended fault (same page table:
/// an invalid PTE, not a missing directory).
pub const VA_BASE: u64 = 0x1_0000_0000;
pub const VA_HOLE: u64 = VA_BASE + 0x10_0000;
/// IBs inside a queue's IB page (32-byte aligned for SDMA's INDIRECT).
const IB_WRITE: u64 = 0;
const IB_FAULT: u64 = 0x100;
const IB_AFTER: u64 = 0x200;
/// What the IBs write ("VMOK", the fault's value, "VM2!").
pub const VM_VALUE: u32 = 0x564D_4F4B;
pub const FAULT_VALUE: u32 = 0xFA17_0BAD;
pub const AFTER_VALUE: u32 = 0x564D_3221;
pub const POISON: u32 = 0xCAFE_DEAD;
/// WAIT_REG_MEM's poll interval (gfx_v11_0 reg_wait / reg_write_reg_wait: 0x20).
const POLL_INTERVAL: u32 = 0x20;

/// GX-7m2: the test VM's pages as Linux maps MES's self-test meta data, which holds the rings, IBs and fences
/// alike: READABLE | WRITEABLE | EXECUTABLE (amdgpu_mes_ctx_map_meta_data, amdgpu_mes.c:1129-1132; the CSA the
/// same, amdgpu_csa.c:91-93), and gmc_v11_0_get_vm_pte keeps EXECUTABLE from the mapping (gmc_v11_0.c:527-528).
/// Without it the 780M refused the CP's fetch of the first IB (29.10.: GFXHUB 0x00100880, CPF, read, VMID 1,
/// PERMISSION_FAULTS 0x8 = execute). Uncached as before.
pub const MAP_FLAGS: MapFlags = MapFlags { read: true, write: true, exec: true, mtype: Mtype::Uc, tmz: false, noalloc: false };

/// GX-7m2: why an IB rung's fault says the fetch was refused: a read at the IB's own page with the EXECUTABLE
/// permission missing (PERMISSION_FAULTS bit 3), what the 780M showed for an IB page mapped without it.
pub fn fetch_refused(f: &FaultRead, ib_va: u64) -> Option<String> {
    let s = f.status.filter(|&s| s != 0 && s != u32::MAX)?;
    let d = Fault::decode(s);
    (!d.write && d.permission & 0x8 != 0 && f.addr == Some(ib_va & 0x0000_ffff_ffff_f000)).then(|| {
        format!(" (the IB's own page was refused for execute: PERMISSION_FAULTS {:#x}, the PTE lacks EXECUTABLE, bit 4)", d.permission)
    })
}

/// gfx_v11_0_ring_emit_ib_compute (:5325-5327): a MES queue's IB "inherit[s] vmid from mqd": the VMID field stays 0
/// and this control bit says so. Without it the CP would fetch the IB's VA in VMID 0.
pub const IB_INHERIT_VMID: u32 = 0x4000_0000;

/// The process queue's IB: VMID field 0, the inherit bit (see `IB_INHERIT_VMID`).
pub fn proc_ib(out: &mut Vec<u32>, va: u64, dwords: u32) -> Result<(), amdgpu_fmt::encode::EncodeError> {
    pm4::indirect_buffer(out, va, dwords, 0, false, pm4::IB_VALID | IB_INHERIT_VMID)
}

/// How the in-ring ack wait is encoded (see the module text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckWait {
    /// gfx_v11_0_ring_emit_reg_write_reg_wait: one WAIT_REG_MEM, operation 1 (write the request, wait for the ack).
    Linux,
    /// amdgpu_ring_emit_reg_write_reg_wait_helper: WRITE_DATA of the request, then WAIT_REG_MEM on the ack bit.
    Helper,
}

/// The queues of the stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Q {
    Gfx,
    Compute,
    Sdma,
    /// GX-7m: a compute queue MES maps as a process queue of the test VM (`vmproc`).
    Proc,
}

impl Q {
    /// The kernel queues (an in-ring flush in a kernel VMID).
    pub const ALL: [Q; 3] = [Q::Gfx, Q::Compute, Q::Sdma];

    pub fn name(self) -> &'static str {
        match self {
            Q::Gfx => "GFX ring 0 (MES)",
            Q::Compute => "compute ring",
            Q::Sdma => "SDMA 0",
            Q::Proc => "MES process queue (compute)",
        }
    }

    /// The ring's invalidation engine: the 780M's dmesg ("ring gfx_0.0.0 uses VM inv eng 0 on hub 0", comp_1.0.0
    /// 1, sdma0 12; amdgpu_gmc_allocate_vm_inv_eng hands them out in ring order, never 17, the CPU's). The process
    /// queue has none: MES flushes its VMIDs itself (`u8::MAX`, never used).
    pub fn engine(self) -> u8 {
        match self {
            Q::Gfx => 0,
            Q::Compute => 1,
            Q::Sdma => 12,
            Q::Proc => u8::MAX,
        }
    }

    fn index(self) -> u64 {
        match self {
            Q::Gfx => 0,
            Q::Compute => 1,
            Q::Sdma => 2,
            Q::Proc => 3,
        }
    }

    pub fn ib_va(self) -> u64 {
        VA_BASE + self.index() * 0x2000
    }

    pub fn target_va(self) -> u64 {
        self.ib_va() + 0x1000
    }

    pub fn hole_va(self) -> u64 {
        VA_HOLE + self.index() * 0x1000
    }

    /// GX-7m3: the queue may halt at its intended fault (`vmrecover`). GFX ring 0 may not: CPG drops the write, and
    /// the chip showed it (30.10.).
    pub fn may_stall(self) -> bool {
        self != Q::Gfx
    }

    /// GX-7m3: the rung of a queue that stalled at its intended fault.
    pub fn recover_rung(self) -> &'static str {
        match self {
            Q::Gfx => "GFX (MES) queue recovered after the fault",
            Q::Compute => "compute queue recovered through MES after the fault",
            Q::Sdma => "SDMA queue reset and restarted after the fault",
            Q::Proc => "MES process queue removed and added again after the fault",
        }
    }

    /// The rungs (also the checks), in order.
    pub fn rungs(self) -> [&'static str; 4] {
        match self {
            Q::Gfx => ["GFX (MES) VM flush in the ring", "GFX (MES) IB in the VM writes a VM address", "GFX (MES) intended VM fault decoded and cleared", "GFX (MES) IB in the VM after the fault"],
            Q::Compute => ["compute VM flush in the ring", "compute IB in the VM writes a VM address", "compute intended VM fault decoded and cleared", "compute IB in the VM after the fault"],
            Q::Sdma => ["SDMA VM flush in the ring", "SDMA IB in the VM writes a VM address", "SDMA intended VM fault decoded and cleared", "SDMA IB in the VM after the fault"],
            Q::Proc => ["MES process queue ADD_QUEUE with the VM and ring test", "MES process queue IB in the VM writes a VM address", "MES process queue intended VM fault decoded and cleared", "MES process queue IB in the VM after the fault"],
        }
    }
}

/// GFXHUB and the IH's VMID-to-PASID table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmAddrs {
    pub hub: HubAddrs,
    /// IH_VMID_0_LUT's byte.
    pub lut0: Option<u32>,
    /// GX-7m3: GC's CP registers, for a stalled compute queue (`vmrecover`).
    pub hqd: Option<MesAddrs>,
}

impl VmAddrs {
    pub fn from_discovery(d: &Discovery) -> Option<VmAddrs> {
        let hub = HubAddrs::from_discovery(d, Hub::Gfx)?;
        let lut0 = d.reg(hwid::OSSSYS, 0, ih6::IH_VMID_0_LUT.0, ih6::IH_VMID_0_LUT.1).and_then(|x| x.checked_mul(4));
        Some(VmAddrs { hub, lut0, hqd: MesAddrs::from_discovery(d) })
    }

    pub fn lut(&self, vmid: u8) -> Option<u32> {
        self.lut0?.checked_add(u32::from(vmid) * 4).filter(|_| vmid < gmc11::CONTEXTS)
    }

    /// Every register the stage reads: the kernel contexts' bases, the fault registers, engine 17 and the rings'
    /// engines' acks, the LUT; for the process queue VM_CONTEXT1_CNTL (ADD_QUEUE's vm_context_cntl) and MES's
    /// contexts' bases and LUT entries (which VMID MES gave the process: reads only).
    pub fn readable(&self) -> Vec<u32> {
        let mut v = self.writable();
        v.extend([R::FaultStatus, R::FaultAddrLo, R::FaultAddrHi, R::Ack(CPU_ENGINE), R::Ctx(1)].into_iter().filter_map(|r| self.hub.byte(r)));
        v.extend(Q::ALL.iter().filter_map(|q| self.hub.byte(R::Ack(q.engine()))));
        for n in vmproc::MES_VMIDS {
            v.extend([R::PtBaseLo(n), R::PtBaseHi(n)].into_iter().filter_map(|r| self.hub.byte(r)));
            v.extend(self.lut(n));
        }
        if let Some(m) = self.hqd {
            v.extend([M::CpHqdActive, M::CpHqdError, M::CpHqdPqRptr, M::CpcUtcl1Error, M::CpcUtcl1Status, M::CpCpcStatus, M::CpCpcStalledStat1, M::CpGfxHqdActive].into_iter().filter_map(|r| m.byte(r)));
        }
        v
    }

    /// Every register the CPU writes: the kernel contexts' bases and LUT entries (teardown), the fault clear,
    /// engine 17's request (the teardown flush). GFXHUB and OSSSYS only (GX-D9).
    pub fn writable(&self) -> Vec<u32> {
        let mut v = Vec::new();
        for n in FIRST_VMID as u8..(FIRST_VMID + VMIDS) as u8 {
            v.extend([R::PtBaseLo(n), R::PtBaseHi(n)].into_iter().filter_map(|r| self.hub.byte(r)));
            v.extend(self.lut(n));
        }
        v.extend([R::FaultCntl, R::Req(CPU_ENGINE)].into_iter().filter_map(|r| self.hub.byte(r)));
        if let Some(m) = self.hqd {
            v.extend([M::CpHqdDequeueRequest, M::SpiComputeQueueReset, M::CpcUtcl1Error, M::SqCmd, M::CpGfxHqdDequeueRequest].into_iter().filter_map(|r| m.byte(r)));
            v.extend(m.gfx(gfx11::R::GrbmGfxCntl));
        }
        v
    }

    pub(super) fn dw(&self, r: R) -> Result<u32, VmError> {
        self.hub.byte(r).map(|b| b / 4).ok_or(VmError::NoAddress)
    }
}

/// SDMA 0 as a kept `until=sdma` left it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SdmaSide {
    pub addrs: SdmaAddrs,
    pub layout: sdma::Layout,
    pub burst_nop: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmInputs {
    pub addrs: VmAddrs,
    pub carve: Carveout,
    pub timeout_us: u64,
    pub flush_us: u64,
    /// The MES stage's buffer and layout (its compute ring, the stage's tables and pages) and the GFX stage's
    /// layout (GFX ring 0's memory).
    pub mes_bo: MesBo,
    pub mes: mes::Layout,
    pub gfx: gfxring::Layout,
    /// SDMA 0's kept queue; `None`: the SDMA rungs are skipped (no kept `until=sdma` in this boot).
    pub sdma: Option<SdmaSide>,
    pub ack: AckWait,
    /// The VM's owner token for the VMID manager (unique per run).
    pub token: u64,
    /// The kernel VMIDs the manager hands out: (first, count); `(FIRST_VMID, VMIDS)`.
    pub vmids: (u32, u32),
    /// The MES process queue rungs run (`vmproc`).
    pub proc: bool,
    /// How long MES may take to answer an API frame (Linux: `mes11::API_TIMEOUT_US`).
    pub api_us: u64,
}

impl VmInputs {
    pub fn new(addrs: VmAddrs, carve: Carveout, mes_bo: MesBo, mes: mes::Layout, gfx: gfxring::Layout, sdma: Option<SdmaSide>, token: u64) -> VmInputs {
        VmInputs {
            addrs,
            carve,
            timeout_us: gfx11::LINUX_TIMEOUT_US,
            flush_us: gmc::LINUX_FLUSH_US,
            mes_bo,
            mes,
            gfx,
            sdma,
            ack: AckWait::Linux,
            token,
            vmids: (FIRST_VMID, VMIDS),
            proc: true,
            api_us: mes11::API_TIMEOUT_US,
        }
    }
}

/// A VM the stage left in place (for the teardown).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmUp {
    pub vmid: u8,
    pub pasid: u32,
    pub pd: u64,
    /// The MES process queue mapped in it (removed first at the teardown).
    pub proc: Option<vmproc::ProcUp>,
}

/// What a teardown did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Teardown {
    pub ok: bool,
    pub lines: Vec<String>,
    /// The VM again when the CPU's part failed (a base, the LUT entry or the flush not done): the caller keeps it
    /// for the next stop or until=vm to retry. The process queue is not in it (one REMOVE_QUEUE try; MES's halt at
    /// the stop ends it either way).
    pub retry: Option<VmUp>,
    /// REMOVE_QUEUE of the process queue was not answered: its HQD may stay active (as a stranded legacy queue).
    pub stranded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VmError {
    NoAddress,
    Read { byte: u32 },
    Write { byte: u32 },
    Mem { mc: u64 },
    Ring(RingError),
    /// The MES buffer does not hold the stage's part.
    BoTooSmall { have: u64, need: u64 },
    /// GR-1's builder refused the VM (tables, mapping).
    Tables(amdgpu_fmt::vm::VmError),
    NoVmid,
    NoPasid,
    /// The earlier until=vm's VM could not be torn down (kept for a retry): nothing new built.
    PrevTeardown,
    /// MES did not answer the process queue's ADD_QUEUE (`vmproc`).
    ProcRefused,
    /// GX-7m3: an earlier run's recovery left the compute ring unmapped (`mes::COMPUTE_UNMAPPED`, `COMPUTE_STUCK`).
    ComputeUnmapped,
    /// A kept queue is not idle (its rptr writeback is not at its wptr) or not running: nothing submitted.
    NotIdle { queue: &'static str, rptr: Option<u64>, wptr: Option<u64> },
    /// The fault status was not clean before the intended fault (after the found one was cleared).
    Unclean { status: u32 },
    Timeout { rung: &'static str, rptr: Option<u64>, want: u64 },
    Result { rung: &'static str },
    NoDoorbell,
}

impl VmError {
    pub fn errno(&self) -> i32 {
        match self {
            VmError::NoAddress | VmError::NotIdle { .. } | VmError::PrevTeardown | VmError::ComputeUnmapped => 19,
            VmError::ProcRefused => 110,
            VmError::BoTooSmall { .. } | VmError::Tables(_) | VmError::NoVmid | VmError::NoPasid => 12,
            VmError::Timeout { .. } => 110,
            _ => 5,
        }
    }

    pub fn advice(&self) -> &'static str {
        match self {
            VmError::ComputeUnmapped => ": an earlier run's recovery left the compute ring unmapped: nothing written; until=stop, then until=gfx_mes with again=true (reboot instead if it reports STRANDED)",
            VmError::NotIdle { .. } => ": a kept queue is busy or stopped: until=vm continues a passed until=gfx_mes (and a kept until=sdma before it) of this boot",
            VmError::Timeout { .. } => ": the ring stopped with no intended fault to explain it (an ack that never came stalls the flush; GFX ring 0 must go on at its fault): the other queues still ran, the VM is torn down; report the rung lines and `vm after`",
            VmError::Result { .. } => ": a rung finished but its memory or registers did not read as expected (a recovery rung: a step of it failed): the other queues still ran, the VM is torn down",
            VmError::Unclean { .. } => ": GFXHUB holds a fault the clear did not remove: no fault could be told apart; the VM is torn down",
            VmError::PrevTeardown => ": the earlier VM's teardown failed (see the teardown lines): it is kept and retried at the next until=stop or until=vm",
            VmError::ProcRefused => ": MES did not answer ADD_QUEUE of the process queue: REMOVE_QUEUE is not tried; until=stop halts MES (reboot before the CP runs again if the stop reports it stranded)",
            _ => "",
        }
    }
}

/// One intended fault as read and cleared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultSeen {
    pub queue: Q,
    pub read: FaultRead,
    pub want_page: u64,
    pub intended: bool,
    /// The status after the clear.
    pub cleared: Option<u32>,
    /// GX-7m3: the queue stopped at this fault (its fence did not land) and took the recovery rung.
    pub stalled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VmReport {
    pub vmid: Option<u8>,
    pub pasid: Option<u32>,
    pub pd: Option<u64>,
    pub tables: usize,
    /// (VA, pages, host-physical) of the mapping.
    pub mapping: Option<(u64, u64, u64)>,
    pub sdma_skipped: Option<String>,
    /// Each queue's position when the stage found it: (queue, rptr writeback, wptr).
    pub found: Vec<(&'static str, Option<u64>, Option<u64>)>,
    /// A fault GFXHUB held before the stage (cleared first).
    pub found_fault: Option<FaultRead>,
    pub rungs: Vec<Rung>,
    pub faults: Vec<FaultSeen>,
    /// CONTEXTn_PAGE_TABLE_BASE and IH_VMID_n_LUT as the CPU read them after the flushes.
    pub ctx_read: Option<u64>,
    pub lut_read: Option<u32>,
    pub doorbells: Vec<(u32, u64, bool)>,
    pub fault_after: Option<FaultRead>,
    /// The VM stays (a pass); otherwise torn down (lines).
    pub kept: Option<VmUp>,
    pub teardown: Vec<String>,
    pub error: Option<VmError>,
    /// GX-7m process queue: why its rungs did not run (`VmInputs::proc` false); what was mapped; the VMID MES gave it
    /// (the context whose base reads the VM's root); the LUT entry MES set there.
    pub proc_skipped: Option<String>,
    pub proc: Option<vmproc::ProcUp>,
    pub proc_vmid: Option<u8>,
    pub proc_lut: Option<u32>,
    /// A failed run's fault that came after the teardown (a stalled ring's late access): cleared, reported.
    pub late_fault: Option<FaultRead>,
    /// A teardown the caller must keep for a retry (the earlier VM's or this run's); a process queue MES did not
    /// remove.
    pub retry: Option<VmUp>,
    pub stranded: bool,
    /// GX-7m3: a fault a failed queue left, cleared before the next queue's turn.
    pub left_faults: Vec<(&'static str, FaultRead, Option<u32>)>,
}

impl VmReport {
    pub fn passed(&self) -> bool {
        self.error.is_none() && checks(self).iter().all(|c| c.pass)
    }
}

fn ring_err(_: amdgpu_fmt::encode::EncodeError) -> RingError {
    RingError::State
}

pub(super) fn mem_err(e: mes::MesError) -> VmError {
    match e {
        mes::MesError::Mem { mc } => VmError::Mem { mc },
        mes::MesError::Ring(r) => VmError::Ring(r),
        mes::MesError::NoDoorbell => VmError::NoDoorbell,
        _ => VmError::NoAddress,
    }
}

/// A register write (WRITE_DATA, dst_sel register) as gfx_v11_0_ring_emit_wreg: GFX rings from the PFP
/// (ENGINE_SEL 1) with WR_CONFIRM, compute rings with WR_CONFIRM.
pub fn pm4_wreg(out: &mut Vec<u32>, reg_dw: u32, value: u32, gfx: bool) -> Result<(), RingError> {
    let at = out.len();
    pm4::write_data(out, pm4::dst_sel::REG, u64::from(reg_dw), pm4::WR_CONFIRM, &[value]).map_err(ring_err)?;
    if gfx {
        out[at + 1] |= 1 << 30;
    }
    Ok(())
}

/// The in-ring VM flush of a PM4 ring (gmc_v11_0_emit_flush_gpu_tlb, gfx_v11_0_ring_emit_vm_flush), then the
/// PASID entry (gmc_v11_0_emit_pasid_mapping) when `lut_dw` is known.
#[allow(clippy::too_many_arguments)]
pub fn pm4_flush(out: &mut Vec<u32>, a: &VmAddrs, vmid: u8, pd: u64, eng: u8, gfx: bool, pasid: u32, ack: AckWait) -> Result<(), VmError> {
    let req = gmc11::invalidate_req(vmid, 0);
    let mask = 1u32 << (vmid & 0xf);
    let (lo, hi) = (a.dw(R::PtBaseLo(vmid))?, a.dw(R::PtBaseHi(vmid))?);
    let (rq, ak) = (a.dw(R::Req(eng))?, a.dw(R::Ack(eng))?);
    pm4_wreg(out, lo, pd as u32, gfx).map_err(VmError::Ring)?;
    pm4_wreg(out, hi, (pd >> 32) as u32, gfx).map_err(VmError::Ring)?;
    let w = match ack {
        AckWait::Linux => WaitRegMem { function: 3, mem: false, operation: 1, engine: u8::from(gfx), addr0: rq, addr1: ak, reference: req, mask, interval: POLL_INTERVAL },
        AckWait::Helper => {
            pm4_wreg(out, rq, req, gfx).map_err(VmError::Ring)?;
            WaitRegMem { function: 3, mem: false, operation: 0, engine: 0, addr0: ak, addr1: 0, reference: mask, mask, interval: POLL_INTERVAL }
        }
    };
    pm4::wait_reg_mem(out, &w).map_err(|_| VmError::Ring(RingError::State))?;
    if gfx {
        // Sync PFP to ME, otherwise the PFP may read through the old translation.
        out.extend([pm4::packet3(pm4::op::PFP_SYNC_ME, 0), 0]);
    }
    if let Some(l) = a.lut(vmid) {
        pm4_wreg(out, l / 4, pasid, gfx).map_err(VmError::Ring)?;
    }
    Ok(())
}

/// The same on SDMA: SRBM_WRITE of the base, sdma_v6_0_ring_emit_reg_write_reg_wait (the request, a poll of the
/// request register "to reset the ack", the ack poll), the PASID entry.
pub fn sdma_flush(out: &mut Vec<u32>, a: &VmAddrs, vmid: u8, pd: u64, eng: u8, pasid: u32) -> Result<(), VmError> {
    let req = gmc11::invalidate_req(vmid, 0);
    let mask = 1u32 << (vmid & 0xf);
    let enc = |_| VmError::Ring(RingError::State);
    let (lo, hi) = (a.dw(R::PtBaseLo(vmid))?, a.dw(R::PtBaseHi(vmid))?);
    let (rq, ak) = (a.dw(R::Req(eng))?, a.dw(R::Ack(eng))?);
    spkt::srbm_write(out, lo, pd as u32).map_err(enc)?;
    spkt::srbm_write(out, hi, (pd >> 32) as u32).map_err(enc)?;
    spkt::srbm_write(out, rq, req).map_err(enc)?;
    let poll = |reg: u32, v: u32, m: u32| spkt::PollRegMem { hdp_flush: false, mem: false, function: 3, addr_lo: reg << 2, addr_hi: 0, reference: v, mask: m, interval: 10, retry: 0xfff };
    spkt::poll_regmem(out, &poll(rq, 0, 0)).map_err(enc)?;
    spkt::poll_regmem(out, &poll(ak, mask, mask)).map_err(enc)?;
    if let Some(l) = a.lut(vmid) {
        spkt::srbm_write(out, l / 4, pasid).map_err(enc)?;
    }
    Ok(())
}

/// SDMA 0's kept queue as the CPU drives it after `until=sdma` (doorbell mode, the stage's last rung).
pub(crate) struct SdmaQ {
    ring: Ring,
    base: u64,
    rptr_mc: u64,
    wptr_mc: u64,
    kind: Kind,
    rptr_dw: u64,
}

impl SdmaQ {
    fn submit(&mut self, hw: &mut dyn Hw, ndw: u64, build: impl FnOnce(&mut Ring) -> Result<(), RingError>, doorbells: &mut Vec<(u32, u64, bool)>) -> Result<u64, VmError> {
        self.ring.begin(ndw, self.rptr_dw).map_err(VmError::Ring)?;
        if let Err(e) = build(&mut self.ring) {
            self.ring.undo();
            return Err(VmError::Ring(e));
        }
        let mut sink = Sparse { size: self.ring.size_dw(), out: Vec::new() };
        let new = self.ring.commit(&mut sink).map_err(VmError::Ring)?;
        for (i, dw) in sink.out {
            mes::mem(hw, self.base + i * 4, &dw.to_le_bytes()).map_err(mem_err)?;
        }
        hw.store_barrier();
        let p = self.kind.hw_ptr(new);
        if !hw.mem_write64(self.wptr_mc, p) {
            return Err(VmError::Mem { mc: self.wptr_mc });
        }
        hw.store_barrier();
        let ok = hw.doorbell_write64(sdma6::DOORBELL_DW, p);
        doorbells.push((sdma6::DOORBELL_DW, p, ok));
        klog::event!(Debug, "gx", "vm.doorbell", index = sdma6::DOORBELL_DW, value = klog::Hex(p), accepted = ok);
        if !ok {
            return Err(VmError::NoDoorbell);
        }
        Ok(new)
    }

    fn wait(&mut self, hw: &mut dyn Hw, us: u64, name: &'static str, wptr: u64, done: &mut dyn FnMut(&mut dyn Hw) -> bool) -> Rung {
        let want = self.kind.hw_ptr(wptr);
        let rp = self.rptr_mc;
        let t0 = hw.now_us();
        let mut polls = 0;
        let mut last = None;
        let ok = gmc::poll(hw, us, &mut polls, &mut |h| {
            last = mes::rd64(h, rp);
            last == Some(want) && done(h)
        });
        if ok {
            self.rptr_dw = wptr;
        }
        let el = hw.now_us().saturating_sub(t0);
        rung_event(name, ok, wptr, last, polls, el);
        Rung { name, pass: ok, done: ok, wptr, rptr: last, fence: None, polls, us: el, detail: String::new() }
    }
}

fn rung_event(name: &'static str, ok: bool, wptr: u64, rptr: Option<u64>, polls: u32, us: u64) {
    if ok {
        klog::event!(Info, "gx", "vm.rung", rung = name, wptr = wptr, polls = polls, us = us);
    } else {
        klog::event!(Error, "gx", "vm.rung_timeout", rung = name, wptr = wptr, rptr = klog::Hex(rptr.unwrap_or(u64::MAX)), polls = polls, us = us);
    }
}

/// One queue of the stage, and of the backend's jobs (`hw::rings`).
pub(crate) enum Queue {
    Pm4 { q: PmQueue, gfx: bool },
    Sdma(SdmaQ),
}

impl Queue {
    pub(crate) fn submit(&mut self, hw: &mut dyn Hw, ndw: u64, build: impl FnOnce(&mut Ring) -> Result<(), RingError>, doorbells: &mut Vec<(u32, u64, bool)>) -> Result<u64, VmError> {
        match self {
            Queue::Pm4 { q, .. } => q.submit(hw, ndw, build, doorbells).map_err(mem_err),
            Queue::Sdma(s) => s.submit(hw, ndw, build, doorbells),
        }
    }

    pub(crate) fn wait(&mut self, hw: &mut dyn Hw, us: u64, name: &'static str, wptr: u64, done: &mut dyn FnMut(&mut dyn Hw) -> bool) -> Rung {
        match self {
            Queue::Pm4 { q, .. } => q.wait(hw, us, name, wptr, done),
            Queue::Sdma(s) => s.wait(hw, us, name, wptr, done),
        }
    }

    fn ring(&self) -> &Ring {
        match self {
            Queue::Pm4 { q, .. } => &q.ring,
            Queue::Sdma(s) => &s.ring,
        }
    }

    /// The committed write pointer (dword counter).
    pub(crate) fn wptr(&self) -> u64 {
        self.ring().wptr()
    }

    pub(crate) fn size_dw(&self) -> u64 {
        self.ring().size_dw()
    }

    pub(crate) fn kind(&self) -> Kind {
        self.ring().kind()
    }

    /// What a frame of `ndw` dwords takes of the ring, its padding included.
    pub(crate) fn fits(&self, ndw: u64, rptr: u64) -> bool {
        let a = self.kind().ring_align_dw();
        let n = ndw.saturating_add(a - 1) & !(a - 1);
        self.ring().free_dw(rptr).is_ok_and(|f| n <= f)
    }

    /// The read pointer a submission checks its space against (the job path's watch knows it, not `wait`).
    pub(crate) fn set_rptr(&mut self, dw: u64) {
        match self {
            Queue::Pm4 { q, .. } => q.rptr_dw = dw,
            Queue::Sdma(s) => s.rptr_dw = dw,
        }
    }

    /// The read pointer as the engine last wrote it back, in the hardware's form.
    pub(crate) fn rptr_hw(&self, hw: &mut dyn Hw) -> Option<u64> {
        match self {
            Queue::Pm4 { q, .. } => q.rptr_wb.and_then(|r| mes::rd32(hw, r)).map(u64::from),
            Queue::Sdma(s) => mes::rd64(hw, s.rptr_mc),
        }
    }

    /// The whole ring, for a hang's dump.
    pub(crate) fn words(&self, hw: &mut dyn Hw) -> Option<Vec<u32>> {
        let base = match self {
            Queue::Pm4 { q, .. } => q.base,
            Queue::Sdma(s) => s.base,
        };
        let mut b = alloc::vec![0u8; self.size_dw() as usize * 4];
        hw.mem_read(base, &mut b).then(|| b.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    }
}

/// What every rung of one queue shares.
pub(super) struct Ctx<'a> {
    pub a: &'a VmAddrs,
    /// The VMID the queue runs in (the process queue: the one MES gave it as the CPU found it, 0 when not found).
    pub vmid: u8,
    pub pd: u64,
    pub pasid: u32,
    /// MC of the queue's IB page, target page and fence slot.
    pub ib_mc: u64,
    pub target_mc: u64,
    pub fence: u64,
    /// The fence slot as the queue's engine addresses it (the kernel rings: the MC address; the process queue: its VA).
    pub fence_gpu: u64,
    pub us: u64,
    pub ack: AckWait,
    /// The queue is MES's process queue: its intended fault must name one of MES's VMIDs (8-15).
    pub mes: bool,
    /// GX-7m3: what a recovery needs: the stage's inputs and, for the process queue, its ADD_QUEUE.
    pub inp: &'a VmInputs,
    pub proc: Option<vmproc::ProcSpec>,
}

/// The IB of one write of `value` to the VM address `va` (WRITE_DATA, or SDMA's WRITE_LINEAR).
fn ib_write(which: Q, va: u64, value: u32) -> Result<Vec<u32>, VmError> {
    let mut v = Vec::new();
    let r = match which {
        Q::Sdma => spkt::write_linear(&mut v, va, &[value]),
        _ => pm4::write_data(&mut v, pm4::dst_sel::MEM, va, pm4::WR_CONFIRM, &[value]),
    };
    r.map_err(|_| VmError::Ring(RingError::State))?;
    if which == Q::Sdma {
        // SDMA reads IBs in 32-byte units: NOP-pad to 8 dwords.
        while v.len() % 8 != 0 {
            v.push(0);
        }
    }
    Ok(v)
}

/// The IB `ib` (dwords) at the queue's IB page + `off`, run in VMID `vmid`, then the ring's fence `seq`. The process
/// queue's IB names VMID 0 and asks to inherit its MQD's (`IB_INHERIT_VMID`; Linux gives MES queues' jobs no VMID).
#[allow(clippy::too_many_arguments)]
fn run_ib<H: Hw>(owner: &mut Bringup<Watch<H>>, q: &mut Queue, which: Q, c: &Ctx, off: u64, ib: &[u32], seq: u64, name: &'static str, doorbells: &mut Vec<(u32, u64, bool)>) -> Result<Rung, VmError> {
    mes::mem(owner, c.ib_mc + off, &mes::dwords(ib)).map_err(mem_err)?;
    mes::mem(owner, c.fence, &[0; 8]).map_err(mem_err)?;
    owner.store_barrier();
    let va = which.ib_va() + off;
    let n = ib.len() as u32;
    let vmid = c.vmid;
    let fence_gpu = c.fence_gpu;
    let w = q.submit(
        owner,
        32,
        |g| {
            match which {
                Q::Compute => g.emit(|v| pm4::indirect_buffer(v, va, n, vmid, false, pm4::IB_VALID))?,
                Q::Proc => g.emit(|v| proc_ib(v, va, n))?,
                _ => g.emit_ib(vmid, va, n, 0, false)?,
            }
            g.emit_fence(fence_gpu, seq, false, false)
        },
        doorbells,
    )?;
    let fence = c.fence;
    let mut x = q.wait(owner, c.us, name, w, &mut |hw| mes::rd32(hw, fence) == Some(seq as u32));
    x.fence = mes::rd32(owner, fence).map(u64::from);
    Ok(x)
}

pub(super) fn push(r: &mut VmReport, x: Rung) -> Result<(), VmError> {
    let (pass, done, name, rptr, want) = (x.pass, x.done, x.name, x.rptr, x.wptr);
    r.rungs.push(x);
    match (pass, done) {
        (true, _) => Ok(()),
        (false, true) => Err(VmError::Result { rung: name }),
        (false, false) => Err(VmError::Timeout { rung: name, rptr, want }),
    }
}

pub(super) fn rd_reg(hw: &mut dyn Hw, b: Option<u32>) -> Option<u32> {
    b.and_then(|b| hw.reg_read(b))
}

/// CONTEXTn_PAGE_TABLE_BASE as the CPU reads it.
pub(super) fn ctx_base(hw: &mut dyn Hw, a: &VmAddrs, vmid: u8) -> Option<u64> {
    let lo = rd_reg(hw, a.hub.byte(R::PtBaseLo(vmid)))?;
    let hi = rd_reg(hw, a.hub.byte(R::PtBaseHi(vmid)))?;
    Some(u64::from(lo) | u64::from(hi) << 32)
}

/// gmc_v11_0_process_interrupt's clear: L2_PROTECTION_FAULT_CNTL bit 0 set (WREG32_P(cntl, 1, ~1)), then the
/// status read back (GFXHUB: after a dummy read). Returns the status after.
pub(crate) fn clear_fault<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &VmAddrs, us: u64) -> Result<Option<u32>, VmError> {
    let b = a.hub.byte(R::FaultCntl).ok_or(VmError::NoAddress)?;
    let v = owner.reg_read(b).ok_or(VmError::Read { byte: b })?;
    let e = owner.write_blind(b, v | gmc11::f::FAULT_CLEAR_STATUS);
    if e.outcome != WriteOutcome::Ok {
        return Err(VmError::Write { byte: b });
    }
    let mut polls = 0;
    let mut last = None;
    gmc::poll(owner, us, &mut polls, &mut |hw| {
        last = gmc::read_fault(hw, &a.hub).status;
        last == Some(0)
    });
    // Linux leaves the clear bit to the hardware; the fake and the chip both drop it. Restore the control value
    // if it still reads with the bit (no write when it reads as before).
    if owner.reg_read(b).is_some_and(|x| x & gmc11::f::FAULT_CLEAR_STATUS != 0) && owner.write_blind(b, v).outcome != WriteOutcome::Ok {
        return Err(VmError::Write { byte: b });
    }
    klog::event!(Info, "gx", "vm.fault_cleared", status = klog::Hex(u64::from(last.unwrap_or(u32::MAX))), polls = polls);
    Ok(last)
}

/// The four rungs of one kernel queue.
fn rungs<H: Hw>(owner: &mut Bringup<Watch<H>>, q: &mut Queue, which: Q, c: &Ctx, r: &mut VmReport) -> Result<(), VmError> {
    let names = which.rungs();
    let mut t = Timeline::new(0, 16);
    let mut seq = || t.emit().map_err(|_| VmError::Ring(RingError::State));
    // 1. The flush (and the PASID entry), then a fence.
    let mut flush = Vec::new();
    match (which, &q) {
        (Q::Sdma, _) => sdma_flush(&mut flush, c.a, c.vmid, c.pd, which.engine(), c.pasid)?,
        (_, Queue::Pm4 { gfx, .. }) => pm4_flush(&mut flush, c.a, c.vmid, c.pd, which.engine(), *gfx, c.pasid, c.ack)?,
        _ => return Err(VmError::NoAddress),
    }
    mes::mem(owner, c.fence, &[0; 8]).map_err(mem_err)?;
    owner.store_barrier();
    let s = seq()?;
    let fence = c.fence;
    let w = q.submit(owner, flush.len() as u64 + 16, |g| {
        g.write(&flush)?;
        g.emit_fence(fence, s, false, false)
    }, &mut r.doorbells)?;
    let mut x = q.wait(owner, c.us, names[0], w, &mut |hw| mes::rd32(hw, fence) == Some(s as u32));
    x.fence = mes::rd32(owner, fence).map(u64::from);
    let base = ctx_base(owner, c.a, c.vmid);
    let lut = rd_reg(owner, c.a.lut(c.vmid));
    // The engine's ack register after the fence: SDMA's poll gives up after its retry count and goes on, so its
    // ack is not proven by the fence (the PM4 wait has no retry limit: the fence proves it there).
    let ack = rd_reg(owner, c.a.hub.byte(R::Ack(which.engine())));
    let acked = ack.is_some_and(|v| v != u32::MAX && v >> c.vmid & 1 != 0);
    r.ctx_read = base;
    r.lut_read = lut;
    if x.pass {
        x.pass = base == Some(c.pd) && (c.a.lut0.is_none() || lut == Some(c.pasid)) && (which != Q::Sdma || acked);
    }
    x.detail = format!(
        "{} dwords: CONTEXT{}_PAGE_TABLE_BASE <- {:#x}, INVALIDATE_ENG{}_REQ <- {:#x}, ack bit {} awaited{}{}; fence {s} read {}; the CPU reads the base {}, IH_VMID_{}_LUT {} and INVALIDATE_ENG{}_ACK {}{}",
        flush.len(),
        c.vmid,
        c.pd,
        which.engine(),
        gmc11::invalidate_req(c.vmid, 0),
        c.vmid,
        if which == Q::Gfx { ", PFP_SYNC_ME" } else { "" },
        if c.a.lut0.is_some() { format!(", IH_VMID_{}_LUT <- PASID {:#x}", c.vmid, c.pasid) } else { String::from(", no IH LUT address") },
        hx(x.fence),
        hx(base),
        c.vmid,
        hx32(lut),
        which.engine(),
        hx32(ack),
        if which == Q::Sdma && !acked { " (SDMA: the ack bit is not set: the poll gave up)" } else { "" }
    );
    push(r, x)?;
    vm_rungs(owner, q, which, c, r, &mut t)
}

/// Rungs 2-4 of a queue whose VM is in place: an IB in the VM, the intended fault, an IB after it.
pub(super) fn vm_rungs<H: Hw>(owner: &mut Bringup<Watch<H>>, q: &mut Queue, which: Q, c: &Ctx, r: &mut VmReport, t: &mut Timeline) -> Result<(), VmError> {
    let names = which.rungs();
    let mut seq = || t.emit().map_err(|_| VmError::Ring(RingError::State));
    // 2. An IB in the VM writes a VM address.
    let tva = which.target_va();
    mes::mem(owner, c.target_mc, &[POISON.to_le_bytes(), POISON.to_le_bytes()].concat()).map_err(mem_err)?;
    let s = seq()?;
    let mut x = run_ib(owner, q, which, c, IB_WRITE, &ib_write(which, tva, VM_VALUE)?, s, names[1], &mut r.doorbells)?;
    let got = mes::rd32(owner, c.target_mc);
    let f = gmc::read_fault(owner, &c.a.hub);
    if x.pass {
        x.pass = got == Some(VM_VALUE) && f.status == Some(0);
    }
    x.detail = format!("IB at VA {:#x} (VMID {}) writes {VM_VALUE:#x} to VA {tva:#x}: the page (MC {:#x}) reads {}; fence {s} read {}; GFXHUB fault status {}{}", which.ib_va() + IB_WRITE, c.vmid, c.target_mc, hx32(got), hx(x.fence), f.text(), fetch_refused(&f, which.ib_va()).unwrap_or_default());
    push(r, x)?;
    // 3. The intended fault: an unmapped page of the same table; the fence after it must land.
    let hole = which.hole_va();
    let s = seq()?;
    let mut x = run_ib(owner, q, which, c, IB_FAULT, &ib_write(which, hole, FAULT_VALUE)?, s, names[2], &mut r.doorbells)?;
    let mut seen = FaultSeen { queue: which, read: FaultRead { hub: Hub::Gfx, status: None, addr: None }, want_page: hole & 0x0000_ffff_ffff_f000, intended: false, cleared: None, stalled: false };
    // The registers are read whether the fence landed or not: a stalled queue's fault is what the rung is about.
    let mut polls = 0;
    let hub = c.a.hub;
    let mut last = gmc::read_fault(owner, &hub);
    gmc::poll(owner, c.us, &mut polls, &mut |hw| {
        last = gmc::read_fault(hw, &hub);
        last.status.is_some_and(|s| s != 0 && s != u32::MAX)
    });
    seen.read = last;
    let d = Fault::decode(last.status.unwrap_or(0));
    let vm_ok = if c.mes { vmproc::is_mes_vmid(d.vmid) && (c.vmid == 0 || d.vmid == u32::from(c.vmid)) } else { d.vmid == u32::from(c.vmid) };
    seen.intended = last.status.is_some_and(|s| s != 0 && s != u32::MAX) && vm_ok && last.addr == Some(seen.want_page) && d.write;
    seen.stalled = !x.done && seen.intended && which.may_stall();
    klog::event!(Info, "gx", "vm.fault", queue = which.name(), status = klog::Hex(u64::from(last.status.unwrap_or(u32::MAX))), addr = klog::Hex(last.addr.unwrap_or(u64::MAX)), intended = seen.intended, stalled = seen.stalled);
    let state = if x.done { String::new() } else { format!("; at the stall {}", vmrecover::state(owner, which, c.inp)) };
    seen.cleared = clear_fault(owner, c.a, c.us)?;
    let mut back = None;
    if seen.stalled {
        let s2 = seq()?;
        back = Some(vmrecover::recover(owner, q, which, c, s2, r));
        seen.cleared = vmrecover::hub_status(owner, c);
    }
    if x.done || seen.stalled {
        x.pass = seen.intended && seen.cleared == Some(0);
    }
    x.detail = format!(
        "IB at VA {:#x} writes {FAULT_VALUE:#x} to the unmapped VA {hole:#x}: fence {s} read {} ({}); GFXHUB {}; {} (want VMID {}, page {:#x}, write); after the clear the status reads {}{state}",
        which.ib_va() + IB_FAULT,
        hx(x.fence),
        if x.done {
            "the ring went on"
        } else if seen.stalled {
            "the queue stalled at its fault, as this engine does by design: see the next rung"
        } else {
            "the ring stopped"
        },
        seen.read.text(),
        if seen.intended { "the intended fault" } else { "NOT the intended fault" },
        match (c.mes, c.vmid) {
            (false, n) => format!("{n}"),
            (true, 0) => String::from("8-15 (MES's)"),
            (true, n) => format!("{n} (MES's)"),
        },
        seen.want_page,
        hx32(seen.cleared)
    );
    r.faults.push(seen);
    push(r, x)?;
    if let Some(b) = back {
        push(r, b)?;
    }
    // 4. The ring survived: one more IB in the VM.
    let s = seq()?;
    let mut x = run_ib(owner, q, which, c, IB_AFTER, &ib_write(which, tva + 4, AFTER_VALUE)?, s, names[3], &mut r.doorbells)?;
    let got = mes::rd32(owner, c.target_mc + 4);
    let f = gmc::read_fault(owner, &c.a.hub);
    if x.pass {
        x.pass = got == Some(AFTER_VALUE) && f.status == Some(0);
    }
    x.detail = format!("IB at VA {:#x} writes {AFTER_VALUE:#x} to VA {:#x}: the page reads {}; fence {s} read {}; GFXHUB fault status {}", which.ib_va() + IB_AFTER, tva + 4, hx32(got), hx(x.fence), f.text());
    push(r, x)
}

/// A kept PM4 queue at its wptr shadow (idle: the rptr writeback there too).
fn pm4_queue(hw: &mut dyn Hw, name: &'static str, base: u64, bytes: u64, rptr: u64, wptr: u64, doorbell: u32, gfx: bool, found: &mut Vec<(&'static str, Option<u64>, Option<u64>)>) -> Result<Queue, VmError> {
    let (rp, wp) = (mes::rd32(hw, rptr).map(u64::from), mes::rd64(hw, wptr));
    found.push((name, rp, wp));
    let size = bytes / 4;
    let at = wp.ok_or(VmError::NotIdle { queue: name, rptr: rp, wptr: wp })?;
    if rp != Some(at & (size - 1)) && rp != Some(at & 0xffff_ffff) {
        return Err(VmError::NotIdle { queue: name, rptr: rp, wptr: wp });
    }
    let ring = Ring::new(Kind::Gfx, size, at).map_err(VmError::Ring)?;
    Ok(Queue::Pm4 { q: PmQueue { ring, base, rptr_wb: Some(rptr), wptr_mc: wptr, doorbell, rptr_dw: at }, gfx })
}

/// SDMA 0's kept queue: running (RB enabled, F32 unhalted, doorbell on) and idle at its wptr shadow.
pub(super) fn sdma_queue(hw: &mut dyn Hw, s: &SdmaSide, found: &mut Vec<(&'static str, Option<u64>, Option<u64>)>) -> Result<Queue, VmError> {
    let name = Q::Sdma.name();
    let l = s.layout;
    let (rp, wp) = (mes::rd64(hw, l.rptr), mes::rd64(hw, l.wptr));
    found.push((name, rp, wp));
    let rb = rd_reg(hw, s.addrs.byte(sdma6::R::RbCntl));
    let f32 = rd_reg(hw, s.addrs.byte(sdma6::R::F32Cntl));
    let db = rd_reg(hw, s.addrs.byte(sdma6::R::Doorbell));
    let runs = rb.is_some_and(|v| v != u32::MAX && v & sdma6::f::RB_ENABLE != 0) && f32.is_some_and(|v| v & sdma6::f::F32_HALT == 0) && db.is_some_and(|v| v != u32::MAX && v & sdma6::f::DOORBELL_ENABLE != 0);
    let kind = Kind::Sdma { burst_nop: s.burst_nop };
    match (runs, wp) {
        (true, Some(w)) if rp == Some(w) && w % 4 == 0 => {
            let at = w >> 2;
            let ring = Ring::new(kind, sdma6::RING_DW, at).map_err(VmError::Ring)?;
            Ok(Queue::Sdma(SdmaQ { ring, base: l.ring, rptr_mc: l.rptr, wptr_mc: l.wptr, kind, rptr_dw: at }))
        }
        _ => Err(VmError::NotIdle { queue: name, rptr: rp, wptr: wp }),
    }
}

/// GX-8b: the three kernel queues the backend's jobs run on, as a kept chain left them: idle, at their write pointers.
/// Nothing is written.
pub(crate) fn kept_queues(hw: &mut dyn Hw, inp: &VmInputs) -> Result<[Queue; 3], VmError> {
    let (l, g) = (inp.mes, inp.gfx);
    if [l.compute_state, l.gfx_state].iter().any(|&s| matches!(mes::rd64(hw, s), Some(mes::COMPUTE_UNMAPPED | mes::COMPUTE_STUCK))) {
        return Err(VmError::ComputeUnmapped);
    }
    let mut found = Vec::new();
    let gfx = pm4_queue(hw, Q::Gfx.name(), g.ring, gfx11::RING_DW * 4, g.rptr, g.wptr, gfx11::DOORBELL_DW, true, &mut found)?;
    let compute = pm4_queue(hw, Q::Compute.name(), l.compute_ring, mes::COMPUTE_RING_BYTES, l.comp_rptr, l.comp_wptr, mes11::COMPUTE_DOORBELL_DW, false, &mut found)?;
    let side = inp.sdma.as_ref().ok_or(VmError::NoAddress)?;
    Ok([gfx, compute, sdma_queue(hw, side, &mut found)?])
}

/// The process queue removed first (REMOVE_QUEUE, when MES answered its ADD_QUEUE), then context n's base and LUT
/// entry back to 0 by the CPU and a CPU flush of VMID n on engine 17 (bounded): the VM is unreachable afterwards.
pub fn teardown<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &VmAddrs, up: &VmUp, us: u64, api_us: u64) -> Teardown {
    let mut t = Teardown { ok: true, ..Teardown::default() };
    if let Some(p) = up.proc {
        let removed = vmproc::remove(owner, &p, api_us, &mut t.lines);
        t.stranded = !removed;
        t.ok &= removed;
    }
    let n = up.vmid;
    let mut cpu_ok = true;
    for (what, b) in [("CONTEXT_PAGE_TABLE_BASE_LO", a.hub.byte(R::PtBaseLo(n))), ("CONTEXT_PAGE_TABLE_BASE_HI", a.hub.byte(R::PtBaseHi(n))), ("IH_VMID_LUT", a.lut(n))] {
        match b {
            Some(b) => {
                let e = owner.write_blind(b, 0);
                if e.outcome != WriteOutcome::Ok {
                    cpu_ok = false;
                    t.lines.push(format!("vm teardown: {what} of VMID {n} ({b:#x}) not written ({:?})", e.outcome));
                }
            }
            None if what == "IH_VMID_LUT" => {}
            None => cpu_ok = false,
        }
    }
    let req = gmc11::invalidate_req(n, 0);
    let (rq, ak) = (a.hub.byte(R::Req(CPU_ENGINE)), a.hub.byte(R::Ack(CPU_ENGINE)));
    let mut acked = false;
    let mut polls = 0;
    if let (Some(rq), Some(ak)) = (rq, ak)
        && owner.write_blind(rq, req).outcome == WriteOutcome::Ok
    {
        acked = gmc::poll(owner, us, &mut polls, &mut |hw| hw.reg_read(ak).is_some_and(|x| x != u32::MAX && x >> n & 1 != 0));
    }
    cpu_ok &= acked;
    t.ok &= cpu_ok;
    if !cpu_ok {
        t.retry = Some(VmUp { proc: None, ..*up });
    }
    klog::event!(Info, "gx", "vm.teardown", vmid = n, pasid = up.pasid, acked = acked, polls = polls, retry = !cpu_ok);
    t.lines.push(format!(
        "vm teardown: VMID {n} (PASID {:#x}): its page-table base and IH_VMID_{n}_LUT set to 0 by the CPU, flushed on engine {CPU_ENGINE} (request {req:#x}): {}{}",
        up.pasid,
        if acked { "acked" } else { "NO ACK" },
        if cpu_ok { "" } else { "; handed back for a retry" }
    ));
    t
}

/// After a failed run's teardown: a stalled ring may still reach the VM (now base 0) and fault late. One bounded
/// look; a fault found is cleared and reported (the next stage's "no new fault" checks start clean).
fn late_fault<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &VmAddrs, us: u64, r: &mut VmReport) {
    let mut polls = 0;
    let mut last = gmc::read_fault(owner, &a.hub);
    gmc::poll(owner, us, &mut polls, &mut |hw| {
        last = gmc::read_fault(hw, &a.hub);
        last.status.is_some_and(|s| s != 0 && s != u32::MAX)
    });
    if last.status.is_some_and(|s| s != 0 && s != u32::MAX) {
        klog::event!(Warn, "gx", "vm.late_fault", status = klog::Hex(u64::from(last.status.unwrap_or(0))), addr = klog::Hex(last.addr.unwrap_or(0)); "a fault after the failed run's teardown: cleared");
        r.late_fault = Some(last);
        let after = clear_fault(owner, a, us).ok().flatten();
        r.teardown.push(format!("vm: a fault came after the teardown (a stalled ring's late access): {}; cleared, the status reads {}", last.text(), hx32(after)));
    }
}

/// The stage. Nothing is written before every kept queue reads idle.
pub fn enable<H: Hw>(owner: &mut Bringup<Watch<H>>, inp: &VmInputs, prev: Option<VmUp>) -> VmReport {
    let mut r = VmReport::default();
    if let Some(p) = prev {
        let td = teardown(owner, &inp.addrs, &p, inp.flush_us, inp.api_us);
        r.teardown.extend(td.lines.into_iter().map(|l| format!("{l} (an earlier until=vm's VM, before this run)")));
        r.stranded |= td.stranded;
        if td.retry.is_some() {
            klog::event!(Error, "gx", "vm.prev_teardown_failed", vmid = p.vmid; "the earlier VM stays: nothing new built");
            r.retry = td.retry;
            r.error = Some(VmError::PrevTeardown);
            r.fault_after = Some(gmc::read_fault(owner, &inp.addrs.hub));
            return r;
        }
    }
    let res = run(owner, inp, &mut r);
    if let Err(e) = res {
        klog::event!(Error, "gx", "vm.failed", error = format!("{e:?}").as_str(), rungs = r.rungs.len());
        r.error = Some(e);
    }
    r.fault_after = Some(gmc::read_fault(owner, &inp.addrs.hub));
    let up = r.vmid.zip(r.pasid).zip(r.pd).map(|((vmid, pasid), pd)| VmUp { vmid, pasid, pd, proc: r.proc });
    let pass = r.error.is_none() && checks(&r).iter().all(|c| c.pass);
    match up {
        Some(u) if pass => {
            r.kept = Some(u);
            klog::event!(Info, "gx", "vm.kept", vmid = u.vmid, pasid = u.pasid, proc = u.proc.is_some(); "the test VM stays until until=stop");
        }
        Some(u) => {
            // A fault this run left is cleared before the teardown (the next stage's "no new fault" checks).
            if r.fault_after.is_some_and(|f| f.status.is_some_and(|s| s != 0 && s != u32::MAX)) {
                let _ = clear_fault(owner, &inp.addrs, inp.flush_us);
            }
            let td = teardown(owner, &inp.addrs, &u, inp.flush_us, inp.api_us);
            r.teardown.extend(td.lines);
            r.retry = td.retry;
            r.stranded |= td.stranded;
            late_fault(owner, &inp.addrs, inp.flush_us, &mut r);
        }
        None => {}
    }
    r
}

fn run<H: Hw>(owner: &mut Bringup<Watch<H>>, inp: &VmInputs, r: &mut VmReport) -> Result<(), VmError> {
    let a = inp.addrs;
    if inp.mes_bo.size < lay::END {
        return Err(VmError::BoTooSmall { have: inp.mes_bo.size, need: lay::END });
    }
    // 1. The kept queues, idle (nothing written before).
    let l = inp.mes;
    let g = inp.gfx;
    if matches!(mes::rd64(owner, l.compute_state), Some(mes::COMPUTE_UNMAPPED | mes::COMPUTE_STUCK)) {
        return Err(VmError::ComputeUnmapped);
    }
    let mut queues: Vec<(Q, Queue)> = Vec::new();
    queues.push((Q::Gfx, pm4_queue(owner, Q::Gfx.name(), g.ring, gfx11::RING_DW * 4, g.rptr, g.wptr, gfx11::DOORBELL_DW, true, &mut r.found)?));
    queues.push((Q::Compute, pm4_queue(owner, Q::Compute.name(), l.compute_ring, mes::COMPUTE_RING_BYTES, l.comp_rptr, l.comp_wptr, mes11::COMPUTE_DOORBELL_DW, false, &mut r.found)?));
    match inp.sdma.as_ref() {
        Some(s) => queues.push((Q::Sdma, sdma_queue(owner, s, &mut r.found)?)),
        None => r.sdma_skipped = Some(String::from("no SDMA 0 queue kept in this boot (until=sdma with keep before gfx_mes): the SDMA rungs did not run")),
    }
    // 2. A fault GFXHUB already holds (an earlier run's stray) is reported and cleared first.
    let f0 = gmc::read_fault(owner, &a.hub);
    if f0.status.is_some_and(|s| s != 0 && s != u32::MAX) {
        klog::event!(Warn, "gx", "vm.fault_found", status = klog::Hex(u64::from(f0.status.unwrap_or(0))); "GFXHUB held a fault before the stage: cleared first");
        r.found_fault = Some(f0);
        if let Some(s) = clear_fault(owner, &a, inp.flush_us)?
            && s != 0
        {
            return Err(VmError::Unclean { status: s });
        }
    }
    // 3. A VMID and a PASID (nothing written when there is none), then the VM: tables, the mapping.
    let mut ids = VmidMgr::new(inp.vmids.0, inp.vmids.1).map_err(|_| VmError::NoVmid)?;
    let pasid = PasidAlloc::new().alloc(PASID_BITS).map_err(|_| VmError::NoPasid)?;
    let base = inp.carve.mc_base() + inp.mes_bo.offset;
    mes::zero(owner, base + lay::TABLES, lay::END - lay::TABLES).map_err(mem_err)?;
    let data_phys = inp.carve.phys_of(inp.mes_bo.offset + lay::DATA).ok_or(VmError::NoAddress)?;
    let pages = (lay::DATA_END - lay::DATA) / 4096;
    let flags = MAP_FLAGS;
    let (pd, root, tables) = {
        let off = inp.mes_bo.offset;
        let mut store = CarveTables::new(owner, inp.carve, off + lay::TABLES..off + lay::TABLES_END).ok_or(VmError::NoAddress)?;
        let mut vm = Vm::new(&mut store, inp.carve).map_err(VmError::Tables)?;
        vm.map(&mut store, VA_BASE, pages * 4096, Backing::Local { phys: data_phys }, flags).map_err(VmError::Tables)?;
        if store.failed() {
            return Err(VmError::Mem { mc: base + lay::TABLES });
        }
        (vm.pd_addr(), vm.root().phys, vm.table_count())
    };
    owner.store_barrier();
    r.pd = Some(pd);
    r.tables = tables;
    r.mapping = Some((VA_BASE, pages, data_phys));
    let grab = ids.grab(Owner(inp.token), pd, 1, false).ok().flatten().ok_or(VmError::NoVmid)?;
    let vmid = grab.vmid as u8;
    r.vmid = Some(vmid);
    r.pasid = Some(pasid);
    klog::event!(Info, "gx", "vm.built", vmid = vmid, pasid = pasid, pd = klog::Hex(pd), tables = tables, va = klog::Hex(VA_BASE), pages = pages);
    // 4. The rungs, queue by queue. A queue that fails is on record and the next one still runs: one round on the
    // chip tells as much as it can. The first failure is the stage's error.
    let mut failed = None;
    for (which, mut q) in queues {
        let i = which.index();
        clean_left_fault(owner, inp, which, r);
        let c = Ctx {
            a: &a,
            vmid,
            pd,
            pasid,
            ib_mc: base + lay::DATA + i * 0x2000,
            target_mc: base + lay::DATA + i * 0x2000 + 0x1000,
            fence: base + lay::FENCES + i * 0x40,
            fence_gpu: base + lay::FENCES + i * 0x40,
            us: inp.timeout_us,
            ack: inp.ack,
            mes: false,
            inp,
            proc: None,
        };
        if let Err(e) = rungs(owner, &mut q, which, &c, r) {
            klog::event!(Error, "gx", "vm.queue_failed", queue = which.name(), error = format!("{e:?}").as_str(); "the next queue still runs");
            failed.get_or_insert(e);
        }
    }
    // 5. The same VM through a MES process queue.
    if inp.proc {
        clean_left_fault(owner, inp, Q::Proc, r);
        if let Err(e) = vmproc::run(owner, inp, root, pasid, pd, r) {
            klog::event!(Error, "gx", "vm.queue_failed", queue = Q::Proc.name(), error = format!("{e:?}").as_str());
            failed.get_or_insert(e);
        }
    } else {
        r.proc_skipped = Some(String::from("the MES process queue rungs are off for this run"));
    }
    failed.map_or(Ok(()), Err)
}

/// GX-7m3: a fault an earlier queue's failure left would read as this queue's: on record with the status after its
/// clear. A pass leaves none.
fn clean_left_fault<H: Hw>(owner: &mut Bringup<Watch<H>>, inp: &VmInputs, next: Q, r: &mut VmReport) {
    let f = gmc::read_fault(owner, &inp.addrs.hub);
    if f.status.is_some_and(|s| s != 0 && s != u32::MAX) {
        klog::event!(Warn, "gx", "vm.fault_left", status = klog::Hex(u64::from(f.status.unwrap_or(0))), next = next.name(); "an earlier queue left a fault: cleared before the next queue");
        let res = clear_fault(owner, &inp.addrs, inp.flush_us);
        let after = res.as_ref().ok().copied().flatten();
        if after != Some(0) {
            klog::event!(Error, "gx", "vm.fault_left_uncleared", next = next.name(), res = format!("{res:?}").as_str(); "the left fault did not clear: the next queue's fault rung reports it");
        }
        r.left_faults.push((next.name(), f, after));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmCheck {
    pub name: &'static str,
    pub pass: bool,
    pub why: String,
}

/// The rungs the stage must show (the SDMA ones unless skipped; a stalled queue's recovery).
pub fn expected(r: &VmReport) -> Vec<&'static str> {
    let mut v = Vec::new();
    let mut queue = |q: Q| {
        let n = q.rungs();
        v.extend(&n[..3]);
        // GX-7m3: a queue that stalled at its fault must also show that it came back.
        if r.faults.iter().any(|f| f.queue == q && f.stalled) {
            v.push(q.recover_rung());
        }
        v.push(n[3]);
    };
    queue(Q::Gfx);
    queue(Q::Compute);
    if r.sdma_skipped.is_none() {
        queue(Q::Sdma);
    }
    if r.proc_skipped.is_none() {
        queue(Q::Proc);
    }
    v
}

pub fn checks(r: &VmReport) -> Vec<VmCheck> {
    let mut v = Vec::new();
    v.push(VmCheck {
        name: "VM built (tables, mapping, VMID, PASID)",
        pass: r.pd.is_some() && r.vmid.is_some_and(|x| (FIRST_VMID..FIRST_VMID + VMIDS).contains(&u32::from(x))) && r.pasid.is_some_and(|p| p != 0),
        why: format!("VMID {}, PASID {}, base {}, {} tables", r.vmid.map_or(String::from("none"), |x| format!("{x}")), hx32(r.pasid), hx(r.pd), r.tables),
    });
    for name in expected(r) {
        match r.rungs.iter().find(|x| x.name == name) {
            Some(x) => v.push(VmCheck { name, pass: x.pass, why: format!("wptr {}, rptr writeback {}, {} polls, {} us; {}", x.wptr, hx(x.rptr), x.polls, x.us, x.detail) }),
            None => v.push(VmCheck { name, pass: false, why: String::from("not run (an earlier rung failed)") }),
        }
    }
    v.push(VmCheck {
        name: "VM no fault left",
        pass: r.fault_after.is_some_and(|f| f.status == Some(0)),
        why: format!("GFXHUB after: {}", r.fault_after.map_or(String::from("unread"), |f| f.text())),
    });
    v
}

/// The stage's lines for the answer.
pub fn render(r: &VmReport) -> Vec<String> {
    let mut v = Vec::new();
    v.extend(r.teardown.iter().filter(|l| l.ends_with("before this run)")).cloned());
    for (q, rp, wp) in &r.found {
        v.push(format!("vm found: {q} rptr writeback {}, wptr shadow {}", hx(*rp), hx(*wp)));
    }
    if let Some(s) = &r.sdma_skipped {
        v.push(format!("vm: {s}"));
    }
    if let Some(s) = &r.proc_skipped {
        v.push(format!("vm: {s}"));
    }
    if let Some(f) = r.found_fault {
        v.push(format!("vm: GFXHUB held a fault before the stage (cleared first): {}", f.text()));
    }
    if let (Some(vmid), Some(pasid), Some(pd)) = (r.vmid, r.pasid, r.pd) {
        v.push(format!("vm built: VMID {vmid} (kernel range {FIRST_VMID}-{}; MES owns 8-15), PASID {pasid:#x}, page-table base {pd:#x}, {} tables", FIRST_VMID + VMIDS - 1, r.tables));
    }
    if let Some((va, pages, phys)) = r.mapping {
        v.push(format!("vm mapping: VA {va:#x}..{:#x} ({pages} pages, uncached, read/write/execute) -> host-physical {phys:#x}; holes at VA {:#x}, {:#x}, {:#x}, {:#x}", va + pages * 4096, Q::Gfx.hole_va(), Q::Compute.hole_va(), Q::Sdma.hole_va(), Q::Proc.hole_va()));
    }
    v.extend(vmproc::render(r));
    for x in &r.rungs {
        v.push(format!("vm {}: {} ({})", x.name, if x.pass { "ok" } else { "FAIL" }, x.detail));
    }
    for f in &r.faults {
        v.push(format!(
            "gfx vm fault: {}: {} (VMID {}, page {:#x}; want {:#x}): {}{}",
            f.queue.name(),
            f.read.text(),
            Fault::decode(f.read.status.unwrap_or(0)).vmid,
            f.read.addr.unwrap_or(0),
            f.want_page,
            if f.intended { "intended" } else { "STRAY" },
            if f.stalled { ", the queue stalled at it" } else { "" }
        ));
    }
    for (next, f, after) in &r.left_faults {
        let done = if *after == Some(0) { String::from("cleared") } else { format!("NOT cleared (the status reads {})", hx32(*after)) };
        v.push(format!("vm: a failed queue left a fault, {done} before {next}: {}", f.text()));
    }
    v.push(format!("vm after: CONTEXT_PAGE_TABLE_BASE {}, IH LUT {}, GFXHUB {}", hx(r.ctx_read), hx32(r.lut_read), r.fault_after.map_or(String::from("unread"), |f| f.text())));
    for l in r.teardown.iter().filter(|l| !l.ends_with("before this run)")) {
        v.push(l.clone());
    }
    if let Some(e) = &r.error {
        v.push(format!("vm error: {e:?} (errno {}){}", e.errno(), e.advice()));
    }
    match (r.kept, r.retry) {
        (Some(u), _) => v.push(format!("vm: VMID {} stays mapped{} (until=stop tears it down)", u.vmid, if u.proc.is_some() { ", the MES process queue with it" } else { "" })),
        (None, Some(u)) => v.push(format!("vm: VMID {}'s teardown failed: kept for a retry at the next until=stop or until=vm", u.vmid)),
        (None, None) => v.push(String::from("vm: no VM left")),
    }
    if r.stranded {
        v.push(String::from("vm: STRANDED: MES did not remove or map a queue of this stage (a stalled queue or the process queue), or the compute ring's HQD stayed active after MES removed it; until=stop halts MES, then reboot before the CP runs again"));
    }
    v.push(format!("vm verdict (stage): {}", if r.passed() { "PASS" } else { "FAIL" }));
    v
}
