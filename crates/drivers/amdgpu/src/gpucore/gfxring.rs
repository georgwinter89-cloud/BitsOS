//! GX-7j: the GFX tail and GFX ring 0 (roadmap wave 26, milestone M5: a RELEASE_MEM fence on GFX ring 0). After
//! the PSP autoload booted GC and the GMC stage programmed GFXHUB, Linux's GFX hw_init goes on with the golden
//! registers, `constants_init` and the RLC resume of the PSP path; then the CP. This stage is the **direct CP_RB0
//! experiment**: GFX ring 0 programmed and fed by the CP itself (Linux 6.6's `cp_gfx_resume` with
//! `amdgpu_async_gfx_ring=0`), MES loaded but halted and never touched. Rungs, each checked by the CPU:
//! 1. Linux's ring test: SET_UCONFIG_REG of 0xDEADBEEF into SCRATCH_REG0 (poisoned 0xCAFEDEAD), after the clear-
//!    state preamble `cp_gfx_start` puts first on the ring;
//! 2. Linux's IB test: an IB with WRITE_DATA of 0xDEADBEEF into a carve-out dword, then a 32-bit fence;
//! 3. the capture's golden GFX IB (the raw-IB harness: NOP + RELEASE_MEM of 0x1122334455667788, its target
//!    patched), then a 32-bit fence;
//! 4. M5: RELEASE_MEM end-of-pipe fence of a 64-bit value that moves both halves;
//! 5. the doorbell: NBIO's aperture and the ring's doorbell on, a WRITE_DATA + fence committed through BAR2.
//!
//! Everything the CP reads or writes (ring, rptr writeback, wptr shadow, IBs, fences, targets, the clear-state
//! buffer the RLC reads) lies in one carve-out BO kept for the boot, reached through GFXHUB's system aperture (as
//! the SDMA stage: no GART needed, the CPU reads it uncached, no snoop question). The ring is Linux's 8 KiB.
//! A failed rung or stage halts the CP again (CP_ME_CNTL ME/PFP/CE halt, CP_MEC_CNTL MEC ME1 halt); after a pass
//! it stays running (Linux's state) until `until=stop` or the Quiesce hook halt it.
//!
//! GX-7j2: on the 780M the GX-7j order (ring 0 programmed and active, then the ME/PFP unhalt, MEC never
//! unhalted) left CP_STAT at CP_BUSY + PFP_BUSY for 100 ms. Linux's only captured unhalt (trace-load lines
//! 41659-41679) unhalts MEC first (CP_MEC_CNTL 0x40000000 -> 0, `cp_compute_enable`), ~60 us later ME/PFP, with
//! the ring at its defaults. The cause is unproven, so the start has three orders (`Variant`): e1a Linux's
//! unhalt and nothing more (then halted again), e1b that unhalt then the ring (CP_RB_ACTIVE last) and the rungs,
//! e1c MEC plus the GX-7j order. Every unhalt records CP_STAT's values with their times.
//!
//! If MES runs (an earlier driver mapped the GFX queue through it) the stage refuses before any write; if the CP
//! never fetches the directly programmed ring, the answer names what is missing (MES, GX-7k).
//!
//! Linux 6.6 (amdgpu/gfx_v11_0.c): init_golden_registers :266 (+ soc15.c:446), constants_init :1694 (setup_rb,
//! get_cu_info, init_compute_vmid :1620, init_gds_vmid :1660), rlc_resume :1973 (init_csb :1757, enable_srm
//! :1819), cp_set_doorbell_range :3572, cp_gfx_resume :3230-3325, cp_gfx_set_doorbell :3205, cp_gfx_start :3118,
//! cp_gfx_enable :2630, ring_test_ring :313, ring_test_ib :354, set_wptr_gfx :5144, emit_ib_gfx :5278,
//! emit_fence :5357. The order of the tail is the 27.10. capture's (Linux 7.1 on the 780M); the CP part has no
//! capture (7.1 maps the GFX queue through MES), so 6.6's direct path decides it.

use super::bringup::{Bringup, Outcome as WriteOutcome};
use super::disc::Discovery;
use super::gmc::{self, FaultRead, HubAddrs};
use super::hwid;
use super::psp::{BoAlloc, Hw};
use super::watch::Watch;
use crate::bo::{BoError, PAGE};
use crate::ipmod::gfx11::{self, R, f};
use crate::ipmod::{mesmqd, nbio7};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::addr::Carveout;
use amdgpu_fmt::fence::{Half, Timeline, read_split};
use amdgpu_fmt::pm4::{self, RegSpace};
use amdgpu_fmt::ring::{Kind, Ring, RingError};
use amdgpu_fmt::wb::WbPool;

/// The BO's layout (carve-out offsets from its start).
pub mod lay {
    pub const RING: u64 = 0;
    pub const WB: u64 = 0x2000;
    pub const TARGET: u64 = 0x3000;
    pub const IB: u64 = 0x4000;
    pub const GOLDEN_IB: u64 = 0x4100;
    pub const CSB: u64 = 0x5000;
    /// GX-7j3: ring 0's MQD (`mesmqd::gfx_mqd`, 2 KiB), what CP_GFX_MQD_BASE_ADDR points at.
    pub const MQD: u64 = 0x6000;
    pub const END: u64 = 0x7000;
}
/// The BO the stage asks for (the live allocator rounds it up to a whole 2 MiB piece).
pub const BO_BYTES: u64 = 0x1_0000;

/// The clear-state buffer of GC 11.0.1 as Linux's `get_csb_buffer` builds it (gfx_v11_0.c:605 from
/// clearstate_gfx11.h, MIT, AMD): PREAMBLE_CNTL begin, CONTEXT_CONTROL, the context registers' defaults,
/// PA_SC_TILE_STEERING_OVERRIDE 0, PREAMBLE_CNTL end, CLEAR_STATE. Taken byte for byte from the 27.10. capture
/// (the first 960 dwords of the debugfs dump of GFX ring 0, where `cp_gfx_start` wrote the same stream), so the
/// buffer is the hardware's truth, not a transcription; RLC_CSIB_LENGTH was 0x3c0 there.
pub const CSB: &[u8] = include_bytes!("gfx11_csb.bin");
pub const CSB_DW: u32 = 960;

pub fn csb() -> Vec<u32> {
    CSB.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// The raw-IB harness's GFX IB on the real 780M (plan/hw capture, bundle/ibtest.out): NOP with two body dwords,
/// RELEASE_MEM (Linux 6.6's fence bits, 64-bit data 0x1122334455667788 to VA 0x1_0000_3000), NOP padding. Words
/// 6-7 (the address) are patched with the stage's target.
pub const GOLDEN_IB: [u32; 16] = [
    0xc001_1000, 0x1b17_5000, 0x1b17_5001, 0xc006_4900, 0x0671_f514, 0x4000_0000, 0x0000_3000, 0x0000_0001,
    0x5566_7788, 0x1122_3344, 0x0000_0000, 0xffff_1000, 0xffff_1000, 0xffff_1000, 0xffff_1000, 0xffff_1000,
];
/// What the capture read back ("RESULT gfx ok fence 0x1122334455667788").
pub const GOLDEN_FENCE: u64 = 0x1122_3344_5566_7788;
/// The 64-bit fence's timeline starts below a 2^32 boundary, so its value changes both halves.
pub const FENCE64_START: u64 = 0x1_ffff_ffff;
pub const DOORBELL_VALUE: u32 = 0xD00D_6F11;

/// The rungs' names (also the checks' names), in order.
pub const RUNGS: [&str; 5] = ["GFX ring test (SCRATCH_REG0)", "GFX IB test (WRITE_DATA)", "GFX golden IB (capture)", "GFX RELEASE_MEM 64-bit fence (M5)", "GFX doorbell"];

/// GX-7j2: the order of the CP start (the desk's unhalt experiments, gx-GX-7j-unhalt-scout.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Variant {
    /// Linux's captured unhalt and nothing more: doorbell ranges, MEC unhalt, ME/PFP unhalt, CP_STAT watched, then
    /// the CP and MEC halted again. No ring register is written, no rung runs.
    E1a,
    /// e1a's unhalt, then ring 0 programmed on the running CP with CP_RB_ACTIVE as its last register, then the
    /// rungs (tests whether an active ring at the unhalt is what kept the PFP busy). The default (`DEFAULT`).
    #[default]
    E1b,
    /// MEC unhalted, ring 0 programmed and active on the halted CP, then the ME/PFP unhalt: linux-6.6's direct
    /// order with amdgpu_async_gfx_ring=0 (cp_resume :4115 doorbell range, kcq_resume :4069 cp_compute_enable,
    /// cp_gfx_resume :3230), i.e. GX-7j's order plus MEC.
    E1c,
    /// GX-7k: Linux 7.1's cp_resume up to amdgpu_mes_kiq_hw_init (doorbell ranges, MEC unhalt, ME/PFP unhalt
    /// with the ring at its defaults, CP_STAT watched), then the CP left running for MES (`until=mes`/`gfx_mes`,
    /// `gpucore::mes`). Not a desk variant of `until=gfx_ring` (not in `ALL`).
    Mes,
}

impl Variant {
    /// The order without a `variant` argument: e1b keeps the one unhalt proven on this chip (Linux's, ring at its
    /// defaults) and still reaches the rungs; the fake passes it with and without an active-ring hang.
    pub const DEFAULT: Variant = Variant::E1b;
    pub const ALL: [Variant; 3] = [Variant::E1a, Variant::E1b, Variant::E1c];

    pub fn name(self) -> &'static str {
        match self {
            Variant::E1a => "e1a",
            Variant::E1b => "e1b",
            Variant::E1c => "e1c",
            Variant::Mes => "mes",
        }
    }

    pub fn parse(s: &str) -> Option<Variant> {
        Variant::ALL.into_iter().find(|v| v.name() == s)
    }

    pub fn what(self) -> &'static str {
        match self {
            Variant::E1a => "Linux's unhalt only (doorbell ranges, MEC unhalt, ME/PFP unhalt, CP_STAT watched), then halted again; no ring register, no rung",
            Variant::E1b => "Linux's unhalt (ring at its defaults), then ring 0 programmed on the running CP with CP_RB_ACTIVE last, then the rungs",
            Variant::E1c => "MEC unhalted, ring 0 programmed and active on the halted CP, then the ME/PFP unhalt (Linux 6.6's direct order), then the rungs",
            Variant::Mes => "Linux 7.1's cp_resume before the MES start: doorbell ranges, MEC unhalt, ME/PFP unhalt (ring at its defaults), the CP left running for MES; no ring register, no rung",
        }
    }

    /// Whether the variant programs ring 0 and runs the rungs.
    pub fn runs_ring(self) -> bool {
        matches!(self, Variant::E1b | Variant::E1c)
    }
}

/// GC's registers of the stage as BAR5 bytes (GC's segment bases) and NBIO's doorbell aperture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GfxAddrs {
    base: [u32; 2],
    /// RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN.
    pub aper_en: Option<u32>,
}

impl GfxAddrs {
    pub fn from_discovery(d: &Discovery) -> Option<GfxAddrs> {
        let base = [d.reg(hwid::GC, 0, 0, 0)?, d.reg(hwid::GC, 0, 1, 0)?];
        let aper_en = d.reg(hwid::NBIF, 0, nbio7::RCC_DOORBELL_APER_EN.0, nbio7::RCC_DOORBELL_APER_EN.1).and_then(|x| x.checked_mul(4));
        Some(GfxAddrs { base, aper_en })
    }

    pub fn byte(&self, r: R) -> Option<u32> {
        let (seg, dw) = r.at();
        self.base.get(seg)?.checked_add(dw)?.checked_mul(4)
    }

    /// The register's dword index (what a PM4 SET_*_REG names).
    pub fn dword(&self, r: R) -> Option<u32> {
        self.byte(r).map(|b| b / 4)
    }

    pub fn name_at(&self, byte: u32) -> Option<&'static str> {
        if let Some(r) = R::all().into_iter().find(|&r| self.byte(r) == Some(byte)) {
            return Some(r.name());
        }
        // GX-7k: MES's and the HQDs' registers (same GC bases; the MES stage's writes in the same answer).
        if let Some(m) = crate::ipmod::mes11::M::ALL.into_iter().find(|&m| {
            let (seg, dw) = m.at();
            self.base.get(seg).and_then(|b| b.checked_add(dw)).and_then(|x| x.checked_mul(4)) == Some(byte)
        }) {
            return Some(m.name());
        }
        (self.aper_en == Some(byte)).then_some("RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN")
    }

    /// Every register the stage reads.
    pub fn readable(&self) -> Vec<u32> {
        let mut v: Vec<u32> = R::all().into_iter().filter_map(|r| self.byte(r)).collect();
        v.extend(self.aper_en);
        v
    }

    /// Every register the stage writes. Never MES's (CP_MES_CNTL: read only here), never a display register;
    /// CP_MEC_CNTL since GX-7j2 (MEC ME1 unhalted before the ME/PFP unhalt, halted again with the CP).
    pub fn writable(&self) -> Vec<u32> {
        let ro = [R::CpMesCntl, R::CpStat, R::GrbmStatus, R::CpRb0Rptr, R::CcGcSaUnitDisable, R::GcUserSaUnitDisable, R::CcRbBackendDisable, R::GcUserRbBackendDisable, R::CcGcShaderArrayConfig, R::GcUserShaderArrayConfig, R::CgtsTccDisable, R::CgtsUserTccDisable, R::TaCntl2];
        let mut v: Vec<u32> = R::all().into_iter().filter(|r| !ro.contains(r)).filter_map(|r| self.byte(r)).collect();
        v.extend(self.aper_en);
        v
    }
}

/// The buffer of the stage (kept for the boot: the CP's ring and writeback and the RLC's clear-state buffer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GfxBo {
    pub handle: u32,
    pub offset: u64,
    pub size: u64,
}

#[derive(Clone, Copy)]
pub struct GfxRingInputs<'a> {
    pub addrs: GfxAddrs,
    pub carve: Carveout,
    pub timeout_us: u64,
    /// The BO of an earlier run in this boot (reused).
    pub bo: Option<GfxBo>,
    /// (shader engines, SAs per engine) from the discovery's GC info; `None`: one engine, the SAs the disable
    /// masks leave.
    pub shape: Option<(u32, u32)>,
    /// Called once, after GC read powered and before the stage's first write: the live side arms its Quiesce halt.
    pub arm: Option<&'a dyn Fn(GfxAddrs)>,
/// GX-7j2: the order of the CP start.
    pub variant: Variant,
}

impl GfxRingInputs<'_> {
    pub fn new(d: &Discovery, carve: Carveout) -> Option<GfxRingInputs<'static>> {
        let shape = d.gc.as_ref().and_then(|g| Some((g.get("gc_num_se")?, g.get("gc_num_sa_per_se")?))).filter(|&(se, sa)| se > 0 && sa > 0);
        Some(GfxRingInputs { addrs: GfxAddrs::from_discovery(d)?, carve, timeout_us: gfx11::LINUX_TIMEOUT_US, bo: None, shape, arm: None, variant: Variant::DEFAULT })
    }
}

/// Where everything lies (MC addresses through GFXHUB's system aperture).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub bo_mc: u64,
    pub ring: u64,
    pub rptr: u64,
    pub wptr: u64,
    pub fence_ib: u64,
    pub fence_golden: u64,
    pub fence64: u64,
    pub fence_db: u64,
    pub ib_target: u64,
    pub golden_target: u64,
    pub db_target: u64,
    pub ib: u64,
    pub golden_ib: u64,
    pub csb: u64,
    pub mqd: u64,
}

impl Layout {
    /// The writeback slots from GR-2's pool (32 bytes each, so every slot is 8-aligned for a 64-bit fence).
    pub fn new(bo_mc: u64) -> Option<Layout> {
        let mut wb = WbPool::new(((lay::TARGET - lay::WB) / 32) as u32);
        let wbase = bo_mc + lay::WB;
        let mut slot = || wb.get().ok().map(|s| s.gpu_addr(wbase));
        let t = bo_mc + lay::TARGET;
        Some(Layout {
            bo_mc,
            ring: bo_mc + lay::RING,
            rptr: slot()?,
            wptr: slot()?,
            fence_ib: slot()?,
            fence_golden: slot()?,
            fence64: slot()?,
            fence_db: slot()?,
            ib_target: t,
            golden_target: t + 0x40,
            db_target: t + 0x80,
            ib: bo_mc + lay::IB,
            golden_ib: bo_mc + lay::GOLDEN_IB,
            csb: bo_mc + lay::CSB,
            mqd: bo_mc + lay::MQD,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GfxError {
    NoAddress,
    /// CP_ME_CNTL reads all ones: GC is gated (nothing written).
    GcOff { byte: u32 },
    Read { byte: u32 },
    Write { byte: u32 },
    Bo(BoError),
    BoTooSmall { have: u64, need: u64 },
    Mem { mc: u64 },
    Ring(RingError),
    /// MES runs (CP_MES_CNTL MES_HALT clear: an earlier driver mapped the queues through it): the direct path
    /// needs it idle. Nothing written.
    MesRunning { mes: u32 },
    /// CP_STAT did not read 0 within Linux's 100 ms after a halt or the unhalt.
    CpBusy { what: &'static str, stat: Option<u32> },
    /// A rung's rptr writeback or result never came.
    Timeout { rung: &'static str, rptr: Option<u64>, want: u64 },
    /// The rung completed but its memory did not read as expected.
    Result { rung: &'static str },
    /// After the unhalt CP_ME_CNTL still reads ME or PFP halted, or CP_RB_ACTIVE is off.
    NotStarted { me: Option<u32>, active: Option<u32> },
    /// After the resume CP_RB0_RPTR / CP_RB0_WPTR hold an earlier run's values.
    StalePointers { rptr: u64, wptr: u64 },
    /// No NBIO aperture address, or the doorbell write was refused (no BAR2).
    NoDoorbell,
}

impl GfxError {
    pub fn errno(&self) -> i32 {
        match self {
            GfxError::Bo(_) | GfxError::BoTooSmall { .. } => 12,
            GfxError::Timeout { .. } | GfxError::CpBusy { .. } => 110,
            GfxError::Mem { .. } => 14,
            GfxError::Ring(e) => e.errno(),
            GfxError::MesRunning { .. } => 16,
            GfxError::NoAddress | GfxError::NoDoorbell => 19,
            _ => 5,
        }
    }

    pub fn advice(&self) -> &'static str {
        match self {
            GfxError::GcOff { .. } => ": GC is not on: run the GFX and GMC stages first (until=gart); nothing was written",
            GfxError::MesRunning { .. } => {
                ": MES runs (an earlier driver, Linux, mapped the queues through it): the direct CP_RB0 path needs MES idle; nothing was written; until=stop halts a MES this boot started (until=mes/gfx_mes), else reboot for a cold start (or use until=mes, which stops a running MES first)"
            }
            GfxError::Timeout { rung: "GFX ring test (SCRATCH_REG0)", .. } => {
                ": the CP unhalted and read idle but never fetched ring 0 programmed directly: with MES halted the CP_RB0 path is not enough on this chip, MES is next (GX-7k: the capture's MES start, trace-load lines 41672-41715; Linux 6.6 maps the GFX queue through the KIQ, gfx_v11_0.c:3701); the CP and MEC are halted again, MES was never touched; report the lines"
            }
            GfxError::Timeout { .. } => ": the CP did not finish the rung within Linux's 100 ms: it is halted again; report the lines",
            GfxError::Result { .. } => ": the rung completed but its result differs (see its line): the CP is halted again; report it",
            GfxError::NotStarted { .. } => ": the CP did not read back unhalted after cp_gfx_enable (or CP_RB_ACTIVE did not read 1): the CP and MEC are halted again",
            GfxError::CpBusy { what: "unhalt", .. } => {
                ": CP_STAT never read idle after the unhalt (Linux's 100 ms; every value is on the 'gfx unhalt CP_STAT' line): the PFP microcode spins without fetching (GC 11.0.1's CP firmware is not RS64, so no RS64 step is missing); the CP and MEC are halted again; run the next variant of the desk's list (e1a, e1b, e1c) and report the lines"
            }
            GfxError::CpBusy { .. } => ": CP_STAT never read idle after the halt (Linux's 100 ms): the halt bits are set; report the 'gfx after' line",
            GfxError::StalePointers { .. } => ": CP_RB0_RPTR / CP_RB0_WPTR still hold an earlier run's values after the restart (see 'gfx restart'): the CP is halted again; report the lines",
            GfxError::NoDoorbell => ": no doorbell (BAR2 not mapped or NBIO unknown): the register path stands",
            _ => "",
        }
    }
}

/// One rung's result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rung {
    pub name: &'static str,
    pub pass: bool,
    /// The rptr writeback reached the write pointer and the rung's own completion (register, fence) came.
    pub done: bool,
    pub doorbell: bool,
    /// The committed write pointer (dwords) and the rptr writeback as read (dwords).
    pub wptr: u64,
    pub rptr: Option<u64>,
    pub fence: Option<u64>,
    pub polls: u32,
    pub us: u64,
    pub detail: String,
}

/// A restart's pointer reset (before the resume): CP_RB0_RPTR, CP_RB0_WPTR and CP_RB_DOORBELL_CONTROL as found
/// and after the reset, and where the ring starts (dwords).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rewind {
    pub found: (u64, u64, u32),
    pub after: (u64, u64, u32),
    pub start: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GfxReport {
    pub layout: Option<Layout>,
    pub bo: Option<GfxBo>,
    pub reused_bo: bool,
    /// The registers as found (before anything was written).
    pub before: Vec<(R, Option<u32>)>,
    /// NBIO's doorbell aperture before the stage and after it.
    pub aper: (Option<u32>, Option<u32>),
    /// The CP was found running (ME or PFP unhalted): halted first.
    pub halted_first: bool,
    /// The golden registers: (register, read, written).
    pub golden: Vec<(R, Option<u32>, u32)>,
    /// (shader engines, SAs per engine, active SA mask) the constants used.
    pub shape: Option<(u32, u32, u32)>,
    /// The owner's write count around the tail and the resume (`regdiff` phases gfx_tail and gfx_ring).
    pub marks: Vec<(&'static str, u64, u64)>,
    pub rewind: Option<Rewind>,
    /// CP_ME_CNTL and CP_STAT read right after the unhalt.
    pub started: Option<(Option<u32>, Option<u32>)>,
    /// GX-7j2: the variant that ran.
    pub variant: Variant,
    /// CP_RB_ACTIVE read 1 before the unhalt (an earlier run): set 0 first (e1a, e1b unhalt an inactive ring).
    pub deactivated: bool,
    /// CP_MEC_CNTL as found and as written by the MEC unhalt.
    pub mec_unhalt: Option<(u32, u32)>,
    /// CP_STAT after the ME/PFP unhalt: every distinct value with its time (us after the CP_ME_CNTL write), until
    /// it read 0 or Linux's 100 ms passed (`None`: a read failed).
    pub unhalt_stat: Vec<(u64, Option<u32>)>,
    /// Changes of CP_STAT not kept in `unhalt_stat` (it keeps the first `UNHALT_KEEP - 1` and the latest).
    pub unhalt_more: u32,
    /// e1b: CP_STAT right after ring 0 was activated on the running CP.
    pub ring_stat: Option<Option<u32>>,
    /// e1a: CP_ME_CNTL, CP_MEC_CNTL and CP_STAT after the stage halted the CP and MEC again (`rehalted`: the halt
    /// writes landed).
    pub rehalt: Option<(Option<u32>, Option<u32>, Option<u32>)>,
    pub rehalted: bool,
    /// The clear-state preamble's write pointer (dwords).
    pub preamble_wptr: Option<u64>,
    pub rungs: Vec<Rung>,
    /// Doorbell writes: (dword index, value, accepted).
    pub doorbells: Vec<(u32, u64, bool)>,
    pub fault_before: Option<FaultRead>,
    pub fault_after: Option<FaultRead>,
    pub after: Vec<(R, Option<u32>)>,
    /// The CP stays running (Linux's state); otherwise halted again.
    pub left_running: bool,
    /// After a failed rung or stage the CP halt itself failed: the CP may still run on the kept buffer.
    pub halt_failed: Option<GfxError>,
    pub error: Option<GfxError>,
}

impl GfxReport {
    pub fn passed(&self) -> bool {
        self.error.is_none() && checks(self).iter().all(|c| c.pass)
    }
    pub fn before_of(&self, r: R) -> Option<u32> {
        self.before.iter().find(|x| x.0 == r).and_then(|x| x.1)
    }
    /// The aperture as found (what a stop puts back).
    pub fn aper_found(&self) -> Option<u32> {
        self.aper.0
    }
    /// This stage left the doorbell aperture on where it found it off.
    pub fn turned_aperture_on(&self) -> bool {
        matches!(self.aper, (Some(b), Some(a)) if b & 1 == 0 && a & 1 != 0)
    }
}

/// The registers the stage reads first.
const BEFORE: [R; 16] = [
    R::CpMeCntl,
    R::CpMecCntl,
    R::CpMesCntl,
    R::CpStat,
    R::GrbmStatus,
    R::CpRb0Cntl,
    R::CpRb0Rptr,
    R::CpRb0Wptr,
    R::CpRbActive,
    R::CpRbDoorbellControl,
    R::CpRbDoorbellRangeLower,
    R::RlcCsibAddrHi,
    R::RlcCsibAddrLo,
    R::RlcCsibLength,
    R::RlcSrmCntl,
    R::ScratchReg0,
];

/// Register access for the tail and the resume: the owner (logged) or a recorder (the Linux plan).
trait Regs {
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

/// Answers reads from what was found, records the writes (`linux_sequence`).
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

fn rd(o: &mut dyn Regs, a: &GfxAddrs, r: R) -> Result<u32, GfxError> {
    let b = a.byte(r).ok_or(GfxError::NoAddress)?;
    match o.rd(b) {
        Some(u32::MAX) if r == R::CpMeCntl => Err(GfxError::GcOff { byte: b }),
        Some(v) => Ok(v),
        None => Err(GfxError::Read { byte: b }),
    }
}

fn wr(o: &mut dyn Regs, a: &GfxAddrs, r: R, v: u32, n: &mut usize) -> Result<(), GfxError> {
    let b = a.byte(r).ok_or(GfxError::NoAddress)?;
    if !o.wr(b, v) {
        return Err(GfxError::Write { byte: b });
    }
    *n += 1;
    Ok(())
}

/// What the tail found and did (for the report).
struct Tail {
    writes: usize,
    golden: Vec<(R, Option<u32>, u32)>,
    shape: (u32, u32, u32),
}

/// The GFX tail of hw_init after gfxhub_enable, in the capture's order: the golden registers, `constants_init`
/// (GRBM read timeout, the SE/SA walk of setup_rb/get_cu_info, SH_MEM per VMID, KFD's VMIDs with the trap
/// enable, GDS cleared), the RLC resume of the PSP path (the clear-state buffer's address and length, SRM on).
/// The CSB itself is in memory already (the caller wrote it).
fn tail(o: &mut dyn Regs, a: &GfxAddrs, l: &Layout, shape: Option<(u32, u32)>) -> Result<Tail, GfxError> {
    let res = tail_writes(o, a, l, shape);
    if let Err(e) = &res {
        // A tail cut off mid-walk leaves one SE/SA or one VMID selected, and the halt and every later stage would
        // land there: broadcast and VMID 0 back, best effort (GC gated: the writes would be lost, none sent).
        if !matches!(e, GfxError::GcOff { .. }) {
            for (r, v) in [(R::GrbmGfxIndex, gfx11::GFX_INDEX_BROADCAST), (R::GrbmGfxCntl, 0)] {
                if let Some(b) = a.byte(r) {
                    let _ = o.wr(b, v);
                }
            }
        }
    }
    res
}

fn tail_writes(o: &mut dyn Regs, a: &GfxAddrs, l: &Layout, shape: Option<(u32, u32)>) -> Result<Tail, GfxError> {
    let mut n = 0;
    let mut golden = Vec::new();
    for g in gfx11::GOLDEN {
        let found = if g.needs_read() { Some(rd(o, a, g.reg)?) } else { None };
        let v = g.apply(found.unwrap_or(0));
        wr(o, a, g.reg, v, &mut n)?;
        golden.push((g.reg, found, v));
    }
    // constants_init
    let v = rd(o, a, R::GrbmCntl)?;
    wr(o, a, R::GrbmCntl, (v & !f::READ_TIMEOUT_MASK) | 0xff, &mut n)?;
    let sa_off = (rd(o, a, R::CcGcSaUnitDisable)? | rd(o, a, R::GcUserSaUnitDisable)?) & f::SA_DISABLE_MASK;
    let sa_active = !(sa_off >> f::SA_DISABLE_SHIFT) & 0xffff;
    let _rb = rd(o, a, R::CcRbBackendDisable)? | rd(o, a, R::GcUserRbBackendDisable)?;
    let (se_n, sa_n) = shape.unwrap_or((1, 16 - (sa_active as u16).leading_zeros()));
    for se in 0..se_n.min(16) {
        for sa in 0..sa_n.min(16) {
            let bit = se * sa_n + sa;
            if bit >= 16 || (sa_active >> bit) & 1 == 0 {
                continue;
            }
            wr(o, a, R::GrbmGfxIndex, gfx11::gfx_index(se, sa), &mut n)?;
            let _ = rd(o, a, R::CcGcShaderArrayConfig)?;
            let _ = rd(o, a, R::GcUserShaderArrayConfig)?;
        }
    }
    wr(o, a, R::GrbmGfxIndex, gfx11::GFX_INDEX_BROADCAST, &mut n)?;
    let _ = (rd(o, a, R::CgtsTccDisable)?, rd(o, a, R::CgtsUserTccDisable)?, rd(o, a, R::TaCntl2)?);
    for vmid in 0..gfx11::FIRST_KFD_VMID {
        wr(o, a, R::GrbmGfxCntl, gfx11::grbm_vmid(vmid), &mut n)?;
        wr(o, a, R::ShMemConfig, gfx11::SH_MEM_CONFIG, &mut n)?;
        if vmid != 0 {
            wr(o, a, R::ShMemBases, gfx11::SH_MEM_BASES_GFX, &mut n)?;
        }
    }
    wr(o, a, R::GrbmGfxCntl, 0, &mut n)?;
    for vmid in gfx11::FIRST_KFD_VMID..gfx11::NUM_VMID {
        wr(o, a, R::GrbmGfxCntl, gfx11::grbm_vmid(vmid), &mut n)?;
        wr(o, a, R::ShMemConfig, gfx11::SH_MEM_CONFIG, &mut n)?;
        wr(o, a, R::ShMemBases, gfx11::SH_MEM_BASES_KFD, &mut n)?;
        let v = rd(o, a, R::SpiGdbgPerVmidCntl)?;
        wr(o, a, R::SpiGdbgPerVmidCntl, v | f::TRAP_EN, &mut n)?;
    }
    wr(o, a, R::GrbmGfxCntl, 0, &mut n)?;
    for vmid in (gfx11::FIRST_KFD_VMID..gfx11::NUM_VMID).chain(1..gfx11::NUM_VMID) {
        for r in [R::GdsBase(vmid), R::GdsSize(vmid), R::GdsGws(vmid), R::GdsOa(vmid)] {
            wr(o, a, r, 0, &mut n)?;
        }
    }
    // rlc_resume, PSP path: init_csb, enable_srm.
    wr(o, a, R::RlcCsibAddrHi, (l.csb >> 32) as u32, &mut n)?;
    wr(o, a, R::RlcCsibAddrLo, l.csb as u32 & 0xffff_fffc, &mut n)?;
    wr(o, a, R::RlcCsibLength, CSB_DW, &mut n)?;
    let v = rd(o, a, R::RlcSrmCntl)?;
    wr(o, a, R::RlcSrmCntl, v | f::SRM_AUTO_INCR_ADDR | f::SRM_ENABLE, &mut n)?;
    Ok(Tail { writes: n, golden, shape: (se_n, sa_n, sa_active) })
}

/// `cp_gfx_resume`'s ring 0 (pipe 0, VMID 0, its doorbell off: the wptr goes through CP_RB0_WPTR first) and
/// `cp_gfx_start`'s register half (no unhalt; the CSB preamble on the ring is the caller's). `start`: the write
/// pointer (dwords) the ring starts at. `active_last` (e1b): CP_RB_ACTIVE after the ring's doorbell writes, the
/// ring's last register, instead of Linux 6.6's place right after CP_RB0_BASE_HI; cp_gfx_start's GRBM_GFX_CNTL,
/// CP_MAX_CONTEXT and CP_DEVICE_ID stay behind it either way (they are no ring registers, and Linux 7.1 writes them
/// on the running CP too, trace-load 41792-41793).
fn ring_regs(o: &mut dyn Regs, a: &GfxAddrs, l: &Layout, start: u64, active_last: bool, n: &mut usize) -> Result<(), GfxError> {
    wr(o, a, R::CpRbWptrDelay, 0, n)?;
    wr(o, a, R::CpRbVmid, 0, n)?;
    let v = rd(o, a, R::GrbmGfxCntl)?;
    wr(o, a, R::GrbmGfxCntl, v & !f::PIPEID_MASK, n)?;
    let cntl = gfx11::rb0_cntl(gfx11::RING_DW);
    wr(o, a, R::CpRb0Cntl, cntl, n)?;
    wr(o, a, R::CpRb0Wptr, start as u32, n)?;
    wr(o, a, R::CpRb0WptrHi, (start >> 32) as u32, n)?;
    wr(o, a, R::CpRb0RptrAddr, l.rptr as u32, n)?;
    wr(o, a, R::CpRb0RptrAddrHi, (l.rptr >> 32) as u32 & f::RPTR_ADDR_HI_MASK, n)?;
    wr(o, a, R::CpRbWptrPollAddrLo, l.wptr as u32, n)?;
    wr(o, a, R::CpRbWptrPollAddrHi, (l.wptr >> 32) as u32, n)?;
    // GX-7j3: the queue's MQD pointer, as gfx_v11_0_gfx_mqd_init's MQD gives it on the MES path (cp_mqd_base_addr,
    // cp_gfx_mqd_control: VMID 0, PRIV_STATE 1, cache policy 0). Linux's direct cp_gfx_resume (amdgpu_async_gfx_ring=0)
    // writes neither; on the 780M its CP then read page 0 of VMID 0 (fault 0x00000d33, client CPG) after the rungs.
    wr(o, a, R::CpGfxMqdBaseAddr, l.mqd as u32 & !3, n)?;
    wr(o, a, R::CpGfxMqdBaseAddrHi, (l.mqd >> 32) as u32, n)?;
    let v = rd(o, a, R::CpGfxMqdControl)?;
    wr(o, a, R::CpGfxMqdControl, gfx11::gfx_mqd_control(v), n)?;
    o.delay(gfx11::RESUME_DELAY_US);
    wr(o, a, R::CpRb0Cntl, cntl, n)?;
    let rb = l.ring >> 8;
    wr(o, a, R::CpRb0Base, rb as u32, n)?;
    wr(o, a, R::CpRb0BaseHi, (rb >> 32) as u32, n)?;
    if !active_last {
        wr(o, a, R::CpRbActive, f::RB_ACTIVE, n)?;
    }
    let v = rd(o, a, R::CpRbDoorbellControl)?;
    wr(o, a, R::CpRbDoorbellControl, gfx11::doorbell_control(v, false), n)?;
    for (r, v) in gfx11::set_doorbell_range() {
        wr(o, a, r, v, n)?;
    }
    if active_last {
        wr(o, a, R::CpRbActive, f::RB_ACTIVE, n)?;
    }
    let v = rd(o, a, R::GrbmGfxCntl)?;
    wr(o, a, R::GrbmGfxCntl, v & !f::PIPEID_MASK, n)?;
    wr(o, a, R::CpMaxContext, gfx11::MAX_HW_CONTEXTS - 1, n)?;
    wr(o, a, R::CpDeviceId, 1, n)?;
    Ok(())
}

/// GX-7j3: the direct ring 0's MQD (`mesmqd::gfx_mqd`, Linux's gfx_v11_0_gfx_mqd_init) for `doorbell`.
pub fn direct_mqd(l: &Layout, doorbell: Option<u32>) -> Vec<u32> {
    mesmqd::gfx_mqd(&mesmqd::Prop { mqd: l.mqd, ring: l.ring, ring_bytes: gfx11::RING_DW * 4, rptr: l.rptr, wptr: l.wptr, eop: 0, doorbell_dw: doorbell, active: true })
}

/// e1a/e1b on a ring an earlier run left active: CP_RB_ACTIVE 0 on the halted CP (before the doorbell ranges), so
/// the unhalt meets the ring as Linux's does. `true` when written.
fn deactivate(o: &mut dyn Regs, a: &GfxAddrs, n: &mut usize) -> Result<bool, GfxError> {
    let v = rd(o, a, R::CpRbActive)?;
    if v == u32::MAX || v & f::RB_ACTIVE == 0 {
        return Ok(false);
    }
    wr(o, a, R::CpRbActive, v & !f::RB_ACTIVE, n)?;
    Ok(true)
}

/// Everything before the ME/PFP unhalt: `cp_set_doorbell_range` (the capture's four writes), MEC ME1 unhalted
/// (`cp_compute_enable(true)`, the capture: 0x40000000 -> 0) and its 50 us, then for e1c ring 0 (Linux 6.6's
/// place, CP_RB_ACTIVE included). Returns CP_MEC_CNTL as found.
fn pre_unhalt(o: &mut dyn Regs, a: &GfxAddrs, l: &Layout, start: u64, v: Variant, n: &mut usize) -> Result<u32, GfxError> {
    for (r, x) in gfx11::doorbell_ranges() {
        wr(o, a, r, x, n)?;
    }
    let mec = rd(o, a, R::CpMecCntl)?;
    wr(o, a, R::CpMecCntl, gfx11::mec_cntl(mec, true), n)?;
    o.delay(gfx11::MEC_DELAY_US);
    if v == Variant::E1c {
        ring_regs(o, a, l, start, false, n)?;
    }
    Ok(mec)
}

/// `cp_gfx_enable(true)`: ME and PFP unhalted (CE as found: the capture's 0x15000000 -> 0x01000000).
fn me_unhalt(o: &mut dyn Regs, a: &GfxAddrs, n: &mut usize) -> Result<(), GfxError> {
    let v = rd(o, a, R::CpMeCntl)?;
    wr(o, a, R::CpMeCntl, gfx11::me_cntl(v, true), n)
}

/// After the unhalt (and CP_STAT idle): e1b's ring 0 on the running CP, CP_RB_ACTIVE last.
fn post_unhalt(o: &mut dyn Regs, a: &GfxAddrs, l: &Layout, start: u64, v: Variant, n: &mut usize) -> Result<(), GfxError> {
    if v == Variant::E1b {
        ring_regs(o, a, l, start, true, n)?;
    }
    Ok(())
}

/// The variant's whole start as one write sequence (the Linux plan of `linux_sequence`; the stage itself polls
/// CP_STAT between the unhalt and `post_unhalt`).
fn start_plan(o: &mut dyn Regs, a: &GfxAddrs, l: &Layout, start: u64, v: Variant) -> Result<usize, GfxError> {
    let mut n = 0;
    if v != Variant::E1c {
        deactivate(o, a, &mut n)?;
    }
    pre_unhalt(o, a, l, start, v, &mut n)?;
    me_unhalt(o, a, &mut n)?;
    post_unhalt(o, a, l, start, v, &mut n)?;
    Ok(n)
}

/// The stop's halt, Linux's `cp_enable(false)` order (gfx_v11_0.c:4160-4163): CE, PFP and ME halted (CP_ME_CNTL
/// bits 24/26/28), then MEC ME1 (CP_MEC_CNTL bit 30, written only when it runs). MEC is halted even when the
/// CP_ME_CNTL write was refused; the first error is returned. GC off (CP_ME_CNTL all ones): nothing written.
/// Logged when `o` is the owner.
fn stop_regs(o: &mut dyn Regs, a: &GfxAddrs) -> Result<(), GfxError> {
    let mut n = 0;
    let me = rd(o, a, R::CpMeCntl).and_then(|v| wr(o, a, R::CpMeCntl, v | f::ALL_HALTS, &mut n));
    if let Err(GfxError::GcOff { .. }) = me {
        return me;
    }
    let mec = rd(o, a, R::CpMecCntl).and_then(|m| if m == u32::MAX || m & f::MEC_ME1_HALT != 0 { Ok(()) } else { wr(o, a, R::CpMecCntl, gfx11::mec_cntl(m, false), &mut n) });
    me.and(mec)
}

/// The Quiesce path's halt (no log, no allocation, no lock, no wait): CP_ME_CNTL's CE/PFP/ME halt bits set and
/// MEC ME1 halted, raw. `true` when the writes landed (GC off reads all ones: nothing to halt, `true`).
pub fn halt(hw: &mut dyn Hw, a: &GfxAddrs) -> bool {
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
    match stop_regs(&mut Raw(hw), a) {
        Ok(()) | Err(GfxError::GcOff { .. }) => true,
        Err(_) => false,
    }
}

/// GX-7m2: whether the CP's ME or PFP runs (CP_ME_CNTL read powered, a halt bit clear): what a passed direct ring
/// (e1b, e1c) leaves. GC off or unread: `false`.
pub fn cp_running(hw: &mut dyn Hw, a: &GfxAddrs) -> bool {
    a.byte(R::CpMeCntl).and_then(|b| hw.reg_read(b)).is_some_and(|v| v != u32::MAX && v & f::GFX_HALTS != f::GFX_HALTS)
}

/// The halt through the owner, `cp_compute_enable`'s 50 us, then CP_STAT polled for idle (Linux's
/// `cp_gfx_enable(false)` wait).
fn halt_logged<H: Hw>(o: &mut Bringup<Watch<H>>, a: &GfxAddrs, us: u64) -> Result<(), GfxError> {
    stop_regs(o, a)?;
    o.delay_us(gfx11::MEC_DELAY_US);
    cp_idle(o, a, us, "halt")
}

/// CP_STAT values one answer line shows: a CP_STAT that flaps between two values for the whole 100 ms would
/// otherwise make a line of thousands; the first ones and the latest say what the CP did.
pub const UNHALT_KEEP: usize = 64;

/// CP_STAT right after the ME/PFP unhalt, polled as Linux's cp_gfx_enable wait (every 1 us, after 100 us every
/// 10 us, Linux's 100 ms), every distinct value kept with its time (us after `t0`, the unhalt write), so a hang
/// shows what the CP passed through (the capture: 0x88028000 ... 0x80008000, 0x0 after ~18 us).
fn unhalt_watch<H: Hw>(o: &mut Bringup<Watch<H>>, a: &GfxAddrs, us: u64, t0: u64, seen: &mut Vec<(u64, Option<u32>)>, more: &mut u32) -> Result<(), GfxError> {
    let b = a.byte(R::CpStat).ok_or(GfxError::NoAddress)?;
    let mut polls = 0u32;
    let mut last = None;
    for _ in 0..us.saturating_add(2) {
        let v = o.reg_read(b);
        let el = o.now_us().saturating_sub(t0);
        if seen.last().is_none_or(|x| x.1 != v) {
            let full = seen.len() >= UNHALT_KEEP;
            match seen.last_mut() {
                Some(l) if full => {
                    *l = (el, v);
                    *more = more.saturating_add(1);
                }
                _ => seen.push((el, v)),
            }
        }
        last = v;
        if v == Some(0) {
            klog::event!(Info, "gx", "gfx.unhalt_idle", us = el, values = seen.len(), polls = polls);
            return Ok(());
        }
        if el >= us {
            break;
        }
        polls = polls.saturating_add(1);
        o.delay_us(if el < 100 { 1 } else { 10 }.min(us - el).max(1));
    }
    klog::event!(Error, "gx", "gfx.cp_busy", what = "unhalt", stat = klog::Hex(u64::from(last.unwrap_or(u32::MAX))), polls = polls, values = seen.len());
    Err(GfxError::CpBusy { what: "unhalt", stat: last })
}

fn cp_idle<H: Hw>(o: &mut Bringup<Watch<H>>, a: &GfxAddrs, us: u64, what: &'static str) -> Result<(), GfxError> {
    let b = a.byte(R::CpStat).ok_or(GfxError::NoAddress)?;
    let mut polls = 0;
    let mut last = None;
    let ok = gmc::poll(o, us, &mut polls, &mut |hw| {
        last = hw.reg_read(b);
        last == Some(0)
    });
    if ok {
        Ok(())
    } else {
        klog::event!(Error, "gx", "gfx.cp_busy", what = what, stat = klog::Hex(u64::from(last.unwrap_or(u32::MAX))), polls = polls);
        Err(GfxError::CpBusy { what, stat: last })
    }
}

/// The ring pointers: CP_RB0_RPTR (dwords, as the CP reports it) and CP_RB0_WPTR (with _HI).
fn ptrs(o: &mut dyn Regs, a: &GfxAddrs) -> Result<(u64, u64), GfxError> {
    let rp = rd(o, a, R::CpRb0Rptr)?;
    let lo = rd(o, a, R::CpRb0Wptr)?;
    let hi = rd(o, a, R::CpRb0WptrHi)?;
    Ok((u64::from(rp), u64::from(hi) << 32 | u64::from(lo)))
}

/// Before a resume of a ring that holds an earlier run's state (pointers not 0, the doorbell on), on the halted
/// CP: the read pointer set to 0 through CP_RB_RPTR_WR under CP_RB0_CNTL.RB_RPTR_WR_ENA, the write pointer 0,
/// the doorbell off, then read back. `None` (nothing written) on a cold ring, so a first start writes exactly the
/// Linux sequence. Linux has no restart of the direct ring; the SDMA restart on 28.10. kept its pointers through
/// a plain resume, so this one resets them explicitly.
fn rewind(o: &mut dyn Regs, a: &GfxAddrs) -> Result<Option<Rewind>, GfxError> {
    let (rp, wp) = ptrs(o, a)?;
    let dc = rd(o, a, R::CpRbDoorbellControl)?;
    if rp == 0 && wp == 0 && dc & f::DOORBELL_EN == 0 {
        return Ok(None);
    }
    let mut n = 0;
    if dc & f::DOORBELL_EN != 0 {
        wr(o, a, R::CpRbDoorbellControl, gfx11::doorbell_control(dc, false), &mut n)?;
    }
    let cntl = rd(o, a, R::CpRb0Cntl)?;
    wr(o, a, R::CpRb0Cntl, cntl | f::RB_RPTR_WR_ENA, &mut n)?;
    wr(o, a, R::CpRbRptrWr, 0, &mut n)?;
    wr(o, a, R::CpRb0Wptr, 0, &mut n)?;
    wr(o, a, R::CpRb0WptrHi, 0, &mut n)?;
    wr(o, a, R::CpRb0Cntl, cntl & !f::RB_RPTR_WR_ENA, &mut n)?;
    let (rp2, wp2) = ptrs(o, a)?;
    let dc2 = rd(o, a, R::CpRbDoorbellControl)?;
    Ok(Some(Rewind { found: (rp, wp, dc), after: (rp2, wp2, dc2), start: 0 }))
}

/// What `until=stop` (or a run that keeps nothing) puts back after the CP is halted: the ring's doorbell off and,
/// when `aperture` names it (no SDMA stage that puts it back itself), NBIO's doorbell aperture to the value this
/// boot's first doorbell stage found. Written only where it differs. `(every write landed, lines)`.
pub fn doorbell_back<H: Hw>(o: &mut Bringup<Watch<H>>, a: &GfxAddrs, aperture: Option<u32>) -> (bool, Vec<String>) {
    let mut lines = Vec::new();
    let mut ok = true;
    let mut n = 0;
    match rd(o, a, R::CpRbDoorbellControl) {
        Ok(u32::MAX) | Err(_) => lines.push(String::from("stop: GFX ring 0's doorbell not read (GC off?): left")),
        Ok(dc) if dc & f::DOORBELL_EN == 0 => lines.push(format!("stop: GFX ring 0's doorbell already off ({dc:#x})")),
        Ok(dc) => {
            let res = wr(o, a, R::CpRbDoorbellControl, gfx11::doorbell_control(dc, false), &mut n);
            ok &= res.is_ok();
            lines.push(format!("stop: GFX ring 0's doorbell off: CP_RB_DOORBELL_CONTROL {dc:#x} -> {}{}", hx32(a.byte(R::CpRbDoorbellControl).and_then(|b| o.reg_read(b))), if res.is_ok() { "" } else { " (refused)" }));
        }
    }
    if let (Some(found), Some(b)) = (aperture, a.aper_en) {
        match o.reg_read(b) {
            Some(cur) if cur == found => lines.push(format!("stop: RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN {cur:#x}, as found")),
            Some(cur) => {
                let landed = Regs::wr(o, b, found);
                ok &= landed;
                lines.push(format!("stop: RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN back to {found:#x} as found (was {cur:#x}){}", if landed { "" } else { ": REFUSED" }));
            }
            None => {
                ok = false;
                lines.push(String::from("stop: RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN not read: left"));
            }
        }
    }
    klog::event!(Info, "gx", "gfx.doorbell_back", ok = ok, aperture = aperture.is_some());
    (ok, lines)
}

/// The tail and the variant's start Linux writes on a cold start in one go (from the registers as the stage found them,
/// the capture's golden reads where the stage read none): what the Linux diff compares (`regdiff` phases
/// gfx_tail and gfx_ring).
pub fn linux_sequence(r: &GfxReport, a: &GfxAddrs) -> Vec<(u32, u32)> {
    let Some(l) = r.layout else { return Vec::new() };
    let mut vals: Vec<(u32, u32)> = r.before.iter().filter_map(|&(x, v)| Some((a.byte(x)?, v?))).collect();
    vals.extend(r.golden.iter().filter_map(|&(x, found, _)| Some((a.byte(x)?, found?))));
    let mut rec = Rec { vals, writes: Vec::new() };
    let shape = r.shape.map(|(se, sa, _)| (se, sa));
    if tail(&mut rec, a, &l, shape).is_ok() {
        let _ = start_plan(&mut rec, a, &l, 0, r.variant);
    }
    rec.writes
}

/// GFX ring 0 as the CPU drives it.
struct Queue<'o, H: Hw> {
    o: &'o mut Bringup<Watch<H>>,
    a: GfxAddrs,
    l: Layout,
    ring: Ring,
    shadow: Vec<u32>,
    rptr_dw: u64,
    doorbell: bool,
    t: u64,
    doorbells: Vec<(u32, u64, bool)>,
}

fn rd32(hw: &mut dyn Hw, mc: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    hw.mem_read(mc, &mut b).then(|| u32::from_le_bytes(b))
}

fn rd64(hw: &mut dyn Hw, mc: u64) -> Option<u64> {
    let mut ok = true;
    let v = read_split(|h| {
        let at = if h == Half::Low { mc } else { mc + 4 };
        rd32(hw, at).unwrap_or_else(|| {
            ok = false;
            0
        })
    });
    ok.then_some(v)
}

fn dwords(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|d| d.to_le_bytes()).collect()
}

impl<H: Hw> Queue<'_, H> {
    fn reset(&mut self, start_dw: u64) -> Result<(), GfxError> {
        self.ring = Ring::new(Kind::Gfx, gfx11::RING_DW, start_dw).map_err(GfxError::Ring)?;
        self.shadow = vec![0; gfx11::RING_DW as usize];
        self.rptr_dw = start_dw;
        self.doorbell = false;
        // The ring zeroed (a stale packet of an earlier run must never look valid), both pointer slots at the start.
        let zero = vec![0u8; (gfx11::RING_DW * 4) as usize];
        self.mem(self.l.ring, &zero)?;
        self.mem(self.l.rptr, &start_dw.to_le_bytes())?;
        self.mem(self.l.wptr, &start_dw.to_le_bytes())?;
        self.o.store_barrier();
        Ok(())
    }

    fn mem(&mut self, mc: u64, data: &[u8]) -> Result<(), GfxError> {
        if self.o.mem_write(mc, data) { Ok(()) } else { Err(GfxError::Mem { mc }) }
    }

    /// Packets into the ring, then the commit: the wptr shadow (the poll address), a store barrier, then the
    /// doorbell or CP_RB0_WPTR/_HI (gfx_v11_0.c:5175-5186). Returns the new write pointer (dwords).
    fn submit(&mut self, ndw: u64, build: impl FnOnce(&mut Ring) -> Result<(), RingError>) -> Result<u64, GfxError> {
        self.ring.begin(ndw, self.rptr_dw).map_err(GfxError::Ring)?;
        if let Err(e) = build(&mut self.ring) {
            self.ring.undo();
            return Err(GfxError::Ring(e));
        }
        let old = self.ring.wptr();
        let new = self.ring.commit(&mut self.shadow).map_err(GfxError::Ring)?;
        let (size, mask) = (self.ring.size_dw(), self.ring.mask());
        let mut p = old;
        while p != new {
            let i = p & mask;
            let n = (new - p).min(size - i);
            let bytes = dwords(&self.shadow[i as usize..(i + n) as usize]);
            self.mem(self.l.ring + i * 4, &bytes)?;
            p += n;
        }
        self.o.store_barrier();
        if !self.o.mem_write64(self.l.wptr, new) {
            return Err(GfxError::Mem { mc: self.l.wptr });
        }
        self.o.store_barrier();
        if self.doorbell {
            let ok = self.o.doorbell_write64(gfx11::DOORBELL_DW, new);
            self.doorbells.push((gfx11::DOORBELL_DW, new, ok));
            klog::event!(Info, "gx", "gfx.doorbell", index = gfx11::DOORBELL_DW, value = klog::Hex(new), accepted = ok);
            if !ok {
                return Err(GfxError::NoDoorbell);
            }
        } else {
            let mut n = 0;
            wr(&mut *self.o, &self.a, R::CpRb0Wptr, new as u32, &mut n)?;
            wr(&mut *self.o, &self.a, R::CpRb0WptrHi, (new >> 32) as u32, &mut n)?;
        }
        Ok(new)
    }

    /// Polls the rptr writeback up to the write pointer (compared within the ring: the CP may report its read
    /// pointer as a ring offset or as a 32-bit counter) and `done`, Linux's 100 ms.
    fn wait(&mut self, name: &'static str, wptr: u64, done: &mut dyn FnMut(&mut dyn Hw) -> bool) -> Rung {
        let mask = self.ring.mask();
        let rptr_mc = self.l.rptr;
        let t0 = self.o.now_us();
        let mut polls = 0;
        let mut last = None;
        let ok = gmc::poll(&mut *self.o, self.t, &mut polls, &mut |hw| {
            last = rd32(hw, rptr_mc).map(u64::from);
            last.is_some_and(|r| r & mask == wptr & mask) && done(hw)
        });
        if ok {
            self.rptr_dw = wptr;
        }
        let us = self.o.now_us().saturating_sub(t0);
        if ok {
            klog::event!(Info, "gx", "gfx.rung", rung = name, done = true, wptr = wptr, polls = polls, us = us);
        } else {
            klog::event!(Error, "gx", "gfx.rung_timeout", rung = name, wptr = wptr, rptr = klog::Hex(last.unwrap_or(u64::MAX)), polls = polls, us = us);
        }
        Rung { name, pass: ok, done: ok, doorbell: self.doorbell, wptr, rptr: last, fence: None, polls, us, detail: String::new() }
    }
}

/// The stage. Nothing is written before CP_ME_CNTL reads powered and MES reads halted.
pub fn enable<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &GfxRingInputs, hub: &HubAddrs) -> GfxReport {
    let mut r = GfxReport::default();
    let res = run(owner, bo, inp, hub, &mut r);
    if let Err(e) = res {
        klog::event!(Error, "gx", "gfx.failed", error = format!("{e:?}").as_str(), rungs = r.rungs.len());
        r.error = Some(e);
    }
    if r.layout.is_some() {
        r.fault_after = Some(gmc::read_fault(owner, hub));
    }
    let complete = match inp.variant {
        v if v.runs_ring() => r.rungs.len() == RUNGS.len() && r.rungs.iter().all(|x| x.pass),
        Variant::Mes => r.started.is_some(),
        _ => r.rehalted,
    };
    let all = r.error.is_none() && complete && no_new_fault(&r);
    let running = all && (inp.variant.runs_ring() || inp.variant == Variant::Mes);
    let wrote = !matches!(r.error, Some(GfxError::GcOff { .. } | GfxError::NoAddress | GfxError::Read { .. } | GfxError::MesRunning { .. })) && r.layout.is_some();
    // e1a halted the CP and MEC itself (its halt writes landed): nothing more to write.
    if !running && wrote && !r.rehalted {
        // A failed rung or stage: the CP halted again (no CP left running on our buffers half-way).
        match stop_regs(owner, &inp.addrs) {
            Ok(()) => klog::event!(Warn, "gx", "gfx.halted", ok = true; "a rung or the stage failed: the CP is halted again"),
            Err(e) => {
                klog::event!(Error, "gx", "gfx.halt_failed", ok = false, error = format!("{e:?}").as_str(); "a rung or the stage failed and the CP halt failed too: the CP may still run (until=stop and the Quiesce hook halt it)");
                r.halt_failed = Some(e);
            }
        }
    }
    r.left_running = running;
    for x in [R::CpMeCntl, R::CpMecCntl, R::CpStat, R::GrbmStatus, R::CpRb0Cntl, R::CpRb0Rptr, R::CpRb0Wptr, R::CpRbActive, R::CpRbDoorbellControl, R::CpMesCntl, R::ScratchReg0] {
        if let Some(b) = inp.addrs.byte(x) {
            let v = owner.reg_read(b);
            r.after.push((x, v));
        }
    }
    if let Some(b) = inp.addrs.aper_en {
        r.aper.1 = owner.reg_read(b);
    }
    if running && inp.variant == Variant::Mes {
        klog::event!(Info, "gx", "gfx.unhalted_for_mes"; "the CP runs (Linux's unhalt): MES starts next");
    } else if running {
        klog::event!(Info, "gx", "gfx.enabled", rungs = r.rungs.len(), variant = inp.variant.name(); "GFX ring 0 runs on the CP directly: every rung passed");
    } else if all {
        klog::event!(Info, "gx", "gfx.unhalt_only", variant = inp.variant.name(); "e1a: the CP unhalted, read idle and is halted again");
    }
    r
}

fn run<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &GfxRingInputs, hub: &HubAddrs, r: &mut GfxReport) -> Result<(), GfxError> {
    let a = inp.addrs;
    // 1. The CP as found (GC gated reads all ones; MES running: refused before any write).
    for x in BEFORE {
        let v = a.byte(x).and_then(|b| owner.reg_read(b));
        r.before.push((x, v));
    }
    r.variant = inp.variant;
    let me0 = rd(owner, &a, R::CpMeCntl)?;
    let mes = rd(owner, &a, R::CpMesCntl)?;
    let mec0 = rd(owner, &a, R::CpMecCntl)?;
    if mes == u32::MAX {
        return Err(GfxError::GcOff { byte: a.byte(R::CpMesCntl).unwrap_or(0) });
    }
    if mes & f::MES_HALT == 0 {
        klog::event!(Warn, "gx", "gfx.mes_running", mes = klog::Hex(u64::from(mes)); "MES runs: the direct CP_RB0 path is refused");
        return Err(GfxError::MesRunning { mes });
    }
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
        Some(b) => return Err(GfxError::BoTooSmall { have: b.size, need: lay::END }),
        None => {
            let (handle, range) = bo.alloc(BO_BYTES, PAGE).map_err(GfxError::Bo)?;
            GfxBo { handle, offset: range.start, size: range.end - range.start }
        }
    };
    r.bo = Some(b);
    if b.size < lay::END {
        return Err(GfxError::BoTooSmall { have: b.size, need: lay::END });
    }
    let l = Layout::new(inp.carve.mc_base() + b.offset).ok_or(GfxError::NoAddress)?;
    r.layout = Some(l);
    klog::event!(Info, "gx", "gfx.bo", mc = klog::Hex(l.bo_mc), reused = r.reused_bo);
    // 3. A running CP or MEC (an earlier run, a kernel before) is halted first.
    if me0 & f::GFX_HALTS != f::GFX_HALTS || (mec0 != u32::MAX && mec0 & f::MEC_ME1_HALT == 0) {
        klog::event!(Warn, "gx", "gfx.halt_first", me = klog::Hex(u64::from(me0)), mec = klog::Hex(u64::from(mec0)); "the CP or MEC was found running: halted before the tail");
        halt_logged(owner, &a, inp.timeout_us)?;
        r.halted_first = true;
    }
    // 4. The clear-state buffer into memory, then the tail (golden, constants, RLC resume). GX-7j3: ring 0's MQD
    // too, the registers the direct path writes as the MES path's MQD holds them (doorbell off until rung 5).
    if !owner.mem_write(l.csb, CSB) {
        return Err(GfxError::Mem { mc: l.csb });
    }
    if !owner.mem_write(l.mqd, &mesmqd::bytes(&direct_mqd(&l, None))) {
        return Err(GfxError::Mem { mc: l.mqd });
    }
    owner.store_barrier();
    let s0 = owner.log().totals().writes;
    let t = tail(owner, &a, &l, inp.shape)?;
    r.marks.push(("gfx tail", s0, owner.log().totals().writes));
    r.golden = t.golden;
    r.shape = Some(t.shape);
    klog::event!(Info, "gx", "gfx.tail", writes = t.writes, se = t.shape.0, sa = t.shape.1, sa_active = klog::Hex(u64::from(t.shape.2)));
    let mut q = Queue { o: owner, a, l, ring: Ring::new(Kind::Gfx, gfx11::RING_DW, 0).map_err(GfxError::Ring)?, shadow: Vec::new(), rptr_dw: 0, doorbell: false, t: inp.timeout_us, doorbells: Vec::new() };
    let res = start(&mut q, r, inp.variant).and_then(|()| if inp.variant.runs_ring() { rungs(&mut q, r) } else { Ok(()) });
    r.doorbells = core::mem::take(&mut q.doorbells);
    res
}

/// A (re)start on the halted CP in the variant's order: an earlier run's pointers reset (`rewind`, not for e1a,
/// which writes no ring register), the ring's memory zeroed, the start (`start_plan`'s writes with CP_STAT
/// watched after the unhalt), then proof the ring sits at its start; e1a halts the CP and MEC again instead.
fn start<H: Hw>(q: &mut Queue<'_, H>, r: &mut GfxReport, v: Variant) -> Result<(), GfxError> {
    let mut start = 0;
    if v.runs_ring() && let Some(mut w) = rewind(&mut *q.o, &q.a)? {
        let (rp2, wp2, _) = w.after;
        klog::event!(Info, "gx", "gfx.rewind", rptr = klog::Hex(w.found.0), wptr = klog::Hex(w.found.1), rptr_after = klog::Hex(rp2), wptr_after = klog::Hex(wp2));
        if rp2 != 0 && rp2 % Kind::Gfx.ring_align_dw() == 0 && rp2 < gfx11::RING_DW {
            klog::event!(Warn, "gx", "gfx.ptr_kept", rptr = klog::Hex(rp2); "CP_RB0_RPTR kept its value through the reset: the ring starts there");
            start = rp2;
        } else if rp2 != 0 {
            r.rewind = Some(w);
            return Err(GfxError::StalePointers { rptr: rp2, wptr: wp2 });
        }
        w.start = start;
        r.rewind = Some(w);
    }
    q.reset(start)?;
    let mut n = 0;
    if v != Variant::E1c && deactivate(&mut *q.o, &q.a, &mut n)? {
        r.deactivated = true;
        klog::event!(Warn, "gx", "gfx.deactivated", variant = v.name(); "CP_RB_ACTIVE was 1 (an earlier run): set 0 before the unhalt");
    }
    let s0 = q.o.log().totals().writes;
    let res = unhalt(q, r, v, start, &mut n);
    r.marks.push(("gfx resume", s0, q.o.log().totals().writes));
    res?;
    klog::event!(Info, "gx", "gfx.resumed", writes = n, start = start, variant = v.name());
    match v {
        Variant::E1a => return rehalt(q, r),
        Variant::Mes => return Ok(()),
        _ => {}
    }
    let me = q.a.byte(R::CpMeCntl).and_then(|b| q.o.reg_read(b));
    let active = q.a.byte(R::CpRbActive).and_then(|b| q.o.reg_read(b));
    if v == Variant::E1b {
        r.ring_stat = Some(q.a.byte(R::CpStat).and_then(|b| q.o.reg_read(b)));
    }
    if me.is_none_or(|x| x == u32::MAX || x & f::GFX_HALTS != 0) || active.is_none_or(|x| x & f::RB_ACTIVE == 0) {
        return Err(GfxError::NotStarted { me, active });
    }
    let (rp, wp) = ptrs(&mut *q.o, &q.a)?;
    if rp != start || wp != start {
        klog::event!(Error, "gx", "gfx.stale_pointers", rptr = klog::Hex(rp), wptr = klog::Hex(wp), start = klog::Hex(start));
        return Err(GfxError::StalePointers { rptr: rp, wptr: wp });
    }
    Ok(())
}

/// The doorbell ranges, MEC, the ME/PFP unhalt and CP_STAT's values after it, then (e1b) the ring.
fn unhalt<H: Hw>(q: &mut Queue<'_, H>, r: &mut GfxReport, v: Variant, start: u64, n: &mut usize) -> Result<(), GfxError> {
    let mec = pre_unhalt(&mut *q.o, &q.a, &q.l, start, v, n)?;
    r.mec_unhalt = Some((mec, gfx11::mec_cntl(mec, true)));
    klog::event!(Info, "gx", "gfx.mec_unhalt", found = klog::Hex(u64::from(mec)), variant = v.name());
    me_unhalt(&mut *q.o, &q.a, n)?;
    let t0 = q.o.now_us();
    let watched = unhalt_watch(q.o, &q.a, q.t, t0, &mut r.unhalt_stat, &mut r.unhalt_more);
    let me = q.a.byte(R::CpMeCntl).and_then(|b| q.o.reg_read(b));
    let stat = q.a.byte(R::CpStat).and_then(|b| q.o.reg_read(b));
    r.started = Some((me, stat));
    watched?;
    if me.is_none_or(|x| x == u32::MAX || x & f::GFX_HALTS != 0) {
        let active = q.a.byte(R::CpRbActive).and_then(|b| q.o.reg_read(b));
        return Err(GfxError::NotStarted { me, active });
    }
    post_unhalt(&mut *q.o, &q.a, &q.l, start, v, n)
}

/// e1a's end: the CP and MEC halted again, then CP_STAT idle.
fn rehalt<H: Hw>(q: &mut Queue<'_, H>, r: &mut GfxReport) -> Result<(), GfxError> {
    stop_regs(&mut *q.o, &q.a)?;
    r.rehalted = true;
    q.o.delay_us(gfx11::MEC_DELAY_US);
    let idle = cp_idle(q.o, &q.a, q.t, "halt");
    let a = q.a;
    let rd = |o: &mut Bringup<Watch<H>>, x: R| a.byte(x).and_then(|b| o.reg_read(b));
    r.rehalt = Some((rd(q.o, R::CpMeCntl), rd(q.o, R::CpMecCntl), rd(q.o, R::CpStat)));
    klog::event!(Info, "gx", "gfx.rehalted", idle = idle.is_ok(); "e1a: the CP and MEC halted again after the unhalt");
    idle
}

fn push(r: &mut GfxReport, x: Rung) -> bool {
    let p = x.pass;
    r.rungs.push(x);
    p
}

fn timeout(r: &GfxReport, wptr: u64) -> GfxError {
    match r.rungs.last() {
        Some(x) if x.done => GfxError::Result { rung: x.name },
        last => GfxError::Timeout { rung: last.map_or("?", |x| x.name), rptr: last.and_then(|x| x.rptr), want: last.map_or(wptr, |x| x.wptr) },
    }
}

/// Stops at the first rung that fails (the CP is then halted by `enable`).
fn rungs<H: Hw>(q: &mut Queue<'_, H>, r: &mut GfxReport) -> Result<(), GfxError> {
    let l = q.l;
    let a = q.a;
    let mut t32 = Timeline::new(0, 16);
    let mut t64 = Timeline::new(FENCE64_START, 16);
    // cp_gfx_start's clear-state preamble (the CSB stream), then Linux's ring test into SCRATCH_REG0.
    let csb = csb();
    let p = q.submit(csb.len() as u64, |g| g.write(&csb))?;
    r.preamble_wptr = Some(p);
    let scratch = a.byte(R::ScratchReg0).ok_or(GfxError::NoAddress)?;
    let scratch_dw = scratch / 4;
    let mut n = 0;
    wr(&mut *q.o, &a, R::ScratchReg0, gfx11::POISON, &mut n)?;
    let w = q.submit(3, |g| g.emit(|v| pm4::set_reg(v, RegSpace::Uconfig, scratch_dw, &[gfx11::RING_TEST])))?;
    let mut x = q.wait(RUNGS[0], w, &mut |hw| hw.reg_read(scratch) == Some(gfx11::RING_TEST));
    let got = q.o.reg_read(scratch);
    x.detail = format!("after the clear-state preamble (wptr {p}, {} dwords); SCRATCH_REG0 (PM4 register {scratch_dw:#x}) reads {}", csb.len(), hx32(got));
    if !push(r, x) {
        return Err(timeout(r, w));
    }
    // Linux's IB test: WRITE_DATA (dst_sel 5, confirmed) of 0xDEADBEEF in an IB, then a 32-bit fence.
    let mut ib = Vec::new();
    pm4::write_data(&mut ib, pm4::dst_sel::MEM, l.ib_target, pm4::WR_CONFIRM, &[gfx11::RING_TEST]).map_err(|_| GfxError::Ring(RingError::State))?;
    q.mem(l.ib, &dwords(&ib))?;
    q.mem(l.ib_target, &gfx11::POISON.to_le_bytes())?;
    q.mem(l.fence_ib, &[0; 8])?;
    q.o.store_barrier();
    let seq = t32.emit().map_err(|_| GfxError::Ring(RingError::State))?;
    let w = q.submit(16, |g| {
        g.emit_ib(0, l.ib, ib.len() as u32, 0, false)?;
        g.emit_fence(l.fence_ib, seq, false, false)
    })?;
    let mut x = q.wait(RUNGS[1], w, &mut |hw| rd32(hw, l.fence_ib) == Some(seq as u32));
    x.fence = rd32(q.o, l.fence_ib).map(u64::from);
    let got = rd32(q.o, l.ib_target);
    if x.pass {
        t32.observe(seq);
        x.pass = got == Some(gfx11::RING_TEST);
    }
    x.detail = format!("IB at MC {:#x} ({} dwords), target MC {:#x} reads {}, fence {seq} read {}", l.ib, ib.len(), l.ib_target, hx32(got), hx(x.fence));
    if !push(r, x) {
        return Err(timeout(r, w));
    }
    // The golden IB: the capture's packet stream, its RELEASE_MEM target in the carve-out, then a fence.
    let mut gib = GOLDEN_IB;
    gib[6] = l.golden_target as u32;
    gib[7] = (l.golden_target >> 32) as u32;
    q.mem(l.golden_ib, &dwords(&gib))?;
    q.mem(l.golden_target, &dwords(&[gfx11::POISON; 2]))?;
    q.mem(l.fence_golden, &[0; 8])?;
    q.o.store_barrier();
    let seq = t32.emit().map_err(|_| GfxError::Ring(RingError::State))?;
    let w = q.submit(16, |g| {
        g.emit_ib(0, l.golden_ib, gib.len() as u32, 0, false)?;
        g.emit_fence(l.fence_golden, seq, false, false)
    })?;
    let mut x = q.wait(RUNGS[2], w, &mut |hw| rd32(hw, l.fence_golden) == Some(seq as u32));
    x.fence = rd32(q.o, l.fence_golden).map(u64::from);
    let got = rd64(q.o, l.golden_target);
    if x.pass {
        t32.observe(seq);
        x.pass = got == Some(GOLDEN_FENCE);
    }
    x.detail = format!("IB at MC {:#x} ({} dwords, the capture's), target MC {:#x} reads {} (the capture: 0x1122334455667788), fence {seq} read {}", l.golden_ib, gib.len(), l.golden_target, hx(got), hx(x.fence));
    if !push(r, x) {
        return Err(timeout(r, w));
    }
    // M5: the RELEASE_MEM end-of-pipe fence of a 64-bit value, across a 2^32 boundary.
    q.mem(l.fence64, &FENCE64_START.to_le_bytes())?;
    q.o.store_barrier();
    let seq = t64.emit().map_err(|_| GfxError::Ring(RingError::State))?;
    let w = q.submit(8, |g| g.emit_fence(l.fence64, seq, true, false))?;
    let mut x = q.wait(RUNGS[3], w, &mut |hw| rd64(hw, l.fence64) == Some(seq));
    x.fence = rd64(q.o, l.fence64);
    if x.pass {
        t64.observe(seq);
    }
    x.detail = format!("RELEASE_MEM (CACHE_FLUSH_AND_INV_TS_EVENT, every cache written back and invalidated, DATA_SEL 2) to MC {:#x}: {:#x} -> {seq:#x}, read {}", l.fence64, FENCE64_START, hx(x.fence));
    if !push(r, x) {
        return Err(timeout(r, w));
    }
    // The doorbell: NBIO's aperture (read-modify-write when off), the ring's doorbell on the idle ring
    // (cp_gfx_set_doorbell with use_doorbell), then a WRITE_DATA + fence committed through BAR2.
    let ab = a.aper_en.ok_or(GfxError::NoDoorbell)?;
    let aper = q.o.reg_read(ab).ok_or(GfxError::Read { byte: ab })?;
    if aper & 1 == 0 && !Regs::wr(&mut *q.o, ab, aper | 1) {
        return Err(GfxError::Write { byte: ab });
    }
    let v = rd(&mut *q.o, &a, R::CpRbDoorbellControl)?;
    wr(&mut *q.o, &a, R::CpRbDoorbellControl, gfx11::doorbell_control(v, true), &mut n)?;
    for (reg, v) in gfx11::set_doorbell_range() {
        wr(&mut *q.o, &a, reg, v, &mut n)?;
    }
    q.doorbell = true;
    // GX-7j3: the MQD says what the registers now say (the doorbell on).
    q.mem(l.mqd + 4 * mesmqd::g::DOORBELL_CONTROL as u64, &direct_mqd(&l, Some(gfx11::DOORBELL_DW))[mesmqd::g::DOORBELL_CONTROL].to_le_bytes())?;
    q.mem(l.db_target, &gfx11::POISON.to_le_bytes())?;
    q.mem(l.fence_db, &[0; 8])?;
    q.o.store_barrier();
    let seq = t32.emit().map_err(|_| GfxError::Ring(RingError::State))?;
    let w = q.submit(16, |g| {
        g.emit(|v| pm4::write_data(v, pm4::dst_sel::MEM, l.db_target, pm4::WR_CONFIRM, &[DOORBELL_VALUE]))?;
        g.emit_fence(l.fence_db, seq, false, false)
    })?;
    let mut x = q.wait(RUNGS[4], w, &mut |hw| rd32(hw, l.fence_db) == Some(seq as u32) && rd32(hw, l.db_target) == Some(DOORBELL_VALUE));
    x.fence = rd32(q.o, l.fence_db).map(u64::from);
    let dc = a.byte(R::CpRbDoorbellControl).and_then(|b| q.o.reg_read(b));
    if x.pass {
        t32.observe(seq);
        x.pass = dc.is_some_and(|v| v & f::DOORBELL_EN != 0);
    }
    x.detail = format!(
        "BAR2 dword {:#x} (byte {:#x}) <- {w:#x}; target MC {:#x} reads {}; fence {seq} read {}; CP_RB_DOORBELL_CONTROL {}",
        gfx11::DOORBELL_DW,
        gfx11::DOORBELL_DW * 4,
        l.db_target,
        hx32(rd32(q.o, l.db_target)),
        hx(x.fence),
        hx32(dc)
    );
    if !push(r, x) {
        return Err(timeout(r, w));
    }
    Ok(())
}

fn hx(v: Option<u64>) -> String {
    v.map_or(String::from("unread"), |x| format!("{x:#x}"))
}

fn hx32(v: Option<u32>) -> String {
    v.map_or(String::from("unread"), |x| format!("{x:#x}"))
}

/// CP_STAT's values after the unhalt as "0x88028000 at 0 us, ..., 0x0 at 18 us".
fn stat_seq(s: &[(u64, Option<u32>)], more: u32) -> String {
    if s.is_empty() {
        return String::from("not watched");
    }
    let mut v: Vec<String> = s.iter().map(|&(t, x)| format!("{} at {t} us", hx32(x))).collect();
    if more > 0 {
        let at = v.len().saturating_sub(1);
        v.insert(at, format!("... {more} more changes ..."));
    }
    let end = match s.last() {
        Some((_, Some(0))) => "",
        _ => " (never 0)",
    };
    format!("{}{end}", v.join(", "))
}

/// One check of the stage (as `pspbringup::Check`).
pub struct GfxCheck {
    pub name: &'static str,
    pub pass: bool,
    pub why: String,
}

/// The pass rules (roadmap wave 26 GX-7j, M5: the fence lands on GFX ring 0, the picture untouched; the picture
/// is the caller's DCN snapshot and CRC).
pub fn checks(r: &GfxReport) -> Vec<GfxCheck> {
    let mut v = Vec::new();
    let mec = r.mec_unhalt.map_or(String::from("MEC not unhalted"), |(a, b)| format!("CP_MEC_CNTL {a:#x} -> {b:#x}"));
    let (pass, why) = match r.started {
        Some((Some(me), Some(stat))) => (me & f::GFX_HALTS == 0 && stat == 0, format!("{mec}; after cp_gfx_enable CP_ME_CNTL {me:#x} (ME and PFP running), CP_STAT {stat:#x}; CP_STAT {}", stat_seq(&r.unhalt_stat, r.unhalt_more))),
        Some((me, stat)) => (false, format!("{mec}; after cp_gfx_enable CP_ME_CNTL {} CP_STAT {}; CP_STAT {}", hx32(me), hx32(stat), stat_seq(&r.unhalt_stat, r.unhalt_more))),
        None => (false, format!("{mec}; not started")),
    };
    let name = match r.variant {
        Variant::E1a => "GFX CP unhalted (e1a: Linux's order, MEC first, no ring)",
        Variant::E1b => "GFX CP started (e1b: the unhalt first, then ring 0, MES idle)",
        Variant::E1c => "GFX CP started (e1c: MEC and ring 0 before the unhalt, MES idle)",
        Variant::Mes => "GFX CP unhalted for MES (Linux 7.1's cp_resume order: MEC, then ME/PFP)",
    };
    v.push(GfxCheck { name, pass, why });
    if r.variant == Variant::E1a {
        let (pass, why) = match r.rehalt {
            Some((Some(me), Some(mec), Some(stat))) => {
                (r.rehalted && me & f::ALL_HALTS == f::ALL_HALTS && mec & f::MEC_ME1_HALT != 0 && stat == 0, format!("CP_ME_CNTL {me:#x}, CP_MEC_CNTL {mec:#x}, CP_STAT {stat:#x}"))
            }
            Some((me, mec, stat)) => (false, format!("CP_ME_CNTL {}, CP_MEC_CNTL {}, CP_STAT {}", hx32(me), hx32(mec), hx32(stat))),
            None => (false, String::from("not halted by the stage (an earlier step failed)")),
        };
        v.push(GfxCheck { name: "GFX CP and MEC halted again (e1a)", pass, why });
    }
    for (i, name) in RUNGS.iter().enumerate().filter(|_| r.variant.runs_ring()) {
        match r.rungs.get(i) {
            Some(x) => v.push(GfxCheck {
                name,
                pass: x.pass,
                why: format!("wptr {}, rptr writeback {}, {} polls, {} us{}; {}", x.wptr, hx(x.rptr), x.polls, x.us, if x.doorbell { ", doorbell" } else { ", CP_RB0_WPTR" }, x.detail),
            }),
            None => v.push(GfxCheck { name, pass: false, why: String::from("not run (an earlier rung or step failed)") }),
        }
    }
    v.push(GfxCheck {
        name: "GFX no new VM fault",
        pass: no_new_fault(r),
        why: format!("GFXHUB before: {}; after: {}", r.fault_before.map_or(String::from("unread"), |f| f.text()), r.fault_after.map_or(String::from("unread"), |f| f.text())),
    });
    v
}

fn no_new_fault(r: &GfxReport) -> bool {
    match (r.fault_before, r.fault_after) {
        (Some(b), Some(a)) => b.status.is_some() && b.status == a.status && b.addr == a.addr,
        _ => false,
    }
}

/// The stage's lines for the answer.
pub fn render(r: &GfxReport) -> Vec<String> {
    let mut v = Vec::new();
    v.push(format!("gfx variant {}: {}", r.variant.name(), r.variant.what()));
    let found: Vec<String> = r.before.iter().map(|(x, val)| format!("{} {}", x.name(), hx32(*val))).collect();
    v.push(format!("gfx found: {}", found.join(", ")));
    if let Some(l) = r.layout {
        v.push(format!(
            "gfx: buffer MC {:#x}{} (ring {:#x}, {} dwords; rptr writeback {:#x}, wptr shadow {:#x}; fences {:#x} {:#x} {:#x} {:#x}; clear-state buffer {:#x}, {CSB_DW} dwords)",
            l.bo_mc,
            if r.reused_bo { " (an earlier run's, reused)" } else { "" },
            l.ring,
            gfx11::RING_DW,
            l.rptr,
            l.wptr,
            l.fence_ib,
            l.fence_golden,
            l.fence64,
            l.fence_db,
            l.csb
        ));
    }
    if r.halted_first {
        v.push(String::from("gfx: CP or MEC found running: halted first (CP_ME_CNTL CE/PFP/ME halt, CP_MEC_CNTL ME1 halt, CP_STAT idle)"));
    }
    if !r.golden.is_empty() {
        let g: Vec<String> = r.golden.iter().map(|(x, found, w)| format!("{} {} -> {w:#x}", x.name(), hx32(*found))).collect();
        v.push(format!("gfx golden: {}", g.join(", ")));
    }
    if let Some((se, sa, act)) = r.shape {
        v.push(format!("gfx constants: {se} shader engine(s), {sa} SA(s) per engine, active SAs {act:#x}; SH_MEM for VMIDs 0-15, GDS cleared, KFD VMIDs 8-15 with the trap enable"));
    }
    for &(what, a, b) in &r.marks {
        v.push(format!("{what}: {} writes", b - a));
    }
    if let Some(w) = r.rewind {
        let ((rp, wp, dc), (rp2, wp2, dc2)) = (w.found, w.after);
        v.push(format!(
            "gfx restart: found CP_RB0_RPTR {rp:#x} CP_RB0_WPTR {wp:#x} CP_RB_DOORBELL_CONTROL {dc:#x}; after the reset (doorbell off, CP_RB_RPTR_WR 0 under RB_RPTR_WR_ENA, wptr 0) CP_RB0_RPTR {rp2:#x} CP_RB0_WPTR {wp2:#x} CP_RB_DOORBELL_CONTROL {dc2:#x}; the ring starts at {:#x}{}",
            w.start,
            if w.start != 0 { " (CP_RB0_RPTR kept its value: taken over)" } else { "" }
        ));
    }
    if r.deactivated {
        v.push(String::from("gfx: CP_RB_ACTIVE found 1 (an earlier run): set 0 before the unhalt, so the CP unhalts with the ring inactive as Linux's"));
    }
    if let Some((m0, m1)) = r.mec_unhalt {
        v.push(format!("gfx mec: CP_MEC_CNTL {m0:#x} -> {m1:#x} (MEC ME1 unhalted, Linux's cp_compute_enable; {} us before the ME/PFP unhalt)", gfx11::MEC_DELAY_US));
    }
    if r.started.is_some() {
        v.push(format!("gfx unhalt CP_STAT ({}): {} (Linux's capture: 0x88028000 ... 0x80008000, 0x0 after ~18 us)", r.variant.name(), stat_seq(&r.unhalt_stat, r.unhalt_more)));
    }
    if let Some(st) = r.ring_stat {
        v.push(format!("gfx e1b: ring 0 programmed on the running CP, CP_RB_ACTIVE last; CP_STAT then {}", hx32(st)));
    }
    if let Some((me, mec, st)) = r.rehalt {
        v.push(format!("gfx e1a re-halt: CP_ME_CNTL {} CP_MEC_CNTL {} CP_STAT {}", hx32(me), hx32(mec), hx32(st)));
    }
    let (b, a) = r.aper;
    v.push(format!("gfx nbio RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN: before {} after {}", hx32(b), hx32(a)));
    for x in &r.rungs {
        v.push(format!("gfx {}: {} ({})", x.name, if x.pass { "ok" } else { "FAILED" }, x.detail));
    }
    for &(i, val, ok) in &r.doorbells {
        v.push(format!("gfx doorbell: BAR2 dword {i:#x} <- {val:#x} ({})", if ok { "written" } else { "REFUSED" }));
    }
    let after: Vec<String> = r.after.iter().map(|(x, val)| format!("{} {}", x.name(), hx32(*val))).collect();
    v.push(format!("gfx after: {}", after.join(", ")));
    match &r.halt_failed {
        Some(e) => v.push(format!("gfx: CP halt FAILED ({e:?}): the CP may still run; until=stop and the Quiesce hook halt it")),
        None => v.push(format!("gfx: CP {}", if r.left_running { "left running (Linux's state; until=stop and the Quiesce hook halt it)" } else { "halted (CP_ME_CNTL CE/PFP/ME and CP_MEC_CNTL ME1)" })),
    }
    if let Some(e) = &r.error {
        v.push(format!("gfx error: {e:?} (errno {}){}", e.errno(), e.advice()));
    }
    v.push(format!("gfx verdict ({}): {}", r.variant.name(), if r.passed() { "PASS" } else { "FAIL" }));
    v
}
