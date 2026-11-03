//! GX-7h: the GMC stage (roadmap wave 25): the GART table in the carve-out, VMID 0 on GFXHUB as Linux programs
//! it, MMHUB only where it differs and the display does not use it, the CPU's TLB flush (engine 17) and the L2
//! protection fault registers read and explained. Generic over the device like `pspbringup` (fakes in tests, the
//! live owner on the Mini-PC); every write goes through the bring-up owner (logged with its read-back) and the
//! scanout watcher.
//!
//! Linux 6.6 (amdgpu/): gmc_v11_0.c gart_enable :904 (MMHUB gart_enable, set_fault_enable_default, flush VMID 0),
//! gfx_v11_0.c:4166 (GFXHUB with GFX hw_init), gfxhub_v3_0.c:353 / mmhub_v3_0_1.c:370 gart_enable (aperture,
//! system aperture, TLB, cache, system domain, identity aperture off, VMID 1-15, invalidation ranges),
//! flush_vm_hub :194 (engine 17; MMHUB takes the engine's semaphore first, :221-235), amdgpu_gart.c:296-318
//! (unbound entries point at the dummy page with no flags), amdgpu_gmc.c:127 pd_addr.
//!
//! Where BitsOS differs (gx-GX-7h.md):
//! - GC is on first (the GFX stage ran): GFXHUB registers read all ones before; Linux programs MMHUB before PSP.
//! - The GART window is where Linux 7.1 put it on the 780M (the capture: 512 MiB at 0x7FFF_0000_0000, high below
//!   the hole) and AGP is off (AGP_BOT 0xFFFFFF, TOP 0), as in the capture; 6.6 would place both elsewhere.
//! - MMHUB (DCN fetches through it, GX-D9): every register Linux would write is read first, each difference is
//!   logged; when a display field (the watcher's `MMHUB_DISPLAY`) would change, nothing is written to MMHUB and
//!   the stage says which (roadmap risk 2: GFXHUB alone carries the GART until the scanout is ours). Otherwise
//!   only the differing registers are written, context 0's last, then the flush (allowed one-shot by the
//!   watcher). L2_CNTL2 is an action (invalidate the L2): it counts as differing only with the L2 setup.
//! - Tables are written by the CPU (`carvetable`), a store barrier before every flush.

use super::bringup::{Bringup, Outcome as WriteOutcome};
use super::carvetable::CarveGart;
use super::disc::Discovery;
use super::psp::{BoAlloc, Hw};
use super::watch::{DisplayGuard, Watch};
use crate::bo::{BoError, PAGE};
use crate::ipmod::gmc11::{self, CONTEXTS, CPU_ENGINE, ENGINES, Fault, Hub, R};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::addr::{Carveout, HOLE_START};
use amdgpu_fmt::gart::Gart;
use amdgpu_fmt::pte::{self, Target};
use amdgpu_fmt::store::TableAddr;

/// Linux's GART window (gmc_v11_0.c:732, amdgpu_gart_size -1): independent of the carve-out's size.
pub const LINUX_GART_SIZE: u64 = 512 << 20;
/// Linux's wait for the semaphore and the ack (adev->usec_timeout, 1 us polls).
pub const LINUX_FLUSH_US: u64 = 100_000;
/// Pages after the table: the system aperture's default page, then the dummy page (fault default, unbound GART
/// entries). Linux uses a VRAM scratch page and a system page for these.
pub const EXTRA_PAGES: u64 = 2;
const FOUR_GB: u64 = 1 << 32;

/// One hub's registers as BAR5 bytes (from the discovery table: any layout).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubAddrs {
    pub hub: Hub,
    /// The segment's base in dwords.
    base: u32,
}

impl HubAddrs {
    pub fn from_discovery(d: &Discovery, hub: Hub) -> Option<HubAddrs> {
        let (hw, seg) = hub.ip();
        Some(HubAddrs { hub, base: d.reg(hw, 0, seg, 0)? })
    }

    pub fn byte(&self, r: R) -> Option<u32> {
        self.base.checked_add(r.dword(self.hub)?)?.checked_mul(4)
    }

    /// The register at `byte` (for names in the answer).
    pub fn reg_at(&self, byte: u32) -> Option<R> {
        regs_of(self.hub).into_iter().chain([R::FaultStatus, R::FaultAddrLo, R::FaultAddrHi, R::FbBase, R::FbTop, R::FbOffset]).chain((0..ENGINES).flat_map(|e| [R::Req(e), R::Sem(e), R::Ack(e)])).find(|&r| self.byte(r) == Some(byte))
    }
}

/// Both hubs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GmcAddrs {
    pub gfx: HubAddrs,
    pub mm: HubAddrs,
}

impl GmcAddrs {
    pub fn from_discovery(d: &Discovery) -> Option<GmcAddrs> {
        Some(GmcAddrs { gfx: HubAddrs::from_discovery(d, Hub::Gfx)?, mm: HubAddrs::from_discovery(d, Hub::Mm)? })
    }

    /// Every register the stage reads (the semaphore only inside a flush: reading it takes it).
    pub fn readable(&self) -> Vec<u32> {
        let mut v = Vec::new();
        for a in [self.gfx, self.mm] {
            let extra = [R::FaultStatus, R::FaultAddrLo, R::FaultAddrHi, R::FbBase, R::FbTop, R::FbOffset, R::Ack(CPU_ENGINE), R::PtBaseLo(0), R::PtBaseHi(0)];
            v.extend(regs_of(a.hub).into_iter().chain(extra).filter_map(|r| a.byte(r)));
            if a.hub == Hub::Mm {
                v.extend(a.byte(R::Sem(CPU_ENGINE)));
            }
        }
        v
    }

    /// Every register the stage may write: GFXHUB's whole plan, its flush and its FB location; MMHUB's plan without the
    /// registers the display uses (`guard`), and its flush (still allowed one-shot by the watcher).
    pub fn writable(&self, guard: &DisplayGuard) -> Vec<u32> {
        // GFXHUB's FB location only when it reads 0 (set from MMHUB's, as Linux's SR-IOV branch).
        let mut v: Vec<u32> = regs_of(Hub::Gfx).into_iter().chain([R::Req(CPU_ENGINE), R::FbBase, R::FbTop]).filter_map(|r| self.gfx.byte(r)).collect();
        v.extend(regs_of(Hub::Mm).into_iter().filter_map(|r| self.mm.byte(r)).filter(|&b| guard.class(b).is_none_or(|c| !c.is_display())));
        v.extend([R::Req(CPU_ENGINE), R::Sem(CPU_ENGINE)].into_iter().filter_map(|r| self.mm.byte(r)));
        v
    }
}

/// Linux's GART placement on the 780M (7.1, AMDGPU_GART_PLACEMENT_HIGH, the capture): the top of the space below
/// the hole, aligned down to 4 GiB (amdgpu_gmc.c:271-299). `None` when it would meet graphics memory.
pub fn gart_location(fb_end: u64, size: u64) -> Option<(u64, u64)> {
    if size == 0 || !size.is_multiple_of(PAGE) {
        return None;
    }
    let max_mc = HOLE_START - 1;
    let start = max_mc.checked_sub(size - 1)? & !(FOUR_GB - 1);
    (start > fb_end).then_some((start, start + size - 1))
}

/// Where everything lies (MC addresses for registers, host-physical for page-table values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub fb_start: u64,
    pub fb_end: u64,
    pub fb_phys: u64,
    pub gart_start: u64,
    pub gart_end: u64,
    pub table_mc: u64,
    pub table_phys: u64,
    pub scratch_phys: u64,
    pub dummy_phys: u64,
}

impl Geometry {
    /// CONTEXT0_PAGE_TABLE_BASE: the table as a PDE (amdgpu_gmc.c:127; valid bit, local memory).
    pub fn pd_addr(&self) -> u64 {
        pte::pd_addr(TableAddr { phys: self.table_phys, target: Target::Local }).unwrap_or(0)
    }

    pub fn entries(&self) -> u64 {
        (self.gart_end + 1 - self.gart_start) / PAGE
    }
}

#[derive(Clone, Copy)]
enum Want {
    Set(u32),
    Rmw(fn(u32) -> u32),
}

/// The registers of one hub's gart_enable and set_fault_enable_default, in Linux's order (no values).
fn regs_of(hub: Hub) -> Vec<R> {
    let g = Geometry { fb_start: 0, fb_end: 0, fb_phys: 0, gart_start: 0, gart_end: 0, table_mc: 0, table_phys: 0, scratch_phys: 0, dummy_phys: 0 };
    targets(hub, &g).into_iter().map(|(r, _)| r).collect()
}

fn lo(v: u64) -> u32 {
    v as u32
}
fn hi(v: u64) -> u32 {
    (v >> 32) as u32
}

fn targets(hub: Hub, g: &Geometry) -> Vec<(R, Want)> {
    use Want::{Rmw, Set};
    let pd = g.pd_addr();
    let mut v = alloc::vec![
        // init_gart_aperture_regs
        (R::PtBaseLo(0), Set(lo(pd))),
        (R::PtBaseHi(0), Set(hi(pd))),
        (R::PtStartLo(0), Set(lo(g.gart_start >> 12))),
        (R::PtStartHi(0), Set(lo(g.gart_start >> 44))),
        (R::PtEndLo(0), Set(lo(g.gart_end >> 12))),
        (R::PtEndHi(0), Set(lo(g.gart_end >> 44))),
        // init_system_aperture_regs (AGP off as on the 780M under Linux 7.1)
        (R::AgpBase, Set(0)),
        (R::AgpBot, Set(0x00ff_ffff)),
        (R::AgpTop, Set(0)),
        (R::ApertLow, Set(lo(g.fb_start >> 18))),
        (R::ApertHigh, Set(lo(g.fb_end >> 18))),
        (R::DefaultLsb, Set(lo(g.scratch_phys >> 12))),
        (R::DefaultMsb, Set(lo(g.scratch_phys >> 44))),
        (R::FaultDefaultLo, Set(lo(g.dummy_phys >> 12))),
        (R::FaultDefaultHi, Set(lo(g.dummy_phys >> 44))),
        (R::FaultCntl2, Rmw(gmc11::fault_cntl2)),
        // init_tlb_regs, init_cache_regs
        (R::L1Tlb, Rmw(gmc11::l1_tlb)),
        (R::L2Cntl, Rmw(gmc11::l2_cntl)),
        (R::L2Cntl2, Rmw(gmc11::l2_cntl2)),
        (R::L2Cntl3, Set(gmc11::L2_CNTL3_VALUE)),
        (R::L2Cntl4, Set(gmc11::L2_CNTL4_VALUE)),
        (R::L2Cntl5, Set(gmc11::L2_CNTL5_VALUE)),
        // enable_system_domain, disable_identity_aperture
        (R::Ctx(0), Rmw(gmc11::ctx0_cntl)),
        (R::IdentLowLo, Set(gmc11::IDENT_LOW_LO)),
        (R::IdentLowHi, Set(gmc11::IDENT_LOW_HI)),
        (R::IdentHighLo, Set(0)),
        (R::IdentHighHi, Set(0)),
        (R::IdentOffLo, Set(0)),
        (R::IdentOffHi, Set(0)),
    ];
    // setup_vmid_config: VMIDs 1-15.
    for n in 1..CONTEXTS {
        v.extend([
            (R::Ctx(n), Rmw(gmc11::ctxn_cntl)),
            (R::PtStartLo(n), Set(0)),
            (R::PtStartHi(n), Set(0)),
            (R::PtEndLo(n), Set(lo(gmc11::MAX_PFN_LAST))),
            (R::PtEndHi(n), Set(hi(gmc11::MAX_PFN_LAST))),
        ]);
    }
    // program_invalidation
    for e in 0..ENGINES {
        v.extend([(R::RangeLo(e), Set(gmc11::RANGE_LO_ALL)), (R::RangeHi(e), Set(gmc11::RANGE_HI_ALL))]);
    }
    // set_fault_enable_default(true): GFXHUB also stops the CP halting on a UTCL1 error.
    if hub == Hub::Gfx {
        v.push((R::CpDebug, Rmw(gmc11::cp_debug)));
    }
    v.push((R::FaultCntl, Rmw(gmc11::fault_cntl)));
    v
}

/// One register of the plan: what it read, what Linux writes there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Planned {
    pub r: R,
    pub byte: u32,
    pub before: u32,
    pub value: u32,
    /// The display's fetch uses it (the watcher's list).
    pub display: bool,
}

impl Planned {
    pub fn differs(&self) -> bool {
        self.before != self.value
    }
}

/// Reads every register of `a`'s plan and computes Linux's value for it (read-modify-write from the value read).
pub fn plan(a: &HubAddrs, g: &Geometry, guard: &DisplayGuard, read: &mut dyn FnMut(u32) -> Option<u32>) -> Result<Vec<Planned>, GmcError> {
    let mut v = Vec::new();
    for (r, w) in targets(a.hub, g) {
        let byte = a.byte(r).ok_or(GmcError::NoAddress(a.hub))?;
        let before = read(byte).ok_or(GmcError::Read { hub: a.hub, byte })?;
        let value = match w {
            Want::Set(x) => x,
            Want::Rmw(f) => f(before),
        };
        v.push(Planned { r, byte, before, value, display: guard.class(byte).is_some_and(|c| c.is_display()) });
    }
    Ok(v)
}

fn is_ctx0(r: R) -> bool {
    matches!(r, R::Ctx(0) | R::PtBaseLo(0) | R::PtBaseHi(0) | R::PtStartLo(0) | R::PtStartHi(0) | R::PtEndLo(0) | R::PtEndHi(0))
}

/// What MMHUB gets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MmDecision {
    /// Every register already holds Linux's value: nothing written.
    Equal,
    /// Only GART fields differ: these, context 0's last.
    Write(Vec<Planned>),
    /// A field the display uses would change: these; nothing is written.
    Stop(Vec<Planned>),
}

/// The differing registers of MMHUB's plan (L2_CNTL2, an invalidate action, only with the L2 setup) and what
/// may be done about them.
pub fn differences(plan: &[Planned]) -> Vec<Planned> {
    let l2 = plan.iter().any(|p| matches!(p.r, R::L2Cntl | R::L2Cntl3 | R::L2Cntl4 | R::L2Cntl5) && p.differs());
    plan.iter().filter(|p| if p.r == R::L2Cntl2 { l2 } else { p.differs() }).copied().collect()
}

pub fn decide(plan: &[Planned]) -> MmDecision {
    let d = differences(plan);
    if d.is_empty() {
        return MmDecision::Equal;
    }
    let display: Vec<Planned> = d.iter().filter(|p| p.display).copied().collect();
    if !display.is_empty() {
        return MmDecision::Stop(display);
    }
    let (ctx0, rest): (Vec<Planned>, Vec<Planned>) = d.into_iter().partition(|p| is_ctx0(p.r));
    MmDecision::Write(rest.into_iter().chain(ctx0).collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GmcError {
    NoAddress(Hub),
    Read { hub: Hub, byte: u32 },
    /// GFXHUB's registers read all ones: GC is not powered (the GFX stage first).
    GcOff { byte: u32 },
    /// GFXHUB's copy of the FB location or offset is not MMHUB's.
    FbMismatch { what: &'static str, gfx: u32, mm: u32 },
    /// The hubs' FB location disagrees with the carve-out the caller knows.
    Carveout { what: &'static str, hub: u64, carve: u64 },
    Geometry,
    Bo(BoError),
    TableTooSmall { have: u64, need: u64 },
    TableWrite { mc: u64 },
    TableReadBack { index: u64, read: Option<u64>, want: u64 },
    Write { hub: Hub, byte: u32 },
    Flush { hub: Hub, what: &'static str },
}

impl GmcError {
    pub fn errno(&self) -> i32 {
        match self {
            GmcError::NoAddress(_) | GmcError::GcOff { .. } => 19,
            GmcError::Read { .. } | GmcError::TableWrite { .. } | GmcError::TableReadBack { .. } | GmcError::Write { .. } => 5,
            GmcError::Bo(_) => 12,
            GmcError::Flush { .. } => 62,
            _ => 22,
        }
    }

    pub fn advice(&self) -> &'static str {
        match self {
            GmcError::GcOff { .. } => ": GC is not on: run the GFX stage first (until=gfx); nothing was written",
            GmcError::FbMismatch { .. } | GmcError::Carveout { .. } => ": the hubs and the carve-out disagree: nothing was written; report the values",
            GmcError::Flush { .. } => ": the hub never acknowledged the TLB flush: the tables are written but not proven in use; report it",
            GmcError::Write { .. } => ": a hub write was refused (the live owner's list or the watcher): stopped there",
            _ => "",
        }
    }
}

/// One TLB flush of VMID 0 through engine 17.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flush {
    pub hub: Hub,
    pub req: u32,
    /// MMHUB: whether the semaphore was taken (Linux goes on without it, gmc_v11_0.c:229-230).
    pub sem: Option<bool>,
    pub acked: bool,
    pub polls: u32,
    pub after_us: u64,
}

/// Polls `done` until it holds, at most `budget_us` (1 us steps, 10 us after the first 100 us).
pub(crate) fn poll(hw: &mut dyn Hw, budget_us: u64, polls: &mut u32, done: &mut dyn FnMut(&mut dyn Hw) -> bool) -> bool {
    let t0 = hw.now_us();
    for _ in 0..budget_us.saturating_add(2) {
        if done(hw) {
            return true;
        }
        *polls = polls.saturating_add(1);
        let el = hw.now_us().saturating_sub(t0);
        if el >= budget_us {
            break;
        }
        hw.delay_us(if el < 100 { 1 } else { 10 }.min(budget_us - el).max(1));
    }
    false
}

/// gmc_v11_0_flush_vm_hub for VMID 0 (flush type 0). The request and the semaphore are written without the
/// read-back (reading the semaphore takes it); on MMHUB both pass the watcher only by its one-shot allowance.
pub fn flush<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &HubAddrs, budget_us: u64) -> (Flush, Option<GmcError>) {
    let hub = a.hub;
    let req = gmc11::invalidate_req(0, 0);
    let mut f = Flush { hub, req, sem: None, acked: false, polls: 0, after_us: 0 };
    let (Some(req_b), Some(ack_b)) = (a.byte(R::Req(CPU_ENGINE)), a.byte(R::Ack(CPU_ENGINE))) else {
        return (f, Some(GmcError::NoAddress(hub)));
    };
    let sem_b = if hub == Hub::Mm { a.byte(R::Sem(CPU_ENGINE)) } else { None };
    let t0 = owner.now_us();
    if let Some(sb) = sem_b {
        let mut p = 0;
        let got = poll(owner, budget_us, &mut p, &mut |hw| hw.reg_read(sb).is_some_and(|v| v != u32::MAX && v & 1 != 0));
        f.polls = p;
        f.sem = Some(got);
        if !got {
            klog::event!(Error, "gx", "gmc.sem_timeout", hub = hub.name(), waited_us = budget_us; "no semaphore for the flush: Linux goes on without it");
        }
        owner.hw().allow(req_b, req, "GMC stage: MMHUB VMID 0 flush after its GART writes (engine 17, as Linux)");
        owner.hw().allow(sb, 0, "GMC stage: the flush's semaphore released (as Linux)");
    }
    let e = owner.write_blind(req_b, req);
    let mut err = None;
    if e.outcome != WriteOutcome::Ok {
        err = Some(GmcError::Write { hub, byte: req_b });
    } else {
        let mut p = 0;
        f.acked = poll(owner, budget_us, &mut p, &mut |hw| hw.reg_read(ack_b).is_some_and(|v| v != u32::MAX && v & 1 != 0));
        f.polls = f.polls.saturating_add(p);
        if !f.acked {
            klog::event!(Error, "gx", "gmc.flush_timeout", hub = hub.name(), waited_us = budget_us; "no ack for the VMID 0 flush");
            err = Some(GmcError::Flush { hub, what: "ack" });
        }
    }
    if let Some(sb) = sem_b {
        let e = owner.write_blind(sb, 0);
        if e.outcome != WriteOutcome::Ok && err.is_none() {
            err = Some(GmcError::Write { hub, byte: sb });
        }
    }
    owner.hw().drop_allowed();
    f.after_us = owner.now_us().saturating_sub(t0);
    klog::event!(Info, "gx", "gmc.flush", hub = hub.name(), req = klog::Hex(u64::from(req)), acked = f.acked, polls = f.polls, after_us = f.after_us);
    (f, err)
}

/// A hub's L2 protection fault registers as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultRead {
    pub hub: Hub,
    pub status: Option<u32>,
    pub addr: Option<u64>,
}

impl FaultRead {
    pub fn text(&self) -> String {
        match (self.status, self.addr) {
            (None, _) => String::from("not readable"),
            (Some(u32::MAX), _) => String::from("reads all ones (hub not powered)"),
            (Some(s), a) => {
                let f = Fault::decode(s);
                if s == 0 { f.text(self.hub) } else { format!("{}, page {}", f.text(self.hub), a.map_or(String::from("?"), |x| format!("{x:#x}"))) }
            }
        }
    }
}

/// gmc_v11_0_process_interrupt's reads (GFXHUB: one dummy read first, :112-117), without the clear.
pub fn read_fault(hw: &mut dyn Hw, a: &HubAddrs) -> FaultRead {
    let rd = |hw: &mut dyn Hw, r: R| a.byte(r).and_then(|b| hw.reg_read(b));
    if a.hub == Hub::Gfx {
        let _ = rd(hw, R::FaultStatus);
    }
    let status = rd(hw, R::FaultStatus);
    let addr = rd(hw, R::FaultAddrLo).zip(rd(hw, R::FaultAddrHi)).map(|(l, h)| Fault::address(l, h));
    FaultRead { hub: a.hub, status, addr }
}

/// The GART table's buffer (kept for the boot, reused by a later run).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableBo {
    pub handle: u32,
    /// Carve-out offsets.
    pub offset: u64,
    pub size: u64,
}

/// Bytes the table and its two pages need for a window of `gart_size`.
pub fn table_bytes(gart_size: u64) -> u64 {
    (gart_size / PAGE * 8).div_ceil(PAGE) * PAGE + EXTRA_PAGES * PAGE
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GmcInputs {
    pub addrs: GmcAddrs,
    pub carve: Carveout,
    pub gart_size: u64,
    pub flush_us: u64,
    /// The table of an earlier run in this boot (reused: the hubs may still point at it).
    pub table: Option<TableBo>,
}

impl GmcInputs {
    pub fn new(d: &Discovery, carve: Carveout) -> Option<GmcInputs> {
        Some(GmcInputs { addrs: GmcAddrs::from_discovery(d)?, carve, gart_size: LINUX_GART_SIZE, flush_us: LINUX_FLUSH_US, table: None })
    }
}

/// What the stage did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GmcReport {
    pub geo: Option<Geometry>,
    pub table: Option<TableBo>,
    pub reused_table: bool,
    /// The first and last entry read back after the fill.
    pub table_check: Option<(Option<u64>, Option<u64>)>,
    pub gfx_plan: Vec<Planned>,
    pub mm_plan: Vec<Planned>,
    pub mm: Option<MmDecision>,
    pub gfx_written: usize,
    pub mm_written: usize,
    /// GFXHUB's FB location read 0 and was set from MMHUB's.
    pub gfx_fb_copied: bool,
    pub flushes: Vec<Flush>,
    pub faults_before: Vec<FaultRead>,
    pub faults_after: Vec<FaultRead>,
    /// CONTEXT0_PAGE_TABLE_BASE (lo, hi) and CONTEXT0_CNTL read after the stage, per hub.
    pub after: Vec<(Hub, Option<u32>, Option<u32>, Option<u32>)>,
    pub error: Option<GmcError>,
}

impl GmcReport {
    pub fn passed(&self) -> bool {
        self.error.is_none() && checks(self).iter().all(|c| c.pass)
    }

    /// Context 0 was on before the stage (an earlier kernel's or run's GART).
    pub fn enabled_before(&self, hub: Hub) -> Option<bool> {
        let p = if hub == Hub::Gfx { &self.gfx_plan } else { &self.mm_plan };
        p.iter().find(|p| p.r == R::Ctx(0)).map(|p| p.before & gmc11::f::CTX_ENABLE != 0)
    }
}

/// The stage. Nothing is written before both hubs' FB location agree with `inp.carve` and GC reads powered.
pub fn enable<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &GmcInputs) -> GmcReport {
    let mut r = GmcReport::default();
    match run(owner, bo, inp, &mut r) {
        Ok(()) => klog::event!(Info, "gx", "gmc.enabled", gfx_writes = r.gfx_written, mm_writes = r.mm_written,
            mm = match &r.mm { Some(MmDecision::Stop(_)) => "stopped", Some(MmDecision::Write(_)) => "written", _ => "equal" }),
        Err(e) => {
            klog::event!(Error, "gx", "gmc.failed", error = format!("{e:?}").as_str(), gfx_writes = r.gfx_written, mm_writes = r.mm_written);
            r.error = Some(e);
        }
    }
    r.faults_after = alloc::vec![read_fault(owner, &inp.addrs.gfx), read_fault(owner, &inp.addrs.mm)];
    r
}

fn rd<H: Hw>(o: &mut Bringup<Watch<H>>, a: &HubAddrs, x: R) -> Result<u32, GmcError> {
    let b = a.byte(x).ok_or(GmcError::NoAddress(a.hub))?;
    o.reg_read(b).ok_or(GmcError::Read { hub: a.hub, byte: b })
}

fn write_all<H: Hw>(owner: &mut Bringup<Watch<H>>, hub: Hub, list: &[Planned], n: &mut usize) -> Result<(), GmcError> {
    for p in list {
        let e = owner.write(p.byte, p.value);
        if !matches!(e.outcome, WriteOutcome::Ok | WriteOutcome::Mismatch) {
            return Err(GmcError::Write { hub, byte: p.byte });
        }
        *n += 1;
    }
    Ok(())
}

fn run<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &GmcInputs, r: &mut GmcReport) -> Result<(), GmcError> {
    let (gx, mm, carve) = (inp.addrs.gfx, inp.addrs.mm, inp.carve);
    // 1. The FB location both hubs use (MMHUB's, as Linux's mmhub get_fb_location; GFXHUB's copy must agree).
    let mut fb = [[0u32; 3]; 2];
    for (k, a) in [mm, gx].iter().enumerate() {
        for (i, x) in [R::FbBase, R::FbTop, R::FbOffset].into_iter().enumerate() {
            fb[k][i] = rd(owner, a, x)?;
        }
    }
    if fb[1].contains(&u32::MAX) {
        return Err(GmcError::GcOff { byte: gx.byte(R::FbBase).unwrap_or(0) });
    }
    // GFXHUB's FB location left 0: Linux's SR-IOV branch programs it from MMHUB's (gfxhub_v3_0.c:355-364);
    // any other difference is refused.
    let unset = fb[1][0] == 0 && fb[1][1] == 0;
    let mut copy_fb = false;
    for (i, what) in ["FB_LOCATION_BASE", "FB_LOCATION_TOP", "FB_OFFSET"].into_iter().enumerate() {
        if (fb[0][i] ^ fb[1][i]) & 0xff_ffff != 0 {
            if i < 2 && unset {
                copy_fb = true;
                continue;
            }
            return Err(GmcError::FbMismatch { what, gfx: fb[1][i], mm: fb[0][i] });
        }
    }
    let m24 = |v: u32| u64::from(v & 0xff_ffff) << 24;
    let (fb_start, fb_end, fb_phys) = (m24(fb[0][0]), m24(fb[0][1]) | 0xff_ffff, m24(fb[0][2]));
    for (what, hub, c) in [("MC base", fb_start, carve.mc_base()), ("CPU base", fb_phys, carve.phys_base())] {
        if hub != c {
            return Err(GmcError::Carveout { what, hub, carve: c });
        }
    }
    if fb_end - fb_start + 1 < carve.size() {
        return Err(GmcError::Carveout { what: "size", hub: fb_end - fb_start + 1, carve: carve.size() });
    }
    r.faults_before = alloc::vec![read_fault(owner, &gx), read_fault(owner, &mm)];
    // 2. The table: one BO for the table, the default page and the dummy page (kept for the boot).
    let (gart_start, gart_end) = gart_location(fb_end, inp.gart_size).ok_or(GmcError::Geometry)?;
    let need = table_bytes(inp.gart_size);
    let table = match inp.table {
        Some(t) if t.size >= need => {
            r.reused_table = true;
            t
        }
        Some(t) => return Err(GmcError::TableTooSmall { have: t.size, need }),
        None => {
            let (handle, range) = bo.alloc(need, PAGE).map_err(GmcError::Bo)?;
            TableBo { handle, offset: range.start, size: range.end - range.start }
        }
    };
    r.table = Some(table);
    let entries = inp.gart_size / PAGE;
    let scratch_off = table.offset + (entries * 8).div_ceil(PAGE) * PAGE;
    let phys = |off: u64| carve.phys_of(off).ok_or(GmcError::Geometry);
    let geo = Geometry {
        fb_start,
        fb_end,
        fb_phys,
        gart_start,
        gart_end,
        table_mc: carve.mc_base() + table.offset,
        table_phys: phys(table.offset)?,
        scratch_phys: phys(scratch_off)?,
        dummy_phys: phys(scratch_off + PAGE)?,
    };
    r.geo = Some(geo);
    let gart = Gart::new(gart_start, inp.gart_size, geo.dummy_phys, carve).map_err(|_| GmcError::Geometry)?;
    {
        let mut t = CarveGart::new(&mut *owner, geo.table_mc, entries);
        t.fill(gart.unbound());
        if t.failed() {
            return Err(GmcError::TableWrite { mc: geo.table_mc });
        }
    }
    let zero = [0u8; PAGE as usize];
    for off in [scratch_off, scratch_off + PAGE] {
        if !owner.mem_write(carve.mc_base() + off, &zero) {
            return Err(GmcError::TableWrite { mc: carve.mc_base() + off });
        }
    }
    owner.store_barrier();
    {
        use amdgpu_fmt::gart::GartRead;
        let t = CarveGart::new(&mut *owner, geo.table_mc, entries);
        let (first, last) = (t.read(0), t.read(entries - 1));
        r.table_check = Some((first, last));
        for (index, read) in [(0, first), (entries - 1, last)] {
            if read != Some(gart.unbound()) {
                return Err(GmcError::TableReadBack { index, read, want: gart.unbound() });
            }
        }
    }
    klog::event!(Info, "gx", "gmc.table", mc = klog::Hex(geo.table_mc), phys = klog::Hex(geo.table_phys), entries = entries, reused = r.reused_table);
    let guard = owner.hw().guard().clone();
    // 3. GFXHUB as Linux (DCN does not use it): every write, Linux's order, then the flush.
    if copy_fb {
        klog::event!(Warn, "gx", "gmc.gfxhub_fb_copied", base = klog::Hex(u64::from(fb[0][0])), top = klog::Hex(u64::from(fb[0][1])); "GFXHUB's FB location read 0: set from MMHUB's");
        let list: Vec<Planned> = [(R::FbBase, fb[0][0]), (R::FbTop, fb[0][1])]
            .into_iter()
            .filter_map(|(x, v)| gx.byte(x).map(|byte| Planned { r: x, byte, before: 0, value: v, display: false }))
            .collect();
        let mut n = 0;
        write_all(owner, Hub::Gfx, &list, &mut n)?;
        r.gfx_fb_copied = true;
    }
    r.gfx_plan = plan(&gx, &geo, &guard, &mut |b| owner.reg_read(b))?;
    let list = r.gfx_plan.clone();
    write_all(owner, Hub::Gfx, &list, &mut r.gfx_written)?;
    owner.store_barrier();
    let (f, e) = flush(owner, &gx, inp.flush_us);
    r.flushes.push(f);
    if let Some(e) = e {
        return Err(e);
    }
    // 4. MMHUB: read all of it, log each difference, then write the GART fields only if no display field moves.
    r.mm_plan = plan(&mm, &geo, &guard, &mut |b| owner.reg_read(b))?;
    for p in differences(&r.mm_plan) {
        klog::event!(Info, "gx", "gmc.mmhub.differs", reg = p.r.name(Hub::Mm).as_str(), before = klog::Hex(u64::from(p.before)), linux = klog::Hex(u64::from(p.value)), display = p.display);
    }
    let d = decide(&r.mm_plan);
    match &d {
        MmDecision::Equal => klog::event!(Info, "gx", "gmc.mmhub.equal"; "every MMHUB register holds Linux's value: nothing written"),
        MmDecision::Stop(list) => klog::event!(Warn, "gx", "gmc.mmhub.stopped", display_fields = list.len(); "an MMHUB field the display uses would change: MMHUB not written"),
        MmDecision::Write(list) => {
            let list = list.clone();
            let res = write_all(owner, Hub::Mm, &list, &mut r.mm_written);
            if let Err(e) = res {
                r.mm = Some(d);
                return Err(e);
            }
            owner.store_barrier();
            let (f, e) = flush(owner, &mm, inp.flush_us);
            r.flushes.push(f);
            if let Some(e) = e {
                r.mm = Some(d);
                return Err(e);
            }
        }
    }
    r.mm = Some(d);
    // 5. What the hubs hold now.
    for a in [gx, mm] {
        let mut b = |x: R| a.byte(x).and_then(|b| owner.reg_read(b));
        let after = (a.hub, b(R::PtBaseLo(0)), b(R::PtBaseHi(0)), b(R::Ctx(0)));
        r.after.push(after);
    }
    Ok(())
}

/// One check of the stage (as `pspbringup::Check`).
pub struct GmcCheck {
    pub name: &'static str,
    pub pass: bool,
    pub why: String,
}

fn hx(v: Option<u32>) -> String {
    v.map_or(String::from("unread"), |x| format!("{x:#x}"))
}

/// The pass rules (roadmap wave 25 GX-7h; GR-1's first Mini-PC check: CONTEXT0_PAGE_TABLE_BASE = pd_addr).
pub fn checks(r: &GmcReport) -> Vec<GmcCheck> {
    let mut v = Vec::new();
    let geo = r.geo;
    v.push(GmcCheck {
        name: "GMC GART table",
        pass: r.table_check.is_some_and(|(a, b)| a.is_some() && a == b) && !matches!(r.error, Some(GmcError::TableWrite { .. } | GmcError::TableReadBack { .. })),
        why: match geo {
            Some(g) => format!("{} entries at MC {:#x} (CPU {:#x}), every one unbound (the dummy page), first/last read back {:?}", g.entries(), g.table_mc, g.table_phys, r.table_check),
            None => String::from("not built"),
        },
    });
    let gfx_ok = !r.gfx_plan.is_empty() && r.gfx_written == r.gfx_plan.len();
    v.push(GmcCheck { name: "GMC GFXHUB programmed as Linux", pass: gfx_ok, why: format!("{} of {} writes", r.gfx_written, r.gfx_plan.len()) });
    let fl = |h: Hub| r.flushes.iter().find(|f| f.hub == h);
    let ftext = |f: Option<&Flush>| f.map_or(String::from("not sent"), |f| format!("engine {CPU_ENGINE} request {:#x}, acked {} after {} polls, {} us{}", f.req, f.acked, f.polls, f.after_us, f.sem.map_or(String::new(), |s| format!(", semaphore taken {s}"))));
    v.push(GmcCheck { name: "GMC GFXHUB TLB flush", pass: fl(Hub::Gfx).is_some_and(|f| f.acked), why: ftext(fl(Hub::Gfx)) });
    let pd = geo.map(|g| g.pd_addr());
    let after = |h: Hub| r.after.iter().find(|a| a.0 == h).copied();
    let base_ok = |h: Hub| after(h).is_some_and(|(_, l, hi, c)| pd.is_some_and(|pd| l == Some(pd as u32) && hi == Some((pd >> 32) as u32)) && c.is_some_and(|c| c & 1 != 0));
    v.push(GmcCheck {
        name: "GMC GFXHUB context 0",
        pass: base_ok(Hub::Gfx),
        why: match after(Hub::Gfx) {
            Some((_, l, h, c)) => format!("CONTEXT0_PAGE_TABLE_BASE {}:{} (pd_addr {}), CONTEXT0_CNTL {}", hx(h), hx(l), pd.map_or(String::from("-"), |p| format!("{p:#x}")), hx(c)),
            None => String::from("not read"),
        },
    });
    let (mm_pass, mm_why) = match &r.mm {
        None => (false, String::from("not reached")),
        Some(MmDecision::Equal) => (true, String::from("every register already holds Linux's value: nothing written")),
        Some(MmDecision::Stop(l)) => (true, format!("not written: {} display field(s) would change (GFXHUB alone carries the GART; roadmap risk 2)", l.len())),
        Some(MmDecision::Write(l)) => (fl(Hub::Mm).is_some_and(|f| f.acked) && r.mm_written == l.len(), format!("{} of {} GART-field writes (context 0 last), flush: {}", r.mm_written, l.len(), ftext(fl(Hub::Mm)))),
    };
    v.push(GmcCheck { name: "GMC MMHUB display fields unchanged", pass: mm_pass, why: mm_why });
    let same = |h: Hub| {
        let b = r.faults_before.iter().find(|f| f.hub == h);
        let a = r.faults_after.iter().find(|f| f.hub == h);
        b.is_some() && a.is_some() && b.map(|f| (f.status, f.addr)) == a.map(|f| (f.status, f.addr)) && a.is_some_and(|f| f.status.is_some_and(|s| s != u32::MAX))
    };
    let ftexts: Vec<String> = r.faults_after.iter().map(|f| format!("{} {}", f.hub.name(), f.text())).collect();
    v.push(GmcCheck { name: "GMC no new VM fault", pass: same(Hub::Gfx) && same(Hub::Mm), why: ftexts.join("; ") });
    v
}

/// The stage's lines for the answer.
pub fn render(r: &GmcReport) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(g) = r.geo {
        v.push(format!(
            "gmc: graphics memory MC {:#x}..{:#x} at CPU {:#x}; GART window {:#x}..{:#x} ({} MiB, {} entries); table at MC {:#x} = CPU {:#x}{}; CONTEXT0_PAGE_TABLE_BASE {:#x}; default page {:#x}, dummy page {:#x}",
            g.fb_start,
            g.fb_end,
            g.fb_phys,
            g.gart_start,
            g.gart_end,
            (g.gart_end + 1 - g.gart_start) >> 20,
            g.entries(),
            g.table_mc,
            g.table_phys,
            if r.reused_table { " (the table of an earlier run, reused)" } else { "" },
            g.pd_addr(),
            g.scratch_phys,
            g.dummy_phys
        ));
    }
    for h in [Hub::Gfx, Hub::Mm] {
        if let Some(on) = r.enabled_before(h) {
            v.push(format!("gmc: {} context 0 before: {}", h.name(), if on { "on (an earlier GART: reprogrammed as Linux's resume does)" } else { "off" }));
        }
    }
    for f in &r.faults_before {
        v.push(format!("gmc fault before, {}: {}", f.hub.name(), f.text()));
    }
    if r.gfx_fb_copied {
        v.push(String::from("gmc GFXHUB: its FB location read 0 and was set from MMHUB's (Linux's SR-IOV path, gfxhub_v3_0.c:355-364)"));
    }
    if !r.gfx_plan.is_empty() {
        let d = r.gfx_plan.iter().filter(|p| p.differs()).count();
        v.push(format!("gmc GFXHUB: {} writes as Linux (gfxhub_v3_0 gart_enable, set_fault_enable_default), {d} changed a value, {} written", r.gfx_plan.len(), r.gfx_written));
    }
    for f in &r.flushes {
        v.push(format!(
            "gmc flush {}: engine {CPU_ENGINE} request {:#x}, acked {} after {} polls, {} us{}",
            f.hub.name(),
            f.req,
            f.acked,
            f.polls,
            f.after_us,
            f.sem.map_or(String::new(), |s| format!(", semaphore taken {s}, released"))
        ));
    }
    if !r.mm_plan.is_empty() {
        let d = differences(&r.mm_plan);
        v.push(format!("gmc MMHUB: {} registers read, {} differ from Linux's values", r.mm_plan.len(), d.len()));
        for p in &d {
            v.push(format!(
                "gmc MMHUB differs: {} {:#x} -> Linux {:#x} ({})",
                p.r.name(Hub::Mm),
                p.before,
                p.value,
                if p.display { "display field: the display fetches through it" } else { "GART field" }
            ));
        }
    }
    match &r.mm {
        Some(MmDecision::Equal) => v.push(String::from("gmc MMHUB: nothing written (every register already holds Linux's value)")),
        Some(MmDecision::Write(l)) => v.push(format!("gmc MMHUB: {} of {} GART-field writes, context 0 last, then the flush; no display field touched", r.mm_written, l.len())),
        Some(MmDecision::Stop(l)) => v.push(format!(
            "gmc MMHUB: STOPPED before any MMHUB write: {} display field(s) would change ({}); MMHUB keeps the firmware's values, GFXHUB alone carries the GART until the scanout is ours (roadmap risk 2)",
            l.len(),
            l.iter().map(|p| p.r.name(Hub::Mm)).collect::<Vec<_>>().join(", ")
        )),
        None => {}
    }
    for &(h, l, hi, c) in &r.after {
        v.push(format!("gmc after: {} CONTEXT0_PAGE_TABLE_BASE {}:{} CONTEXT0_CNTL {}", h.name(), hx(hi), hx(l), hx(c)));
    }
    for f in &r.faults_after {
        v.push(format!("gmc fault after, {}: {}", f.hub.name(), f.text()));
    }
    if let Some(e) = &r.error {
        v.push(format!("gmc error: {e:?} (errno {}){}", e.errno(), e.advice()));
    }
    v
}

/// The stage's writes as Linux makes them in one go (the plan of both hubs plus each flush's request and
/// semaphore), for the Linux diff: what BitsOS would write with nothing equal and nothing refused.
pub fn linux_sequence(r: &GmcReport, a: &GmcAddrs) -> Vec<(u32, u32)> {
    let req = gmc11::invalidate_req(0, 0);
    let mut v: Vec<(u32, u32)> = r.mm_plan.iter().map(|p| (p.byte, p.value)).collect();
    v.extend(a.mm.byte(R::Req(CPU_ENGINE)).map(|b| (b, req)));
    v.extend(a.mm.byte(R::Sem(CPU_ENGINE)).map(|b| (b, 0)));
    v.extend(r.gfx_plan.iter().map(|p| (p.byte, p.value)));
    v.extend(a.gfx.byte(R::Req(CPU_ENGINE)).map(|b| (b, req)));
    v
}
