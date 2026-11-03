//! GX-7k: MES 11 on GC 11.0.1 as Linux 7.1 brings it up (the 27.10. capture, trace-load lines 41680-41715, and
//! 7.1 mes_v11_0.c: kiq_hw_init :1570, hw_init :1642). The PSP loaded both MES pipes' microcode (the scheduler's
//! `_mes_2` as CP_MES/CP_MES_DATA, the KIQ's `_mes1` as CP_MES1/CP_MES1_DATA; ListOpts::mes_kiq) and the RLC
//! autoload put it in place; the GFX stage (`gfxring`, `Variant::Mes`) ran the GFX tail and Linux's CP unhalt
//! (MEC, then ME/PFP) and left the CP running. Then, as Linux:
//! 1. `enable`: CP_MES_CNTL with both pipe resets, per pipe (GRBM_GFX_CNTL ME 3 pipe p) CP_MES_PRGRM_CNTR_START/
//!    _HI from the firmware header's uc start, both pipes active (halt cleared), 500 us;
//! 2. the firmware versions (CP_MES_GP3_LO per pipe; legacy queues through MES need 0x47);
//! 3. kiq_setting (RLC_CP_SCHEDULERS names ME 3 pipe 1 queue 0 the KIQ);
//! 4. the KIQ's queue on pipe 1 from its MQD straight into the HQD registers (queue_init_register);
//! 5. the scheduler's queue on pipe 0: its MQD mapped by the KIQ (MAP_QUEUES, ME 2, ENGINE_SEL 5), then the KIQ
//!    ring test (WRITE_DATA 0xDEADBEEF into SCRATCH_REG0);
//! 6. the MES API on the scheduler ring: SET_HW_RESOURCES with the 780M's masks, SET_HW_RESOURCES_1 (firmware
//!    0x52 and newer), QUERY_SCHEDULER_STATUS; each frame followed by a QUERY_SCHEDULER_STATUS whose fence is the
//!    ring's sequence number (submit_pkt_and_poll_completion :167);
//! 7. the self test: MISC WRITE_REG and READ_REG of SCRATCH_REG0 through MES, checked by the CPU.
//! `until=gfx_mes` goes on in `mesq`: a compute ring and GFX ring 0 mapped through MES (ADD_QUEUE with
//! map_legacy_kq, as 7.1 amdgpu_gfx_enable_kcq/_kgq), cp_gfx_start, ring tests, IB tests, RELEASE_MEM fences.
//!
//! Every MQD, ring, EOP buffer and writeback slot lies in one carve-out BO kept for the boot, reached through
//! GFXHUB's system aperture: Linux puts the MES MQDs in VRAM too (mqd_sw_init: VRAM|GTT; the capture's KIQ MQD is
//! at MC 0x80_1fca3000, the carve-out), and the CPU reads the carve-out uncached (no snoop question, no GART
//! needed). The KIQ ring is Linux's 1 MiB (the capture's CP_HQD_PQ_CONTROL QUEUE_SIZE 0x11), so the regtrace
//! phase `mes` matches the capture write for write; the BO is one 2 MiB piece at any carve-out size.
//! A failure after MES started first unmaps the legacy queues MES already mapped (REMOVE_QUEUE unmap_legacy_queue,
//! as `until=stop`; not when MES itself stopped answering), then dequeues both MES pipes and halts MES again
//! (mes_v11_0_kiq_hw_fini); queues MES could not unmap are reported as stranded. After a pass
//! MES stays running (Linux's state) until `until=stop` (unmap, dequeue, halt) or the Quiesce hook (halt, raw).

use super::bringup::{Bringup, Outcome as WriteOutcome};
use super::disc::Discovery;
use super::gfxring;
use super::gmc::{self, FaultRead, HubAddrs};
use super::hwid;
use super::psp::{BoAlloc, Hw};
use super::watch::Watch;
use crate::bo::{BoError, PAGE};
use crate::ipmod::gfx11::{self, R};
use crate::ipmod::mes11::{self, HwMasks, M, f};
use crate::ipmod::mesapi::{self, Frame};
use crate::ipmod::mesmqd::{self, Prop};
use crate::ipmod::nbio7;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::addr::Carveout;
use amdgpu_fmt::ring::{Kind, Ring, RingError, RingSink};
use amdgpu_fmt::wb::WbPool;

/// The BO's layout (offsets from its start).
pub mod lay {
    pub const SCHED_MQD: u64 = 0x0000;
    pub const KIQ_MQD: u64 = 0x1000;
    pub const GFX_MQD: u64 = 0x2000;
    pub const COMPUTE_MQD: u64 = 0x3000;
    pub const SCHED_EOP: u64 = 0x4000;
    pub const KIQ_EOP: u64 = 0x4800;
    pub const COMPUTE_EOP: u64 = 0x5000;
    pub const WB: u64 = 0x6000;
    pub const TARGET: u64 = 0x7000;
    pub const IB: u64 = 0x8000;
    pub const CLEANER: u64 = 0x9000;
    /// The part zeroed at every run (MQDs, EOPs, writeback, targets, IB, the cleaner page).
    pub const SMALL_END: u64 = 0xA000;
    pub const SCHED_RING: u64 = 0x1_0000;
    pub const COMPUTE_RING: u64 = 0x2_0000;
    pub const KIQ_RING: u64 = 0x10_0000;
    pub const END: u64 = 0x20_0000;
}
pub const BO_BYTES: u64 = lay::END;
/// Linux's KIQ ring (QUEUE_SIZE 0x11 in the capture: 2^18 dwords), the MES ring (ours: 16 KiB, 32 submissions
/// of two frames), the compute ring (Linux's 8 KiB, as GFX ring 0).
pub const KIQ_RING_BYTES: u64 = 0x10_0000;
pub const SCHED_RING_BYTES: u64 = 0x4000;
pub const COMPUTE_RING_BYTES: u64 = 0x2000;

/// GX-7m3: the slot `Layout::compute_state`, what the VM stage's recovery left of the compute ring. In the BO, as the
/// stop's ring position is: a stop needs no state of the run, and every MES run zeroes it (the ring mapped afresh).
/// Mapped: 0. MES answered its REMOVE_QUEUE, the HQD is off, it is not mapped again:
pub const COMPUTE_UNMAPPED: u64 = 1;
/// MES holds no record of it, and its HQD is active or in an unknown state: no firmware owns it (stranded).
pub const COMPUTE_STUCK: u64 = 2;

/// The self test's values (MISC WRITE_REG, then READ_REG of a CPU-written value).
pub const SELFTEST_WRITE: u32 = 0x4D45_5357;
pub const SELFTEST_READ: u32 = 0x4D45_5352;

/// GC's MES/HQD registers, the GFX registers the stage also touches, NBIO's doorbell aperture, and the register
/// bases SET_HW_RESOURCES names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MesAddrs {
    base: [u32; 2],
    pub aper_en: Option<u32>,
    pub gc_base: [u32; 5],
    pub mmhub_base: [u32; 5],
    pub osssys_base: [u32; 5],
}

/// The GFX registers of the stage (read; the first four also written).
const GFX_REGS: [R; 7] = [R::GrbmGfxCntl, R::ScratchReg0, R::CpMaxContext, R::CpDeviceId, R::CpMeCntl, R::CpMecCntl, R::CpStat];

impl MesAddrs {
    pub fn from_discovery(d: &Discovery) -> Option<MesAddrs> {
        let base = [d.reg(hwid::GC, 0, 0, 0)?, d.reg(hwid::GC, 0, 1, 0)?];
        let aper_en = d.reg(hwid::NBIF, 0, nbio7::RCC_DOORBELL_APER_EN.0, nbio7::RCC_DOORBELL_APER_EN.1).and_then(|x| x.checked_mul(4));
        let segs = |hw: u16| core::array::from_fn(|i| d.reg(hw, 0, i, 0).unwrap_or(0));
        Some(MesAddrs { base, aper_en, gc_base: segs(hwid::GC), mmhub_base: segs(hwid::MMHUB), osssys_base: segs(hwid::OSSSYS) })
    }

    pub fn byte(&self, r: M) -> Option<u32> {
        let (seg, dw) = r.at();
        self.base.get(seg)?.checked_add(dw)?.checked_mul(4)
    }

    pub fn gfx(&self, r: R) -> Option<u32> {
        let (seg, dw) = r.at();
        self.base.get(seg)?.checked_add(dw)?.checked_mul(4)
    }

    pub fn name_at(&self, byte: u32) -> Option<&'static str> {
        if let Some(r) = M::ALL.into_iter().find(|&r| self.byte(r) == Some(byte)) {
            return Some(r.name());
        }
        GFX_REGS.into_iter().find(|&r| self.gfx(r) == Some(byte)).map(R::name)
    }

    pub fn readable(&self) -> Vec<u32> {
        let mut v: Vec<u32> = M::ALL.into_iter().filter_map(|r| self.byte(r)).collect();
        v.extend(GFX_REGS.into_iter().filter_map(|r| self.gfx(r)));
        v.extend(self.aper_en);
        v
    }

    /// Never a display register; CP_MES_GP3_LO is read only (the firmware's version), and of the GFX registers only
    /// GRBM_GFX_CNTL, SCRATCH_REG0, CP_MAX_CONTEXT and CP_DEVICE_ID (cp_gfx_start).
    pub fn writable(&self) -> Vec<u32> {
        let mut v: Vec<u32> = M::ALL.into_iter().filter(|&r| !r.read_only()).filter_map(|r| self.byte(r)).collect();
        v.extend(GFX_REGS[..4].iter().filter_map(|&r| self.gfx(r)));
        v.extend(self.aper_en);
        v
    }

    /// The doorbells the stage rings (dword indexes of BAR2's first page).
    pub fn doorbells(&self) -> [u32; 4] {
        [mes11::KIQ_DOORBELL_DW, mes11::SCHED_DOORBELL_DW, mes11::COMPUTE_DOORBELL_DW, gfx11::DOORBELL_DW]
    }
}

/// The buffer of the stage (kept for the boot: MES reads its MQDs and rings while it runs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MesBo {
    pub handle: u32,
    pub offset: u64,
    pub size: u64,
}

#[derive(Clone, Copy)]
pub struct MesInputs<'a> {
    pub addrs: MesAddrs,
    pub carve: Carveout,
    /// The MES API's wait per frame (Linux 2100 ms) and the rings' (Linux 100 ms).
    pub api_us: u64,
    pub timeout_us: u64,
    /// The BO of an earlier run in this boot (reused).
    pub bo: Option<MesBo>,
    /// The MES microcode starts of the scheduler and the KIQ pipe (the firmware headers' uc_start_addr).
    pub uc_start: [Option<u64>; 2],
    pub masks: HwMasks,
    /// `until=gfx_mes`: also a compute ring and GFX ring 0 through MES (`mesq`).
    pub queues: bool,
    /// Called once before the first write: the live side arms its Quiesce halt.
    pub arm: Option<&'a dyn Fn(MesAddrs)>,
    /// MES left running by an earlier run of this boot: its BO's MC base and whether legacy queues are mapped
    /// (`pre_stop` unmaps them before the GFX stage halts the CP).
    pub prev: Option<(Option<u64>, bool)>,
}

impl MesInputs<'_> {
    pub fn new(d: &Discovery, carve: Carveout, uc_start: [Option<u64>; 2]) -> Option<MesInputs<'static>> {
        Some(MesInputs {
            addrs: MesAddrs::from_discovery(d)?,
            carve,
            api_us: mes11::API_TIMEOUT_US,
            timeout_us: gfx11::LINUX_TIMEOUT_US,
            bo: None,
            uc_start,
            masks: HwMasks::LINUX_780M,
            queues: false,
            arm: None,
            prev: None,
        })
    }
}

/// Where everything lies (MC addresses through GFXHUB's system aperture).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub bo_mc: u64,
    pub sched_mqd: u64,
    pub kiq_mqd: u64,
    pub gfx_mqd: u64,
    pub compute_mqd: u64,
    pub sched_eop: u64,
    pub kiq_eop: u64,
    pub compute_eop: u64,
    pub kiq_ring: u64,
    pub sched_ring: u64,
    pub compute_ring: u64,
    pub ib: u64,
    pub cleaner: u64,
    pub kiq_rptr: u64,
    pub kiq_wptr: u64,
    pub sched_rptr: u64,
    pub sched_wptr: u64,
    pub mes_fence: u64,
    pub api_status: u64,
    pub sch_ctx: u64,
    pub query_fence: u64,
    pub comp_rptr: u64,
    pub comp_wptr: u64,
    pub comp_fence_ib: u64,
    pub comp_fence64: u64,
    /// `COMPUTE_UNMAPPED`, `COMPUTE_STUCK`, else mapped.
    pub compute_state: u64,
    pub misc_buf: u64,
    pub comp_ib_target: u64,
}

impl Layout {
    /// The writeback slots from GR-2's pool (32 bytes each: every slot 8-aligned for a 64-bit value).
    pub fn new(bo_mc: u64) -> Option<Layout> {
        let mut wb = WbPool::new(((lay::TARGET - lay::WB) / 32) as u32);
        let wbase = bo_mc + lay::WB;
        let mut slot = || wb.get().ok().map(|s| s.gpu_addr(wbase));
        let t = bo_mc + lay::TARGET;
        Some(Layout {
            bo_mc,
            sched_mqd: bo_mc + lay::SCHED_MQD,
            kiq_mqd: bo_mc + lay::KIQ_MQD,
            gfx_mqd: bo_mc + lay::GFX_MQD,
            compute_mqd: bo_mc + lay::COMPUTE_MQD,
            sched_eop: bo_mc + lay::SCHED_EOP,
            kiq_eop: bo_mc + lay::KIQ_EOP,
            compute_eop: bo_mc + lay::COMPUTE_EOP,
            kiq_ring: bo_mc + lay::KIQ_RING,
            sched_ring: bo_mc + lay::SCHED_RING,
            compute_ring: bo_mc + lay::COMPUTE_RING,
            ib: bo_mc + lay::IB,
            cleaner: bo_mc + lay::CLEANER,
            kiq_rptr: slot()?,
            kiq_wptr: slot()?,
            sched_rptr: slot()?,
            sched_wptr: slot()?,
            mes_fence: slot()?,
            api_status: slot()?,
            sch_ctx: slot()?,
            query_fence: slot()?,
            comp_rptr: slot()?,
            comp_wptr: slot()?,
            comp_fence_ib: slot()?,
            comp_fence64: slot()?,
            compute_state: slot()?,
            misc_buf: t,
            comp_ib_target: t + 0x40,
        })
    }

    pub fn kiq_prop(&self) -> Prop {
        Prop { mqd: self.kiq_mqd, ring: self.kiq_ring, ring_bytes: KIQ_RING_BYTES, rptr: self.kiq_rptr, wptr: self.kiq_wptr, eop: self.kiq_eop, doorbell_dw: Some(mes11::KIQ_DOORBELL_DW), active: true }
    }

    pub fn sched_prop(&self) -> Prop {
        Prop { mqd: self.sched_mqd, ring: self.sched_ring, ring_bytes: SCHED_RING_BYTES, rptr: self.sched_rptr, wptr: self.sched_wptr, eop: self.sched_eop, doorbell_dw: Some(mes11::SCHED_DOORBELL_DW), active: true }
    }

    pub fn compute_prop(&self) -> Prop {
        Prop { mqd: self.compute_mqd, ring: self.compute_ring, ring_bytes: COMPUTE_RING_BYTES, rptr: self.comp_rptr, wptr: self.comp_wptr, eop: self.compute_eop, doorbell_dw: Some(mes11::COMPUTE_DOORBELL_DW), active: false }
    }

    /// GFX ring 0 lies in the GFX stage's BO (`gfxring::Layout`); its MQD in this one.
    pub fn gfx_prop(&self, g: &gfxring::Layout) -> Prop {
        Prop { mqd: self.gfx_mqd, ring: g.ring, ring_bytes: gfx11::RING_DW * 4, rptr: g.rptr, wptr: g.wptr, eop: 0, doorbell_dw: Some(gfx11::DOORBELL_DW), active: true }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MesError {
    NoAddress,
    /// CP_MES_CNTL reads all ones: GC is gated (nothing written).
    GcOff { byte: u32 },
    Read { byte: u32 },
    Write { byte: u32 },
    Bo(BoError),
    BoTooSmall { have: u64, need: u64 },
    Mem { mc: u64 },
    Ring(RingError),
    /// The MES firmware header of this pipe was not read (no uc start address): nothing written.
    NoFirmware { pipe: u32 },
    /// The CP is halted (ME/PFP or MEC ME1): MES needs Linux's CP unhalt first (the GFX stage's `mes` order).
    CpNotRunning { me: u32, mec: u32 },
    /// `until=gfx_mes` without the GFX stage's buffer (its ring 0 is GFX ring 0's memory).
    NoGfxRing,
    /// After the unhalt CP_MES_CNTL does not read both pipes active and running.
    NotStarted { cntl: Option<u32> },
    /// CP_MES_GP3_LO read 0 (or not at all): the pipe's firmware did not start.
    NoVersion { pipe: u32, read: Option<u32> },
    /// The scheduler firmware is older than 0x47: Linux maps the legacy queues through the KIQ then (not here).
    TooOld { version: u32 },
    /// A stale HQD did not go inactive after DEQUEUE_REQUEST.
    Dequeue { pipe: u32 },
    /// A KIQ submission's result never came (MAP_QUEUES with its ring test, SET_RESOURCES).
    KiqTimeout { what: &'static str, scratch: Option<u32> },
    /// A MES API frame's fence or status never came (Linux: "MES failed to respond to msg").
    ApiTimeout { what: &'static str, fence: Option<u64>, status: Option<u64> },
    /// The self test's register did not read the value MES wrote, or MES's copy not the CPU's value.
    SelfTest { what: &'static str, got: Option<u32>, want: u32 },
    /// A ring's rptr writeback or result never came (`mesq` rungs).
    Timeout { rung: &'static str, rptr: Option<u64>, want: u64 },
    /// The rung completed but its memory did not read as expected.
    Result { rung: &'static str },
    /// The doorbell write was refused (no BAR2, or an index this owner may not ring).
    NoDoorbell,
}

impl MesError {
    pub fn errno(&self) -> i32 {
        match self {
            MesError::NoAddress | MesError::GcOff { .. } | MesError::CpNotRunning { .. } | MesError::NoGfxRing => 19,
            MesError::Bo(_) | MesError::BoTooSmall { .. } => 12,
            MesError::NoFirmware { .. } => 2,
            MesError::TooOld { .. } => 95,
            MesError::KiqTimeout { .. } | MesError::ApiTimeout { .. } | MesError::Timeout { .. } | MesError::Dequeue { .. } => 110,
            _ => 5,
        }
    }

    /// What the desk does next.
    pub fn advice(&self) -> &'static str {
        match self {
            MesError::CpNotRunning { .. } => ": the CP must run first (until=mes runs the GFX stage's Linux unhalt before MES; a CP halted by until=stop needs a new until=mes)",
            MesError::NoFirmware { .. } => ": the MES firmware headers were not read in this run (until=mes loads them from the stick's firmware directory)",
            MesError::NotStarted { .. } | MesError::NoVersion { .. } => ": the MES pipe never started its firmware: was CP_MES1 (the KIQ) in the PSP load list (firmware line)? MES halted again; report the CP_MES_CNTL and GP3 values",
            MesError::TooOld { .. } => ": this MES firmware predates mapping legacy queues through MES (Linux maps them through the KIQ then): not supported, MES halted again",
            MesError::KiqTimeout { .. } => ": the KIQ on MES pipe 1 never ran its ring (MAP_QUEUES or the scratch write): MES halted again; check the KIQ HQD lines",
            MesError::ApiTimeout { .. } => ": MES did not answer the frame (wrong SET_HW_RESOURCES masks hang it silently, roadmap wave 27): MES halted again; send the answer whole",
            MesError::SelfTest { .. } => ": MES answered but the register did not move: MES halted again",
            MesError::Dequeue { .. } => ": a stale HQD left by an earlier run did not dequeue: reboot before the next MES run",
            MesError::Timeout { .. } => ": the MES-mapped queue never fetched its ring: MES and the CP halted again",
            MesError::NoDoorbell => ": the doorbell BAR or the aperture is missing",
            _ => "",
        }
    }
}

/// One step of the stage (also a check).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub name: &'static str,
    pub pass: bool,
    pub us: u64,
    pub detail: String,
}

/// A rung of `mesq` (ring, IB and fence tests on a MES-mapped queue).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rung {
    pub name: &'static str,
    pub pass: bool,
    pub done: bool,
    pub wptr: u64,
    pub rptr: Option<u64>,
    pub fence: Option<u64>,
    pub polls: u32,
    pub us: u64,
    pub detail: String,
}

/// A legacy queue mapped through MES: (name, queue type, pipe, queue, doorbell).
pub type Mapped = (&'static str, u32, u32, u32, u32);

/// The legacy queues `until=gfx_mes` maps, in the order `until=stop` unmaps them (Linux: kcq, then kgq).
pub const LEGACY: [Mapped; 2] = [
    ("compute ring", mesapi::qtype::COMPUTE, mes11::COMPUTE_PIPE, mes11::COMPUTE_QUEUE, mes11::COMPUTE_DOORBELL_DW),
    ("GFX ring 0", mesapi::qtype::GFX, mes11::GFX_PIPE, mes11::GFX_QUEUE, gfx11::DOORBELL_DW),
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MesReport {
    pub layout: Option<Layout>,
    pub bo: Option<MesBo>,
    pub reused_bo: bool,
    pub queues: bool,
    /// MES and the CP as found (before anything was written).
    pub before: Vec<(&'static str, Option<u32>)>,
    /// CP_MES_CNTL found running (an earlier run, or Linux): dequeued and halted first.
    pub halted_first: Option<u32>,
    /// A pipe's HQD found active before its queue was set up: dequeued first (pipe, CP_HQD_ACTIVE after).
    pub stale: Vec<(u32, Option<u32>)>,
    pub uc_start: [Option<u64>; 2],
    /// CP_MES_CNTL right after the unhalt and 500 us.
    pub cntl_started: Option<u32>,
    /// CP_MES_GP3_LO of the scheduler and the KIQ pipe.
    pub versions: (Option<u32>, Option<u32>),
    /// RLC_CP_SCHEDULERS as found and written (kiq_setting).
    pub rlc: Option<(u32, u32)>,
    /// The owner's write count around the MES start (`regdiff` phase mes).
    pub marks: Vec<(&'static str, u64, u64)>,
    pub steps: Vec<Step>,
    pub rungs: Vec<Rung>,
    /// The queues mapped through MES: (name, queue type, pipe, queue, doorbell).
    pub mapped: Vec<Mapped>,
    /// After a failure: the unmap of the mapped queues (lines for the answer).
    pub unmap: Vec<String>,
    /// Mapped queues MES did not unmap (it stopped answering): their HQDs stay active with no firmware owning them.
    pub stranded: Vec<&'static str>,
    /// MES API frames sent: (frame, sequence number).
    pub frames: Vec<(&'static str, u64)>,
    pub doorbells: Vec<(u32, u64, bool)>,
    pub aper: (Option<u32>, Option<u32>),
    pub fault_before: Option<FaultRead>,
    pub fault_after: Option<FaultRead>,
    pub after: Vec<(&'static str, Option<u32>)>,
    /// MES stays running (Linux's state); otherwise dequeued and halted again.
    pub left_running: bool,
    /// The halt after a failure failed too: MES may still run.
    pub halt_failed: Option<MesError>,
    pub error: Option<MesError>,
}

impl MesReport {
    pub fn passed(&self) -> bool {
        self.error.is_none() && checks(self).iter().all(|c| c.pass)
    }
    pub fn aper_found(&self) -> Option<u32> {
        self.aper.0
    }
    /// Whether the stage began writing (MES started or about to).
    fn wrote(&self) -> bool {
        self.layout.is_some() && !matches!(self.error, Some(MesError::GcOff { .. } | MesError::NoAddress | MesError::Read { .. } | MesError::NoFirmware { .. } | MesError::CpNotRunning { .. } | MesError::NoGfxRing | MesError::Bo(_) | MesError::BoTooSmall { .. }))
    }
}

// ---- register access ----

/// Register access: the owner (logged) or a recorder (the Linux plan).
pub(crate) trait Regs {
    fn rd(&mut self, byte: u32) -> Option<u32>;
    fn wr(&mut self, byte: u32, value: u32) -> bool;
    fn delay(&mut self, us: u64);
}

impl<H: Hw> Regs for Bringup<Watch<H>> {
    fn rd(&mut self, byte: u32) -> Option<u32> {
        self.reg_read(byte)
    }
    fn wr(&mut self, byte: u32, value: u32) -> bool {
        matches!(self.write(byte, value).outcome, WriteOutcome::Ok | WriteOutcome::Mismatch)
    }
    fn delay(&mut self, us: u64) {
        self.delay_us(us);
    }
}

/// Raw BAR5 (the Quiesce halt): no log, no wait.
struct Raw<'a>(&'a mut dyn Hw);

impl Regs for Raw<'_> {
    fn rd(&mut self, byte: u32) -> Option<u32> {
        self.0.reg_read(byte)
    }
    fn wr(&mut self, byte: u32, value: u32) -> bool {
        self.0.reg_write(byte, value)
    }
    fn delay(&mut self, _us: u64) {}
}

/// Answers reads from what was found (and the capture's cold values), records the writes (`linux_sequence`).
struct Rec {
    vals: Vec<(u32, u32)>,
    writes: Vec<(u32, u32)>,
}

impl Regs for Rec {
    fn rd(&mut self, byte: u32) -> Option<u32> {
        Some(self.vals.iter().rev().find(|x| x.0 == byte).map_or(0, |x| x.1))
    }
    fn wr(&mut self, byte: u32, value: u32) -> bool {
        self.writes.push((byte, value));
        self.vals.push((byte, value));
        true
    }
    fn delay(&mut self, _us: u64) {}
}

pub(crate) fn rd(o: &mut dyn Regs, a: &MesAddrs, r: M) -> Result<u32, MesError> {
    let b = a.byte(r).ok_or(MesError::NoAddress)?;
    match o.rd(b) {
        Some(u32::MAX) if r == M::CpMesCntl => Err(MesError::GcOff { byte: b }),
        Some(v) => Ok(v),
        None => Err(MesError::Read { byte: b }),
    }
}

pub(crate) fn wr(o: &mut dyn Regs, a: &MesAddrs, r: M, v: u32) -> Result<(), MesError> {
    let b = a.byte(r).ok_or(MesError::NoAddress)?;
    if o.wr(b, v) { Ok(()) } else { Err(MesError::Write { byte: b }) }
}

pub(crate) fn rdg(o: &mut dyn Regs, a: &MesAddrs, r: R) -> Result<u32, MesError> {
    let b = a.gfx(r).ok_or(MesError::NoAddress)?;
    o.rd(b).ok_or(MesError::Read { byte: b })
}

pub(crate) fn wrg(o: &mut dyn Regs, a: &MesAddrs, r: R, v: u32) -> Result<(), MesError> {
    let b = a.gfx(r).ok_or(MesError::NoAddress)?;
    if o.wr(b, v) { Ok(()) } else { Err(MesError::Write { byte: b }) }
}

/// soc21_grbm_select: GRBM_GFX_CNTL written from 0.
pub(crate) fn select(o: &mut dyn Regs, a: &MesAddrs, me: u32, pipe: u32, queue: u32) -> Result<(), MesError> {
    wrg(o, a, R::GrbmGfxCntl, mes11::grbm_select(me, pipe, queue, 0))
}

/// mes_v11_0_enable(true) (7.1 :949): pipe resets, the program counters per pipe, both pipes active, 500 us.
/// Returns CP_MES_CNTL as found.
fn enable_writes(o: &mut dyn Regs, a: &MesAddrs, uc: [u64; 2]) -> Result<u32, MesError> {
    let v = rd(o, a, M::CpMesCntl)?;
    wr(o, a, M::CpMesCntl, mes11::cntl_reset(v))?;
    for (pipe, start) in mes11::PIPES.into_iter().zip(uc) {
        select(o, a, mes11::MES_ME, pipe, 0)?;
        let (lo, hi) = mes11::prgrm_cntr(start);
        wr(o, a, M::CpMesPrgrmCntrStart, lo)?;
        wr(o, a, M::CpMesPrgrmCntrStartHi, hi)?;
    }
    select(o, a, 0, 0, 0)?;
    wr(o, a, M::CpMesCntl, mes11::CNTL_RUN)?;
    o.delay(mes11::ENABLE_DELAY_US);
    Ok(v)
}

/// mes_v11_0_get_fw_version: CP_MES_GP3_LO per pipe.
fn versions(o: &mut dyn Regs, a: &MesAddrs) -> Result<(Option<u32>, Option<u32>), MesError> {
    let mut v = [None; 2];
    for (i, pipe) in mes11::PIPES.into_iter().enumerate() {
        select(o, a, mes11::MES_ME, pipe, 0)?;
        v[i] = a.byte(M::CpMesGp3Lo).and_then(|b| o.rd(b));
    }
    select(o, a, 0, 0, 0)?;
    Ok((v[0], v[1]))
}

/// kiq_setting: RLC_CP_SCHEDULERS names ME 3 pipe 1 queue 0. Returns (found, written).
fn kiq_setting(o: &mut dyn Regs, a: &MesAddrs) -> Result<(u32, u32), MesError> {
    let v = rd(o, a, M::RlcCpSchedulers)?;
    let w = mes11::rlc_cp_schedulers(v, mes11::MES_ME, mes11::KIQ_PIPE, 0);
    wr(o, a, M::RlcCpSchedulers, w)?;
    Ok((v, w))
}

/// kiq_dequeue (7.1 :1513) on the selected pipe: DEQUEUE_REQUEST while active (polled, bounded), the doorbell off
/// (HIT acknowledged), the pointers 0. Returns CP_HQD_ACTIVE after.
fn dequeue_selected(o: &mut dyn Regs, a: &MesAddrs, us: u64) -> Result<Option<u32>, MesError> {
    let mut act = rd(o, a, M::CpHqdActive)?;
    if act & f::HQD_ACTIVE != 0 {
        wr(o, a, M::CpHqdDequeueRequest, f::DEQUEUE_REQ)?;
        for _ in 0..us {
            act = rd(o, a, M::CpHqdActive)?;
            if act & f::HQD_ACTIVE == 0 {
                break;
            }
            o.delay(1);
        }
    }
    let db = rd(o, a, M::CpHqdPqDoorbellControl)?;
    wr(o, a, M::CpHqdPqDoorbellControl, (db & !f::DB_EN) | f::DB_HIT)?;
    wr(o, a, M::CpHqdPqDoorbellControl, 0)?;
    wr(o, a, M::CpHqdPqWptrLo, 0)?;
    wr(o, a, M::CpHqdPqWptrHi, 0)?;
    wr(o, a, M::CpHqdPqRptr, 0)?;
    Ok(Some(act))
}

fn dequeue(o: &mut dyn Regs, a: &MesAddrs, pipe: u32, us: u64) -> Result<Option<u32>, MesError> {
    select(o, a, mes11::MES_ME, pipe, 0)?;
    let r = dequeue_selected(o, a, us);
    let back = select(o, a, 0, 0, 0);
    let r = r?;
    back?;
    Ok(r)
}

/// queue_init_register (7.1 :1214) for the KIQ pipe, from its MQD; a stale active HQD (an earlier run) is
/// dequeued first (`stale`). The capture's 15 writes between the selects.
fn kiq_registers(o: &mut dyn Regs, a: &MesAddrs, m: &[u32], us: u64, stale: &mut Vec<(u32, Option<u32>)>) -> Result<(), MesError> {
    use crate::ipmod::mesmqd::c;
    select(o, a, mes11::MES_ME, mes11::KIQ_PIPE, 0)?;
    let res = (|| {
        if rd(o, a, M::CpHqdActive)? & f::HQD_ACTIVE != 0 {
            let after = dequeue_selected(o, a, us)?;
            stale.push((mes11::KIQ_PIPE, after));
            if after.is_some_and(|x| x & f::HQD_ACTIVE != 0) {
                return Err(MesError::Dequeue { pipe: mes11::KIQ_PIPE });
            }
        }
        let v = rd(o, a, M::CpHqdVmid)?;
        wr(o, a, M::CpHqdVmid, v & !f::MQD_VMID_MASK)?;
        let v = rd(o, a, M::CpHqdPqDoorbellControl)?;
        wr(o, a, M::CpHqdPqDoorbellControl, v & !f::DB_EN)?;
        wr(o, a, M::CpMqdBaseAddr, m[c::MQD_BASE_LO])?;
        wr(o, a, M::CpMqdBaseAddrHi, m[c::MQD_BASE_HI])?;
        let _ = rd(o, a, M::CpMqdControl)?;
        wr(o, a, M::CpMqdControl, 0)?;
        wr(o, a, M::CpHqdPqBase, m[c::PQ_BASE_LO])?;
        wr(o, a, M::CpHqdPqBaseHi, m[c::PQ_BASE_HI])?;
        wr(o, a, M::CpHqdPqRptrReportAddr, m[c::RPTR_REPORT_LO])?;
        wr(o, a, M::CpHqdPqRptrReportAddrHi, m[c::RPTR_REPORT_HI])?;
        wr(o, a, M::CpHqdPqControl, m[c::PQ_CONTROL])?;
        wr(o, a, M::CpHqdPqWptrPollAddr, m[c::WPTR_POLL_LO])?;
        wr(o, a, M::CpHqdPqWptrPollAddrHi, m[c::WPTR_POLL_HI])?;
        wr(o, a, M::CpHqdPqDoorbellControl, m[c::DOORBELL_CONTROL])?;
        wr(o, a, M::CpHqdPersistentState, m[c::PERSISTENT_STATE])?;
        wr(o, a, M::CpHqdActive, m[c::HQD_ACTIVE])
    })();
    let back = select(o, a, 0, 0, 0);
    res?;
    back
}

/// The MES start Linux writes (enable, versions, kiq_setting, the KIQ's registers): the stage and the plan.
fn start_plan(o: &mut dyn Regs, a: &MesAddrs, l: &Layout, uc: [u64; 2], us: u64, r: &mut MesReport) -> Result<(), MesError> {
    enable_writes(o, a, uc)?;
    r.cntl_started = a.byte(M::CpMesCntl).and_then(|b| o.rd(b));
    r.versions = versions(o, a)?;
    r.rlc = Some(kiq_setting(o, a)?);
    let m = mesmqd::mes_mqd(&l.kiq_prop());
    kiq_registers(o, a, &m, us, &mut r.stale)
}

/// The stop's and a failure's halt (mes_v11_0_kiq_hw_fini): the scheduler pipe dequeued, the KIQ pipe dequeued
/// (Linux does it under SR-IOV only; here so a later run meets idle HQDs), then MES halted. GC off: nothing.
fn stop_regs(o: &mut dyn Regs, a: &MesAddrs, us: u64) -> Result<(), MesError> {
    let v = rd(o, a, M::CpMesCntl)?;
    let d0 = dequeue(o, a, mes11::SCHED_PIPE, us);
    let d1 = dequeue(o, a, mes11::KIQ_PIPE, us);
    let h = wr(o, a, M::CpMesCntl, mes11::cntl_halt(v));
    h.and(d0.map(|_| ())).and(d1.map(|_| ()))
}

/// The Quiesce path's halt (no log, no allocation, no lock, no wait): CP_MES_CNTL halted, raw, when MES runs.
/// `true` when the write landed or nothing was to halt (GC off reads all ones).
pub fn halt(hw: &mut dyn Hw, a: &MesAddrs) -> bool {
    let mut o = Raw(hw);
    match rd(&mut o, a, M::CpMesCntl) {
        Ok(v) if mes11::cntl_running(v) => wr(&mut o, a, M::CpMesCntl, mes11::cntl_halt(v)).is_ok(),
        Ok(_) | Err(MesError::GcOff { .. }) => true,
        Err(_) => false,
    }
}

/// The halt through the owner (logged): dequeue both pipes, halt.
pub(crate) fn halt_logged<H: Hw>(o: &mut Bringup<Watch<H>>, a: &MesAddrs) -> Result<(), MesError> {
    stop_regs(o, a, mes11::DEQUEUE_TIMEOUT_US)
}

/// Linux's MES start as one write sequence from what the stage found (the plan the Linux diff compares:
/// `regdiff` phase mes, CP_MES_CNTL through the KIQ's CP_HQD_ACTIVE).
pub fn linux_sequence(r: &MesReport, a: &MesAddrs) -> Vec<(u32, u32)> {
    let (Some(l), [Some(u0), Some(u1)]) = (r.layout, r.uc_start) else { return Vec::new() };
    let mut vals: Vec<(u32, u32)> = Vec::new();
    if let (Some(b), Some((found, _))) = (a.byte(M::RlcCpSchedulers), r.rlc) {
        vals.push((b, found));
    }
    if let Some(b) = a.byte(M::CpMesCntl) {
        vals.push((b, r.before.iter().find(|x| x.0 == "CP_MES_CNTL").and_then(|x| x.1).unwrap_or(f::MES_HALT)));
    }
    // The capture's cold CP_MQD_CONTROL (0x100, the register default).
    if let Some(b) = a.byte(M::CpMqdControl) {
        vals.push((b, crate::ipmod::mes11::dflt::CP_MQD_CONTROL));
    }
    let mut rec = Rec { vals, writes: Vec::new() };
    let mut scratch = MesReport::default();
    let _ = start_plan(&mut rec, a, &l, [u0, u1], 0, &mut scratch);
    rec.writes
}

// ---- memory and rings ----

pub(crate) fn rd32(hw: &mut dyn Hw, mc: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    hw.mem_read(mc, &mut b).then(|| u32::from_le_bytes(b))
}

pub(crate) fn rd64(hw: &mut dyn Hw, mc: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    hw.mem_read(mc, &mut b).then(|| u64::from_le_bytes(b))
}

pub(crate) fn dwords(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|d| d.to_le_bytes()).collect()
}

pub(crate) fn mem(hw: &mut dyn Hw, mc: u64, data: &[u8]) -> Result<(), MesError> {
    if hw.mem_write(mc, data) { Ok(()) } else { Err(MesError::Mem { mc }) }
}

/// Zeroes `len` bytes at `mc` in 64 KiB pieces.
pub(crate) fn zero(hw: &mut dyn Hw, mc: u64, len: u64) -> Result<(), MesError> {
    let piece = vec![0u8; 0x1_0000.min(len) as usize];
    let mut at = 0;
    while at < len {
        let n = (len - at).min(piece.len() as u64);
        mem(hw, mc + at, &piece[..n as usize])?;
        at += n;
    }
    Ok(())
}

/// A sink that keeps only the dwords a submission wrote (the KIQ ring is 1 MiB: no shadow of it).
pub(crate) struct Sparse {
    pub size: u64,
    pub out: Vec<(u64, u32)>,
}

impl RingSink for Sparse {
    fn len_dw(&self) -> u64 {
        self.size
    }
    fn put(&mut self, index: u64, dw: u32) {
        self.out.push((index, dw));
    }
}

/// A PM4 queue as the CPU drives it (the KIQ, a MES-mapped compute or GFX ring): packets into its ring, the wptr
/// shadow, a store barrier, the doorbell (MES-mapped queues have no MMIO write pointer).
pub(crate) struct PmQueue {
    pub ring: Ring,
    pub base: u64,
    pub rptr_wb: Option<u64>,
    pub wptr_mc: u64,
    pub doorbell: u32,
    pub rptr_dw: u64,
}

impl PmQueue {
    pub fn new(base: u64, bytes: u64, rptr_wb: Option<u64>, wptr_mc: u64, doorbell: u32) -> Result<PmQueue, MesError> {
        Ok(PmQueue { ring: Ring::new(Kind::Gfx, bytes / 4, 0).map_err(MesError::Ring)?, base, rptr_wb, wptr_mc, doorbell, rptr_dw: 0 })
    }

    /// The ring's pointer slots at 0 (the ring memory was zeroed with the BO).
    pub fn reset(&self, hw: &mut dyn Hw) -> Result<(), MesError> {
        if let Some(r) = self.rptr_wb {
            mem(hw, r, &[0; 8])?;
        }
        mem(hw, self.wptr_mc, &[0; 8])?;
        hw.store_barrier();
        Ok(())
    }

    pub fn submit(&mut self, hw: &mut dyn Hw, ndw: u64, build: impl FnOnce(&mut Ring) -> Result<(), RingError>, doorbells: &mut Vec<(u32, u64, bool)>) -> Result<u64, MesError> {
        self.ring.begin(ndw, self.rptr_dw).map_err(MesError::Ring)?;
        if let Err(e) = build(&mut self.ring) {
            self.ring.undo();
            return Err(MesError::Ring(e));
        }
        let mut sink = Sparse { size: self.ring.size_dw(), out: Vec::new() };
        let new = self.ring.commit(&mut sink).map_err(MesError::Ring)?;
        // Contiguous runs as one write each (a submission wraps at most once).
        let mut i = 0;
        while i < sink.out.len() {
            let start = sink.out[i].0;
            let mut j = i;
            while j + 1 < sink.out.len() && sink.out[j + 1].0 == sink.out[j].0 + 1 {
                j += 1;
            }
            let run: Vec<u32> = sink.out[i..=j].iter().map(|x| x.1).collect();
            mem(hw, self.base + start * 4, &dwords(&run))?;
            i = j + 1;
        }
        hw.store_barrier();
        if !hw.mem_write64(self.wptr_mc, new) {
            return Err(MesError::Mem { mc: self.wptr_mc });
        }
        hw.store_barrier();
        let ok = hw.doorbell_write64(self.doorbell, new);
        doorbells.push((self.doorbell, new, ok));
        klog::event!(Info, "gx", "mes.doorbell", index = self.doorbell, value = klog::Hex(new), accepted = ok);
        if !ok {
            return Err(MesError::NoDoorbell);
        }
        Ok(new)
    }

    /// Polls the rptr writeback (when the queue writes one) up to `wptr` and `done`, at most `us`.
    pub fn wait(&mut self, hw: &mut dyn Hw, us: u64, name: &'static str, wptr: u64, done: &mut dyn FnMut(&mut dyn Hw) -> bool) -> Rung {
        let mask = self.ring.mask();
        let wb = self.rptr_wb;
        let t0 = hw.now_us();
        let mut polls = 0;
        let mut last = None;
        let ok = gmc::poll(hw, us, &mut polls, &mut |h| {
            let at = match wb {
                Some(r) => {
                    last = rd32(h, r).map(u64::from);
                    last.is_some_and(|x| x & mask == wptr & mask)
                }
                None => true,
            };
            at && done(h)
        });
        if ok {
            self.rptr_dw = wptr;
        }
        let el = hw.now_us().saturating_sub(t0);
        if ok {
            klog::event!(Info, "gx", "mes.rung", rung = name, wptr = wptr, polls = polls, us = el);
        } else {
            klog::event!(Error, "gx", "mes.rung_timeout", rung = name, wptr = wptr, rptr = klog::Hex(last.unwrap_or(u64::MAX)), polls = polls, us = el);
        }
        Rung { name, pass: ok, done: ok, wptr, rptr: last, fence: None, polls, us: el, detail: String::new() }
    }
}

/// The scheduler ring (MES API frames, 64 dwords each, no padding: mes_v11_0_ring_funcs nop 0).
pub(crate) struct MesRing {
    pub base: u64,
    pub size_dw: u64,
    pub wptr: u64,
    pub seq: u64,
}

impl MesRing {
    /// submit_pkt_and_poll_completion: the frame with its api_status (fence `api_status`, value 1), then a
    /// QUERY_SCHEDULER_STATUS with the ring's fence (`mes_fence`, the next sequence number), committed (wptr
    /// shadow, doorbell); polled until both land, at most `us`. `Ok((us, polls))`.
    pub fn submit(&mut self, hw: &mut dyn Hw, l: &Layout, mut frame: Frame, us: u64, doorbells: &mut Vec<(u32, u64, bool)>) -> Result<(u64, u32), MesError> {
        let what = frame.name();
        frame.set_status(l.api_status, 1);
        mem(hw, l.api_status, &[0; 16])?;
        self.seq += 1;
        let mut q = Frame::query_status();
        q.set_status(l.mes_fence, self.seq);
        let mut words: Vec<u32> = frame.0.to_vec();
        words.extend_from_slice(&q.0);
        let mask = self.size_dw - 1;
        for (i, chunk) in words.chunks(mesapi::FRAME_DW).enumerate() {
            let at = (self.wptr + (i * mesapi::FRAME_DW) as u64) & mask;
            mem(hw, self.base + at * 4, &dwords(chunk))?;
        }
        self.wptr += words.len() as u64;
        hw.store_barrier();
        if !hw.mem_write64(l.sched_wptr, self.wptr) {
            return Err(MesError::Mem { mc: l.sched_wptr });
        }
        hw.store_barrier();
        let ok = hw.doorbell_write64(mes11::SCHED_DOORBELL_DW, self.wptr);
        doorbells.push((mes11::SCHED_DOORBELL_DW, self.wptr, ok));
        if !ok {
            return Err(MesError::NoDoorbell);
        }
        let seq = self.seq;
        let t0 = hw.now_us();
        let mut polls = 0;
        let (fence_mc, status_mc) = (l.mes_fence, l.api_status);
        let done = gmc::poll(hw, us, &mut polls, &mut |h| rd64(h, fence_mc).is_some_and(|v| v as u32 == seq as u32) && rd64(h, status_mc).is_some_and(|v| v != 0));
        let el = hw.now_us().saturating_sub(t0);
        if done {
            klog::event!(Info, "gx", "mes.api", frame = what, seq = seq, polls = polls, us = el);
            Ok((el, polls))
        } else {
            let (fence, status) = (rd64(hw, fence_mc), rd64(hw, status_mc));
            klog::event!(Error, "gx", "mes.api_timeout", frame = what, seq = seq, fence = klog::Hex(fence.unwrap_or(u64::MAX)), status = klog::Hex(status.unwrap_or(u64::MAX)), us = el; "MES failed to respond to the frame");
            Err(MesError::ApiTimeout { what, fence, status })
        }
    }
}

// ---- the stage ----

/// The stage. Nothing is written before CP_MES_CNTL reads powered, the CP reads running and both firmware starts
/// are known. `gfx`: the GFX stage's layout of this chain (GFX ring 0's memory, `until=gfx_mes`).
pub fn enable<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &MesInputs, hub: &HubAddrs, gfx: Option<&gfxring::Layout>) -> MesReport {
    let mut r = MesReport { queues: inp.queues, uc_start: inp.uc_start, ..MesReport::default() };
    let res = run(owner, bo, inp, hub, gfx, &mut r);
    if let Err(e) = res {
        klog::event!(Error, "gx", "mes.failed", error = format!("{e:?}").as_str(), steps = r.steps.len());
        r.error = Some(e);
    }
    if r.layout.is_some() {
        r.fault_after = Some(gmc::read_fault(owner, hub));
    }
    let all = r.error.is_none() && checks(&r).iter().all(|c| c.pass);
    if !all && r.wrote() {
        if !r.mapped.is_empty() {
            unmap_after_failure(owner, &inp.addrs, &mut r);
        }
        match halt_logged(owner, &inp.addrs) {
            Ok(()) => klog::event!(Warn, "gx", "mes.halted", ok = true; "a step failed: MES dequeued and halted again"),
            Err(e) => {
                klog::event!(Error, "gx", "mes.halt_failed", error = format!("{e:?}").as_str(); "a step failed and the MES halt failed too: until=stop and the Quiesce hook halt it");
                r.halt_failed = Some(e);
            }
        }
    }
    r.left_running = all;
    for (n, b) in [("CP_MES_CNTL", inp.addrs.byte(M::CpMesCntl)), ("RLC_CP_SCHEDULERS", inp.addrs.byte(M::RlcCpSchedulers)), ("SCRATCH_REG0", inp.addrs.gfx(R::ScratchReg0)), ("CP_STAT", inp.addrs.gfx(R::CpStat))] {
        if let Some(b) = b {
            let v = owner.reg_read(b);
            r.after.push((n, v));
        }
    }
    if let Some(b) = inp.addrs.aper_en {
        r.aper.1 = owner.reg_read(b);
    }
    if all {
        klog::event!(Info, "gx", "mes.enabled", queues = inp.queues, steps = r.steps.len(); "MES runs: the KIQ, the scheduler and its self test answered");
    }
    r
}

fn step(r: &mut MesReport, name: &'static str, pass: bool, us: u64, detail: String) -> bool {
    r.steps.push(Step { name, pass, us, detail });
    pass
}

fn run<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &MesInputs, hub: &HubAddrs, gfx: Option<&gfxring::Layout>, r: &mut MesReport) -> Result<(), MesError> {
    let a = inp.addrs;
    // 1. MES and the CP as found (nothing written on a refusal).
    for (n, b) in [("CP_MES_CNTL", a.byte(M::CpMesCntl)), ("RLC_CP_SCHEDULERS", a.byte(M::RlcCpSchedulers)), ("CP_ME_CNTL", a.gfx(R::CpMeCntl)), ("CP_MEC_CNTL", a.gfx(R::CpMecCntl)), ("CP_STAT", a.gfx(R::CpStat))] {
        let v = b.and_then(|b| owner.reg_read(b));
        r.before.push((n, v));
    }
    let cntl = rd(owner, &a, M::CpMesCntl)?;
    let me = rdg(owner, &a, R::CpMeCntl)?;
    let mec = rdg(owner, &a, R::CpMecCntl)?;
    if me == u32::MAX || me & gfx11::f::GFX_HALTS != 0 || mec & gfx11::f::MEC_ME1_HALT != 0 {
        return Err(MesError::CpNotRunning { me, mec });
    }
    if inp.queues && gfx.is_none() {
        return Err(MesError::NoGfxRing);
    }
    let uc = [inp.uc_start[0].ok_or(MesError::NoFirmware { pipe: 0 })?, inp.uc_start[1].ok_or(MesError::NoFirmware { pipe: 1 })?];
    r.aper.0 = a.aper_en.and_then(|b| owner.reg_read(b));
    r.fault_before = Some(gmc::read_fault(owner, hub));
    if let Some(arm) = inp.arm {
        arm(a);
    }
    // 2. The buffer (kept for the boot; an earlier run's reused).
    let b = match inp.bo {
        Some(b) if b.size >= lay::END => {
            r.reused_bo = true;
            b
        }
        Some(b) => return Err(MesError::BoTooSmall { have: b.size, need: lay::END }),
        None => {
            let (handle, range) = bo.alloc(BO_BYTES, PAGE).map_err(MesError::Bo)?;
            MesBo { handle, offset: range.start, size: range.end - range.start }
        }
    };
    r.bo = Some(b);
    if b.size < lay::END {
        return Err(MesError::BoTooSmall { have: b.size, need: lay::END });
    }
    let l = Layout::new(inp.carve.mc_base() + b.offset).ok_or(MesError::NoAddress)?;
    r.layout = Some(l);
    klog::event!(Info, "gx", "mes.bo", mc = klog::Hex(l.bo_mc), reused = r.reused_bo);
    // 3. A running MES (an earlier run, Linux) is dequeued and halted first.
    if mes11::cntl_running(cntl) {
        klog::event!(Warn, "gx", "mes.halt_first", cntl = klog::Hex(u64::from(cntl)); "MES was found running: dequeued and halted before the start");
        halt_logged(owner, &a)?;
        r.halted_first = Some(cntl);
    }
    // 4. Memory: the small part, both MES rings and the compute ring zeroed (no stale packet may look valid);
    //    the MES pipes' MQDs.
    zero(owner, l.bo_mc, lay::SMALL_END)?;
    zero(owner, l.sched_ring, SCHED_RING_BYTES)?;
    zero(owner, l.compute_ring, COMPUTE_RING_BYTES)?;
    zero(owner, l.kiq_ring, KIQ_RING_BYTES)?;
    mem(owner, l.kiq_mqd, &mesmqd::bytes(&mesmqd::mes_mqd(&l.kiq_prop())))?;
    mem(owner, l.sched_mqd, &mesmqd::bytes(&mesmqd::mes_mqd(&l.sched_prop())))?;
    owner.store_barrier();
    // 5. NBIO's doorbell aperture (MES rings are doorbell-driven).
    let ab = a.aper_en.ok_or(MesError::NoDoorbell)?;
    let aper = owner.reg_read(ab).ok_or(MesError::Read { byte: ab })?;
    if aper & 1 == 0 && !Regs::wr(owner, ab, aper | 1) {
        return Err(MesError::Write { byte: ab });
    }
    // 6. Linux's start: enable, versions, kiq_setting, the KIQ's HQD.
    let s0 = owner.log().totals().writes;
    let t0 = owner.now_us();
    let res = start_plan(owner, &a, &l, uc, mes11::DEQUEUE_TIMEOUT_US, r);
    r.marks.push(("mes start", s0, owner.log().totals().writes));
    res?;
    let started = r.cntl_started.is_some_and(|c| c & f::MES_HALT == 0 && c & mes11::CNTL_RUN == mes11::CNTL_RUN);
    let (vs, vk) = r.versions;
    let detail = format!(
        "CP_MES_CNTL {} after the unhalt; uc start {:#x} / {:#x} (CP_MES_PRGRM_CNTR_START {:#x}); CP_MES_GP3_LO scheduler {} KIQ {}",
        hx32(r.cntl_started),
        uc[0],
        uc[1],
        mes11::prgrm_cntr(uc[0]).0,
        hx32(vs),
        hx32(vk)
    );
    let vok = vs.is_some_and(|v| v & mes11::VERSION_MASK != 0) && vk.is_some_and(|v| v & mes11::VERSION_MASK != 0);
    if !step(r, "MES firmware started (both pipes)", started && vok, owner.now_us().saturating_sub(t0), detail) {
        return Err(if !started {
            MesError::NotStarted { cntl: r.cntl_started }
        } else if !vs.is_some_and(|v| v & mes11::VERSION_MASK != 0) {
            MesError::NoVersion { pipe: 0, read: vs }
        } else {
            MesError::NoVersion { pipe: 1, read: vk }
        });
    }
    let sched_v = vs.unwrap_or(0) & mes11::VERSION_MASK;
    if sched_v < mes11::LEGACY_MAP_MIN {
        return Err(MesError::TooOld { version: sched_v });
    }
    // 7. The scheduler's pipe: a stale HQD dequeued first, then its queue mapped by the KIQ + the KIQ ring test.
    select(owner, &a, mes11::MES_ME, mes11::SCHED_PIPE, 0)?;
    let act = rd(owner, &a, M::CpHqdActive);
    select(owner, &a, 0, 0, 0)?;
    if act? & f::HQD_ACTIVE != 0 {
        let after = dequeue(owner, &a, mes11::SCHED_PIPE, mes11::DEQUEUE_TIMEOUT_US)?;
        r.stale.push((mes11::SCHED_PIPE, after));
        if after.is_some_and(|x| x & f::HQD_ACTIVE != 0) {
            return Err(MesError::Dequeue { pipe: mes11::SCHED_PIPE });
        }
    }
    let mut kiq = PmQueue::new(l.kiq_ring, KIQ_RING_BYTES, None, l.kiq_wptr, mes11::KIQ_DOORBELL_DW)?;
    kiq.reset(owner)?;
    let sp = l.sched_prop();
    let map = mesapi::map_queues(mesapi::MAP_ME_MES, mesapi::MAP_ENG_SEL_MES, mes11::SCHED_PIPE, 0, mes11::SCHED_DOORBELL_DW, sp.mqd, sp.wptr);
    kiq_test(owner, &a, &mut kiq, inp.timeout_us, r, "MES KIQ MAP_QUEUES (scheduler queue) and ring test", &map)?;
    // 8. The MES API: hardware resources, then the status query.
    let mut ring = MesRing { base: l.sched_ring, size_dw: SCHED_RING_BYTES / 4, wptr: 0, seq: 0 };
    let res = mesapi::HwRes { masks: inp.masks, agg_doorbells: [0; 5], sch_ctx: l.sch_ctx, query_fence: l.query_fence, gc_base: a.gc_base, mmhub_base: a.mmhub_base, osssys_base: a.osssys_base };
    let m = inp.masks;
    api(owner, &l, &mut ring, inp.api_us, r, "MES SET_HW_RESOURCES", Frame::set_hw_resources(&res), format!(
        "vmid masks mmhub {:#x} gfxhub {:#x}, gfx_hqd {:#x?}, compute_hqd {:#x?}, sdma_hqd {:#x?}, GC base {:#x?}",
        m.vmid_mmhub, m.vmid_gfxhub, m.gfx_hqd, m.compute_hqd, m.sdma_hqd, a.gc_base
    ))?;
    if sched_v >= mes11::HW_RSRC_1_MIN {
        api(owner, &l, &mut ring, inp.api_us, r, "MES SET_HW_RESOURCES_1", Frame::set_hw_resources_1(l.cleaner), format!("enable_mes_info_ctx, cleaner fence MC {:#x} (firmware {sched_v:#x} >= {:#x})", l.cleaner, mes11::HW_RSRC_1_MIN))?;
    }
    api(owner, &l, &mut ring, inp.api_us, r, "MES QUERY_SCHEDULER_STATUS", Frame::query_status(), String::from("query_sched_status"))?;
    // 9. The self test: MES writes SCRATCH_REG0, then copies a CPU-written value of it to memory.
    let sb = a.gfx(R::ScratchReg0).ok_or(MesError::NoAddress)?;
    let sdw = sb / 4;
    api(owner, &l, &mut ring, inp.api_us, r, "MES self test: MISC WRITE_REG", Frame::misc_write_reg(sdw, SELFTEST_WRITE), format!("SCRATCH_REG0 (register {sdw:#x}) <- {SELFTEST_WRITE:#x} by MES"))?;
    let got = owner.reg_read(sb);
    if !step(r, "MES self test: register written by MES", got == Some(SELFTEST_WRITE), 0, format!("SCRATCH_REG0 reads {}", hx32(got))) {
        return Err(MesError::SelfTest { what: "WRITE_REG", got, want: SELFTEST_WRITE });
    }
    wrg(owner, &a, R::ScratchReg0, SELFTEST_READ)?;
    mem(owner, l.misc_buf, &gfx11::POISON.to_le_bytes())?;
    owner.store_barrier();
    api(owner, &l, &mut ring, inp.api_us, r, "MES self test: MISC READ_REG", Frame::misc_read_reg(sdw, l.misc_buf), format!("SCRATCH_REG0 = {SELFTEST_READ:#x} by the CPU, copied by MES to MC {:#x}", l.misc_buf))?;
    let got = rd32(owner, l.misc_buf);
    if !step(r, "MES self test: register copied by MES", got == Some(SELFTEST_READ), 0, format!("MC {:#x} reads {}", l.misc_buf, hx32(got))) {
        return Err(MesError::SelfTest { what: "READ_REG", got, want: SELFTEST_READ });
    }
    // 10. until=gfx_mes: the legacy queues.
    if inp.queues {
        let g = gfx.ok_or(MesError::NoGfxRing)?;
        super::mesq::run(owner, &a, &l, g, &mut kiq, &mut ring, inp, r)?;
    }
    Ok(())
}

/// One KIQ submission (`pkt`) followed by Linux's ring test (WRITE_DATA of 0xDEADBEEF into SCRATCH_REG0,
/// poisoned 0xCAFEDEAD first), as amdgpu_ring_test_helper after kiq_map_queues/kiq_set_resources.
pub(crate) fn kiq_test<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &MesAddrs, kiq: &mut PmQueue, us: u64, r: &mut MesReport, name: &'static str, pkt: &[u32]) -> Result<(), MesError> {
    let sb = a.gfx(R::ScratchReg0).ok_or(MesError::NoAddress)?;
    wrg(owner, a, R::ScratchReg0, gfx11::POISON)?;
    let mut body = Vec::from(pkt);
    mesapi::kiq_wreg(&mut body, sb / 4, gfx11::RING_TEST).map_err(|_| MesError::Ring(RingError::State))?;
    let w = kiq.submit(owner, body.len() as u64, |g| g.write(&body), &mut r.doorbells)?;
    let x = kiq.wait(owner, us, name, w, &mut |hw| hw.reg_read(sb) == Some(gfx11::RING_TEST));
    let got = owner.reg_read(sb);
    let pass = x.pass;
    step(r, name, pass, x.us, format!("{} dwords to the KIQ (wptr {w:#x}, doorbell {:#x}); SCRATCH_REG0 reads {} after {} polls", pkt.len(), kiq.doorbell, hx32(got), x.polls));
    if pass { Ok(()) } else { Err(MesError::KiqTimeout { what: name, scratch: got }) }
}

/// One MES API frame and its check.
pub(crate) fn api<H: Hw>(owner: &mut Bringup<Watch<H>>, l: &Layout, ring: &mut MesRing, us: u64, r: &mut MesReport, name: &'static str, frame: Frame, detail: String) -> Result<(), MesError> {
    let what = frame.name();
    let res = ring.submit(owner, l, frame, us, &mut r.doorbells);
    r.frames.push((what, ring.seq));
    match res {
        Ok((el, polls)) => {
            step(r, name, true, el, format!("{what} answered (sequence {}, {polls} polls): {detail}", ring.seq));
            Ok(())
        }
        Err(e) => {
            let d = match &e {
                MesError::ApiTimeout { fence, status, .. } => format!("{what}: no answer (fence {}, status {}): {detail}", hx(*fence), hx(*status)),
                other => format!("{what}: {other:?}: {detail}"),
            };
            step(r, name, false, 0, d);
            Err(e)
        }
    }
}

// ---- stop ----

/// MES runs now (CP_MES_CNTL: halt clear, a pipe active).
pub fn running(hw: &mut dyn Hw, a: &MesAddrs) -> bool {
    a.byte(M::CpMesCntl).and_then(|b| hw.reg_read(b)).is_some_and(mes11::cntl_running)
}

/// Before `until=mes` again (or after Linux): a running MES is stopped before the GFX stage halts the CP (the GFX
/// stage refuses a running MES): an earlier run's legacy queues unmapped (`prev`), both pipes dequeued, MES
/// halted. `None` when MES does not run.
pub fn pre_stop<H: Hw>(owner: &mut Bringup<Watch<H>>, inp: &MesInputs) -> Option<Stopped> {
    if !running(owner, &inp.addrs) {
        return None;
    }
    let (bo_mc, queues) = inp.prev.unwrap_or((None, false));
    klog::event!(Warn, "gx", "mes.pre_stop", ours = inp.prev.is_some(), queues = queues; "MES runs before the run: stopped before the GFX stage");
    let mut out = stop(owner, &inp.addrs, bo_mc, queues);
    out.lines.insert(0, format!("mes: found running before the run ({}): stopped first", if inp.prev.is_some() { "an earlier run of this boot" } else { "not this boot's: Linux or a kernel before" }));
    Some(out)
}

/// What a stop did: `ok`, its lines for the answer, the mapped queues MES did not unmap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stopped {
    pub ok: bool,
    pub lines: Vec<String>,
    pub stranded: Vec<&'static str>,
}

/// unmap_legacy_queue (REMOVE_QUEUE) of `queues` in order, on the scheduler ring at the position the BO holds
/// (the wptr shadow, the fence slot's sequence number). Stops at the first frame that fails: MES did not answer
/// it, so a frame behind it cannot complete, and each wait costs API_TIMEOUT_US. Returns the queues left mapped.
fn unmap<H: Hw>(owner: &mut Bringup<Watch<H>>, l: &Layout, queues: &[Mapped], lines: &mut Vec<String>) -> Vec<&'static str> {
    let wptr = rd64(owner, l.sched_wptr).unwrap_or(0);
    let seq = rd64(owner, l.mes_fence).unwrap_or(0) & 0xffff_ffff;
    let mut ring = MesRing { base: l.sched_ring, size_dw: SCHED_RING_BYTES / 4, wptr, seq };
    let mut dbs = Vec::new();
    let mut left = Vec::new();
    for &(name, t, pipe, queue, db) in queues {
        if !left.is_empty() {
            lines.push(format!("stop: MES REMOVE_QUEUE (unmap_legacy_queue) of the {name}: not attempted (MES did not answer the one before)"));
            left.push(name);
            continue;
        }
        let res = ring.submit(owner, l, Frame::unmap_legacy(t, pipe, queue, db), mes11::API_TIMEOUT_US, &mut dbs);
        lines.push(format!("stop: MES REMOVE_QUEUE (unmap_legacy_queue) of the {name}: {}", match &res {
            Ok((us, _)) => format!("answered in {us} us"),
            Err(e) => format!("FAILED {e:?}"),
        }));
        if res.is_err() {
            left.push(name);
        }
    }
    if !left.is_empty() {
        klog::event!(Error, "gx", "mes.stranded", queues = left.len(); "MES did not unmap legacy queues: their HQDs stay active");
    }
    left
}

/// A failed run's unmap of the queues MES already mapped (before the halt): tried only while MES runs and still
/// answered its last frame; otherwise every mapped queue is stranded.
fn unmap_after_failure<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &MesAddrs, r: &mut MesReport) {
    let names: Vec<&'static str> = r.mapped.iter().map(|m| m.0).collect();
    let deaf = matches!(r.error, Some(MesError::ApiTimeout { .. }));
    match r.layout {
        Some(l) if !deaf && running(owner, a) => {
            let mapped = r.mapped.clone();
            r.stranded = unmap(owner, &l, &mapped, &mut r.unmap);
        }
        _ => {
            r.unmap.push(format!("stop: unmap of the mapped queues ({}) not attempted: {}", names.join(", "), if deaf { "MES did not answer its last frame" } else { "MES does not run" }));
            klog::event!(Error, "gx", "mes.stranded", queues = names.len(), deaf = deaf; "a step failed with legacy queues mapped and MES cannot unmap them");
            r.stranded = names;
        }
    }
}

/// `until=stop`: the legacy queues unmapped through MES (unmap_legacy_queue, when `queues`), both pipes dequeued,
/// MES halted. The ring position comes from the BO (the wptr shadow and the fence slot), so the stop needs no
/// state of the run.
pub fn stop<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &MesAddrs, bo_mc: Option<u64>, queues: bool) -> Stopped {
    let mut out = Stopped { ok: true, ..Stopped::default() };
    let cntl = a.byte(M::CpMesCntl).and_then(|b| owner.reg_read(b));
    if !cntl.is_some_and(mes11::cntl_running) {
        out.lines.push(format!("stop: MES not running (CP_MES_CNTL {}): nothing to unmap", hx32(cntl)));
    } else if let (true, Some(l)) = (queues, bo_mc.and_then(Layout::new)) {
        // GX-7m3: MES no longer holds a compute ring the VM stage's recovery unmapped: no second REMOVE_QUEUE (its answer
        // is unknown, and a frame that hangs would leave GFX ring 0 unmapped too).
        let state = rd64(owner, l.compute_state);
        let gone = matches!(state, Some(COMPUTE_UNMAPPED | COMPUTE_STUCK));
        if gone {
            out.lines.push(format!("stop: the {}: no REMOVE_QUEUE (the VM stage's recovery unmapped it, MES holds no record of it)", LEGACY[0].0));
        }
        out.stranded = unmap(owner, &l, if gone { &LEGACY[1..] } else { &LEGACY[..] }, &mut out.lines);
        if state == Some(COMPUTE_STUCK) {
            out.stranded.insert(0, LEGACY[0].0);
        }
        out.ok = out.stranded.is_empty();
    }
    let res = halt_logged(owner, a);
    out.ok &= res.is_ok();
    let after = a.byte(M::CpMesCntl).and_then(|b| owner.reg_read(b));
    out.lines.push(format!("stop: MES pipes dequeued and MES halted (CP_MES_CNTL {} -> {}){}", hx32(cntl), hx32(after), match res {
        Ok(()) => String::new(),
        Err(e) => format!(": {e:?}"),
    }));
    if !out.stranded.is_empty() {
        out.lines.push(format!("stop: STRANDED legacy queues ({}): their HQDs stay active with no firmware owning them; reboot before the CP runs again", out.stranded.join(", ")));
    }
    klog::event!(Info, "gx", "mes.stopped", ok = out.ok, queues = queues, stranded = out.stranded.len());
    out
}

// ---- checks and the answer ----

pub struct MesCheck {
    pub name: &'static str,
    pub pass: bool,
    pub why: String,
}

/// The steps a run must pass (in order): MES started, the KIQ, the API, the self test; `mesq`'s for gfx_mes.
pub fn expected(r: &MesReport) -> Vec<&'static str> {
    let mut v = alloc::vec!["MES firmware started (both pipes)", "MES KIQ MAP_QUEUES (scheduler queue) and ring test", "MES SET_HW_RESOURCES"];
    if r.versions.0.is_none_or(|x| x & mes11::VERSION_MASK >= mes11::HW_RSRC_1_MIN) {
        v.push("MES SET_HW_RESOURCES_1");
    }
    v.extend(["MES QUERY_SCHEDULER_STATUS", "MES self test: MISC WRITE_REG", "MES self test: register written by MES", "MES self test: MISC READ_REG", "MES self test: register copied by MES"]);
    if r.queues {
        v.extend(super::mesq::STEPS);
    }
    v
}

pub fn checks(r: &MesReport) -> Vec<MesCheck> {
    let mut v = Vec::new();
    for name in expected(r) {
        match r.steps.iter().find(|s| s.name == name) {
            Some(s) => v.push(MesCheck { name, pass: s.pass, why: format!("{} us; {}", s.us, s.detail) }),
            None => v.push(MesCheck { name, pass: false, why: String::from("not run (an earlier step failed)") }),
        }
    }
    if r.queues {
        for name in super::mesq::RUNGS {
            match r.rungs.iter().find(|x| x.name == name) {
                Some(x) => v.push(MesCheck { name, pass: x.pass, why: format!("wptr {}, rptr writeback {}, {} polls, {} us; {}", x.wptr, hx(x.rptr), x.polls, x.us, x.detail) }),
                None => v.push(MesCheck { name, pass: false, why: String::from("not run (an earlier rung or step failed)") }),
            }
        }
    }
    v.push(MesCheck {
        name: "MES no new VM fault",
        pass: no_new_fault(r),
        why: format!("GFXHUB before: {}; after: {}", r.fault_before.map_or(String::from("unread"), |f| f.text()), r.fault_after.map_or(String::from("unread"), |f| f.text())),
    });
    v
}

fn no_new_fault(r: &MesReport) -> bool {
    match (r.fault_before, r.fault_after) {
        (Some(b), Some(a)) => b.status.is_some() && b.status == a.status && b.addr == a.addr,
        _ => false,
    }
}

pub(crate) fn hx(v: Option<u64>) -> String {
    v.map_or(String::from("unread"), |x| format!("{x:#x}"))
}

pub(crate) fn hx32(v: Option<u32>) -> String {
    v.map_or(String::from("unread"), |x| format!("{x:#x}"))
}

/// The stage's lines for the answer (`mes verdict (...)` last, for the desk's scripts).
pub fn render(r: &MesReport) -> Vec<String> {
    let mut v = Vec::new();
    let what = if r.queues { "gfx_mes" } else { "mes" };
    let found: Vec<String> = r.before.iter().map(|(n, x)| format!("{n} {}", hx32(*x))).collect();
    v.push(format!("mes found: {}", found.join(", ")));
    if let Some(l) = r.layout {
        v.push(format!(
            "mes: buffer MC {:#x}{} (MQDs scheduler {:#x} KIQ {:#x} GFX {:#x} compute {:#x}; rings KIQ {:#x} {} KiB, scheduler {:#x} {} KiB, compute {:#x} {} KiB; fence {:#x}, api status {:#x})",
            l.bo_mc,
            if r.reused_bo { " (an earlier run's, reused)" } else { "" },
            l.sched_mqd,
            l.kiq_mqd,
            l.gfx_mqd,
            l.compute_mqd,
            l.kiq_ring,
            KIQ_RING_BYTES >> 10,
            l.sched_ring,
            SCHED_RING_BYTES >> 10,
            l.compute_ring,
            COMPUTE_RING_BYTES >> 10,
            l.mes_fence,
            l.api_status
        ));
    }
    v.push(format!("mes uc start: scheduler {} KIQ {}", hx(r.uc_start[0]), hx(r.uc_start[1])));
    if let Some(c) = r.halted_first {
        v.push(format!("mes: found running (CP_MES_CNTL {c:#x}): pipes dequeued and MES halted first"));
    }
    for &(p, after) in &r.stale {
        v.push(format!("mes: pipe {p}'s HQD found active (an earlier run): dequeued, CP_HQD_ACTIVE now {}", hx32(after)));
    }
    if let Some(c) = r.cntl_started {
        v.push(format!("mes start: CP_MES_CNTL {c:#x} after the unhalt (want {:#x})", mes11::CNTL_RUN));
    }
    let ver = |x: Option<u32>| x.map_or(String::from("unread"), |x| format!("{x:#010x} (firmware {:#x}, API {:#x})", x & mes11::VERSION_MASK, (x >> mes11::API_VERSION_SHIFT) & mes11::API_VERSION_MASK));
    v.push(format!("mes versions: scheduler {}, KIQ {} (the capture: 0x0102508b, 0x06038110)", ver(r.versions.0), ver(r.versions.1)));
    if let Some((a, b)) = r.rlc {
        v.push(format!("mes kiq_setting: RLC_CP_SCHEDULERS {a:#x} -> {b:#x} (the capture: 0x3038 -> 0x30e8)"));
    }
    for &(what, a, b) in &r.marks {
        v.push(format!("{what}: {} writes", b - a));
    }
    for s in &r.steps {
        v.push(format!("mes {}: {} ({})", s.name, if s.pass { "ok" } else { "FAILED" }, s.detail));
    }
    for &(n, t, p, q, db) in &r.mapped {
        v.push(format!("mes mapped: {n} (queue type {t}, pipe {p}, queue {q}, doorbell {db:#x})"));
    }
    for x in &r.rungs {
        v.push(format!("mes {}: {} ({})", x.name, if x.pass { "ok" } else { "FAILED" }, x.detail));
    }
    v.extend(r.unmap.iter().map(|l| format!("mes {}", l.trim_start_matches("stop: "))));
    if !r.stranded.is_empty() {
        v.push(format!("mes: STRANDED legacy queues ({}): MES did not unmap them, their HQDs stay active; the GFX and MES stages are refused until a reboot", r.stranded.join(", ")));
    }
    for &(i, val, ok) in &r.doorbells {
        v.push(format!("mes doorbell: BAR2 dword {i:#x} <- {val:#x} ({})", if ok { "written" } else { "REFUSED" }));
    }
    let (b, a) = r.aper;
    v.push(format!("mes nbio RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN: before {} after {}", hx32(b), hx32(a)));
    let after: Vec<String> = r.after.iter().map(|(n, x)| format!("{n} {}", hx32(*x))).collect();
    v.push(format!("mes after: {}", after.join(", ")));
    match &r.halt_failed {
        Some(e) => v.push(format!("mes: halt FAILED ({e:?}): MES may still run; until=stop and the Quiesce hook halt it")),
        None => v.push(format!("mes: {}", if r.left_running { "left running (Linux's state; until=stop unmaps, dequeues and halts it, the Quiesce hook halts it)" } else { "halted (pipes dequeued, CP_MES_CNTL halt)" })),
    }
    if let Some(e) = &r.error {
        v.push(format!("mes error: {e:?} (errno {}){}", e.errno(), e.advice()));
    }
    v.push(format!("mes verdict ({what}): {}", if r.passed() { "PASS" } else { "FAIL" }));
    v
}
