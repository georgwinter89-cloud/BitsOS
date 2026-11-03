//! GX-7d: one PSP bring-up run (M1: the ring answers; M2: the firmware is accepted) through the bring-up owner
//! (`Bringup`: every write logged with its read-back) and the scanout watcher in front of the device (`Watch`
//! in `Mode::Block`: a write to a display register or the scanout surface never reaches it). Generic over the
//! device: the fakes in host tests and ktests, `psplive::LiveHw` on the Mini-PC, so the desk's run is the code
//! the tests ran. Pure apart from klog: the live glue gathers the inputs (start state, carve-out, firmware).
//!
//! Policy (gx-roadmap.md wave 24, gx-scout-bringup.md M1/M2): a PSP whose secure OS does not run, or a start
//! state the classifier could not read, is refused before any write; any start but cold destroys a stale ring
//! and TMR first; any non-zero status is fatal and undone; unless `keep`, the run ends with the undo (DESTROY_TMR,
//! DESTROY_RINGS), so it can be repeated.
//!
//! GX-7e: `Until::Gfx` (M3) runs M2, then the SMU/GC stage (`gfxup::power_up`) through the same owner and
//! watcher while the TMR is up; the undo (unless `keep`) follows it and leaves GC on and booted (the desk's
//! Reload rule: GC on, ring and TMR destroyed). `mode2` is the recovery path, alone.
//!
//! GX-7h: `Until::Gart` runs M3, then the GMC stage (`gmc::enable`: the GART table, GFXHUB, MMHUB only where the
//! display does not care) through the same owner and watcher, while TMR and ring stay (held for GC).
//!
//! GX-7i: `Until::Sdma` runs the GART, then the SDMA stage (`sdma::enable`: SDMA 0's ring and its rungs) through
//! the same owner and watcher.
//!
//! GX-7j: `Until::GfxRing` runs the GART, then the GFX stage (`gfxring::enable`: the GFX tail, ring 0 driven by
//! the CP directly, its rungs and the M5 fence) through the same owner and watcher. It does not need SDMA.
//!
//! GX-7k: `Until::Mes` runs the GART, then the GFX stage in Linux's MES order (`Variant::Mes`: the tail and the
//! CP unhalt, the CP left running), then MES (`mes::enable`); `Until::GfxMes` also maps a compute ring and GFX
//! ring 0 through MES and runs their rungs.

use crate::bo::BoError;
use crate::gpucore::bringup::{Bringup, Entry, Outcome as WriteOutcome, Totals};
use crate::gpucore::classify::Start;
use crate::gpucore::gfxring::{self, GfxAddrs, GfxRingInputs};
use crate::gpucore::gfxup::{self, GcAddrs, GfxReport, GfxTimeouts};
use crate::gpucore::smu::{MailboxState, SmuError};
use crate::gpucore::gmc::{self, GmcInputs, GmcReport};
use crate::gpucore::mes::{self, MesInputs, MesReport};
use crate::gpucore::sdma::{self, GartUse, SdmaInputs, SdmaReport};
use crate::gpucore::vmring::{self, VmAddrs, VmReport};
use crate::gpucore::psp::{self, BoAlloc, Carve, Hw, Psp, PspError, Report, Step, Timeouts, Until};
use crate::gpucore::ucode::{FwSet, Ucode};
use crate::gpucore::watch::{Violation, Watch};
use crate::ipmod::psp13::{Addrs, Mailbox13};
use crate::ipmod::smu13;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

/// What LOAD_TOC answered on the 780M under Linux (27.10. capture): printed beside ours, not a pass rule.
pub const LINUX_780M_TMR: u32 = 0x400_0000;

pub struct Inputs<'a> {
    pub addrs: Addrs,
    pub carve: Carve,
    /// The classifier's verdict, read just before.
    pub start: Start,
    pub until: Until,
    pub fw: &'a FwSet,
    pub list: &'a [Ucode],
    pub autoload: bool,
    /// Leave the ring (and TMR) up for the next item; the Quiesce hook destroys them at Reload.
    pub keep: bool,
    pub t: Timeouts,
    /// GX-7e: the SMU's and GC's addresses and waits (needed for `Until::Gfx`).
    pub gfx: Option<GfxInputs>,
    /// GX-7h: the hubs and the carve-out (needed for `Until::Gart`).
    pub gmc: Option<GmcInputs>,
    /// GX-7i: SDMA 0, its buffer and rung 3's system pages (needed for `Until::Sdma`).
    pub sdma: Option<SdmaInputs<'a>>,
    /// GX-7j: GC's CP registers, the GFX buffer of an earlier run (needed for `Until::GfxRing`).
    pub gfx_ring: Option<GfxRingInputs<'a>>,
    /// GX-7k: MES's registers, buffer and firmware starts (needed for `Until::Mes`/`GfxMes`).
    pub mes: Option<MesInputs<'a>>,
    /// GX-7m: GFXHUB and the IH's PASID table (needed for `Until::Vm`).
    pub vm: Option<VmAddrs>,
    /// GX-7j3: GC was left booted by a BitsOS kernel that did not tell PMFW of the unload (a note saying
    /// `Booted`, or none): a GC stage unloads it first (`gfxup::unload`), before the PSP is touched.
    pub unload_first: bool,
}

/// What the SMU/GC stage needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GfxInputs {
    pub smu: smu13::Addrs,
    pub gc: GcAddrs,
    pub t: GfxTimeouts,
}

impl GfxInputs {
    pub fn from_discovery(d: &crate::gpucore::disc::Discovery) -> Option<GfxInputs> {
        Some(GfxInputs { smu: smu13::Addrs::from_discovery(d)?, gc: GcAddrs::from_discovery(d)?, t: GfxTimeouts::LINUX })
    }
}

/// The CPU maps the carve-out in pieces of this size (`psplive`, uncached).
pub const PIECE: u64 = 2 << 20;

/// Every PSP buffer in whole, aligned pieces of its own (GX-7d fixer): the uncached CPU mapping of a piece then
/// covers only PSP-owned pages, never a page another mapping holds with another memory type (the GOP surface
/// and A1/cursor are write-combining: Intel SDM Vol. 3 11.12.4 forbids such aliases). The BO keeps the whole
/// pieces; the PSP is told the size it asked for.
pub struct Pieces<'a>(pub &'a mut dyn BoAlloc);

impl BoAlloc for Pieces<'_> {
    fn alloc(&mut self, size: u64, align: u64) -> Result<(u32, Range<u64>), BoError> {
        let whole = size.div_ceil(PIECE).checked_mul(PIECE).ok_or(BoError::NoSpace)?;
        let (h, r) = self.0.alloc(whole.max(PIECE), align.max(PIECE))?;
        Ok((h, r.start..r.start + size.min(r.end - r.start)))
    }
    fn release(&mut self, handle: u32) {
        self.0.release(handle);
    }
}

/// The PSP's buffers of one boot (GX-7d fixer): BOs taken from the manager once and never given back, reused by
/// later runs. A piece the CPU mapped uncached stays mapped for the boot (hal has no unmap yet), so no other BO
/// may ever come into it: a released buffer goes back to this pool, not to the manager. Bounded by the largest
/// set one run holds at once (one run at a time; equal sizes reuse the same entries).
#[derive(Debug, Default)]
pub struct Pool {
    /// (handle, carve-out range, in use).
    pub entries: Vec<(u32, Range<u64>, bool)>,
}

impl Pool {
    pub const fn new() -> Pool {
        Pool { entries: Vec::new() }
    }

    /// The smallest free entry that fits `size` at `align`, else a new BO from `bo`.
    pub fn alloc(&mut self, bo: &mut dyn BoAlloc, size: u64, align: u64) -> Result<(u32, Range<u64>), BoError> {
        let align = align.max(crate::bo::PAGE);
        let fit = self
            .entries
            .iter_mut()
            .filter(|(_, r, busy)| !*busy && r.start % align == 0 && r.end - r.start >= size)
            .min_by_key(|(_, r, _)| r.end - r.start);
        if let Some((h, r, busy)) = fit {
            *busy = true;
            return Ok((*h, r.start..r.start + size));
        }
        let (h, r) = bo.alloc(size, align)?;
        self.entries.push((h, r.clone(), true));
        Ok((h, r))
    }

    pub fn release(&mut self, handle: u32) {
        if let Some((_, _, busy)) = self.entries.iter_mut().find(|(h, _, _)| *h == handle) {
            *busy = false;
        }
    }

    /// Whether `r` is one of the pool's BOs.
    pub fn holds(&self, r: &Range<u64>) -> bool {
        self.entries.iter().any(|(_, x, _)| x == r)
    }
}

/// `Pool` over a manager, as one `BoAlloc`.
pub struct Pooled<'a>(pub &'a mut Pool, pub &'a mut dyn BoAlloc);

impl BoAlloc for Pooled<'_> {
    fn alloc(&mut self, size: u64, align: u64) -> Result<(u32, Range<u64>), BoError> {
        self.0.alloc(&mut *self.1, size, align)
    }
    fn release(&mut self, handle: u32) {
        self.0.release(handle);
    }
}

/// The piece-rounded carve-out range of a buffer at `off` with `size` bytes (what `psplive` maps of it). Tests
/// only today (C1).
#[cfg(test)]
pub fn piece_range(off: u64, size: u64) -> Range<u64> {
    let s = off / PIECE * PIECE;
    s..(off.saturating_add(size)).div_ceil(PIECE).saturating_mul(PIECE)
}

/// Whether the start state allows a run: `Ok(stale)` (destroy first), `Err(why)` (nothing is written).
pub fn admit(start: Start) -> Result<bool, &'static str> {
    match start {
        Start::Cold => Ok(false),
        Start::AfterLinux | Start::AfterBitsOs | Start::AfterBitsOsNoGc | Start::AfterCrash => Ok(true),
        Start::GcUnsettled => Err("the kernel before sent EnableGfxImu and GC did not boot; TMR and ring are left under a possibly running RLC/IMU (its note): reboot (mode 2 is refused on 13.0.4)"),
        Start::PspDown => Err("the PSP secure OS does not run (C2PMSG_81 is 0): BitsOS has no bootloader path, Linux refuses too; reboot"),
        Start::Unknown => Err("the start state is unknown (a register the classifier needs did not read): nothing written"),
    }
}

/// One milestone's check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub pass: bool,
    pub why: String,
}

pub struct Outcome {
    pub report: Report,
    pub error: Option<PspError>,
    pub checks: Vec<Check>,
    /// The undo after the run: `None` when the PSP was kept (or nothing was created).
    pub undo: Option<Result<(), PspError>>,
    /// The PSP with its ring (and TMR) live: when `keep` and the run passed, and (GX-7f) always once
    /// EnableGfxImu reached PMFW (`held_for_gc`).
    pub kept: Option<Psp<Mailbox13>>,
    /// GX-7f: the ring and TMR stay because EnableGfxImu reached PMFW (the RLC/IMU may read the TMR); Linux keeps
    /// both after its autoload too. `until=stop`, the Quiesce hook (GC booted only) or a reboot frees them.
    pub held_for_gc: bool,
    /// The PSP may still hold its buffers (a failed undo, a wedged ring, a ring create without an answer): they
    /// are NOT given back, so no display BO lands on them before a reboot; the caller parks it.
    pub quarantined: Option<Psp<Mailbox13>>,
    /// The owner's entries of this run (oldest first) and totals.
    pub entries: Vec<Entry>,
    pub totals: Totals,
    pub violations: Vec<Violation>,
    pub violations_total: u64,
    /// The fence value after the run (before the undo).
    pub fence: u32,
    /// Index in `entries` where the closing undo's writes begin (the Linux diff leaves them out: the trace ends
    /// with the driver loaded).
    pub undo_from: Option<usize>,
    /// GX-7e: the SMU/GC stage, when it ran.
    pub gfx: Option<GfxReport>,
    /// GX-7h: the GMC stage, when it ran.
    pub gmc: Option<GmcReport>,
    /// GX-7i: the SDMA stage, when it ran.
    pub sdma: Option<SdmaReport>,
    /// GX-7i: the SDMA stage's resumes as indexes into `entries` (start, end), for the Linux diff's marks.
    pub sdma_resumes: Vec<(bool, usize, usize)>,
    /// GX-7j: the GFX stage, when it ran, and its tail/resume as indexes into `entries` (name, start, end).
    pub gfx_ring: Option<gfxring::GfxReport>,
    pub gfx_marks: Vec<(&'static str, usize, usize)>,
    /// GX-7k: the MES stage, when it ran, and the stop of a MES found running before it (its lines).
    pub mes: Option<MesReport>,
    pub mes_pre: Vec<String>,
    /// GX-7k: that stop left legacy queues MES did not unmap.
    pub mes_pre_stranded: bool,
    /// GX-7m: the VM stage, when it ran (boxed: the run's frame lives on ktest stacks).
    pub vm: Option<alloc::boxed::Box<VmReport>>,
    /// GX-7j3: PMFW's mailbox as the GC stage found it before any write, and the catch-up unload.
    pub pmfw: Option<Result<MailboxState, SmuError>>,
    pub catch_up: Option<gfxup::Unload>,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.error.is_none()
            && self.checks.iter().all(|c| c.pass)
            && self.violations_total == 0
            && !matches!(self.undo, Some(Err(_)))
            && self.quarantined.is_none()
            && self.gfx.as_ref().is_none_or(|g| g.error.is_none())
            && self.gmc.as_ref().is_none_or(GmcReport::passed)
            && self.sdma.as_ref().is_none_or(SdmaReport::passed)
            && self.gfx_ring.as_ref().is_none_or(gfxring::GfxReport::passed)
            && self.mes.as_ref().is_none_or(MesReport::passed)
            && self.vm.as_ref().is_none_or(|v| v.passed())
            && !self.pmfw_stuck()
    }

    /// GX-7j3: PMFW did not answer before the load (a message left in flight, or the catch-up unload): nothing
    /// was loaded, and only a reboot clears it (mode 2 is refused on 13.0.4).
    pub fn pmfw_stuck(&self) -> bool {
        matches!(self.pmfw, Some(Ok(MailboxState::Stuck { .. })))
            || matches!(self.pmfw, Some(Err(e)) if e.needs_reboot())
            || self.catch_up.is_some_and(|u| matches!(u.prepared, Err(e) if e.needs_reboot()))
    }

    /// The GC stage failed after EnableGfxImu reached PMFW: GC may be powered, booting or stuck (a reboot is the
    /// recovery path; mode 2 is refused, `gfxup::MODE2_REFUSED`).
    pub fn gc_failed(&self) -> bool {
        self.gfx.as_ref().is_some_and(|g| g.error.is_some() && g.gc_touched)
    }

    /// The run ended in a state only a reboot clears: an error after which the PSP may still be busy or never
    /// gets ready, a failed undo, or buffers the PSP may still hold.
    pub fn needs_reboot(&self) -> bool {
        self.error.as_ref().is_some_and(needs_reboot)
            || matches!(self.undo, Some(Err(_)))
            || self.quarantined.is_some()
            || self.gfx.as_ref().and_then(|g| g.error).is_some_and(|e| e.needs_reboot())
            || self.gc_failed()
            || self.pmfw_stuck()
    }
}

fn reg(s: &psp::Snap, name: &str) -> Option<u32> {
    s.regs.iter().find(|(n, _)| *n == name).and_then(|&(_, v)| v)
}

fn snap_of(r: &Report, f: impl Fn(&Step) -> bool) -> Option<&psp::Snap> {
    r.steps.iter().position(f).and_then(|i| r.snaps.get(i))
}

/// The pass rules of M1 and M2 (gx-scout-bringup.md: C2PMSG_64 `& 0x8000FFFF == 0x80000000` after create, the
/// fence moving per command, LOAD_TOC's TMR size, every image accepted, autoload after RLC_G).
pub fn checks(r: &Report, inp: &Inputs, fence: u32) -> Vec<Check> {
    let mut v = Vec::new();
    let created = snap_of(r, |s| matches!(s, Step::RingCreated { .. }));
    let ctrl = created.and_then(|s| reg(s, "C2PMSG_64"));
    v.push(Check {
        name: "M1 ring created",
        pass: ctrl.is_some_and(|c| c & 0x8000_FFFF == 0x8000_0000),
        why: match ctrl {
            Some(c) => format!("C2PMSG_64 after create {c:#010x} (bit 31 set, status {:#x})", c & psp::STATUS_MASK),
            None => String::from("no ring created"),
        },
    });
    let toc = r.steps.iter().find_map(|s| if let Step::Toc { tmr_size } = s { Some(*tmr_size) } else { None });
    let toc_fence = snap_of(r, |s| matches!(s, Step::Toc { .. })).and_then(|s| s.fence);
    v.push(Check {
        name: "M1 LOAD_TOC answered",
        pass: toc.is_some_and(|t| t != 0) && toc_fence.is_some_and(|f| f != 0),
        why: match toc {
            Some(t) => format!("TMR size {t:#x} (Linux on the 780M: {LINUX_780M_TMR:#x}), fence {}", toc_fence.map_or(String::from("unread"), |f| format!("{f}"))),
            None => String::from("no LOAD_TOC answer"),
        },
    });
    if inp.until.loads_fw() {
        let tmr = r.steps.iter().find_map(|s| if let Step::Tmr { mc, size } = s { Some((*mc, *size)) } else { None });
        v.push(Check {
            name: "M2 SETUP_TMR",
            pass: tmr.is_some(),
            why: tmr.map_or(String::from("no TMR"), |(mc, size)| format!("TMR at MC {mc:#x}, {size:#x} bytes")),
        });
        let all = r.loaded.len() == inp.list.len() && r.loaded.iter().zip(inp.list).all(|(l, u)| l.id == u.id && l.status == 0);
        v.push(Check { name: "M2 every image accepted", pass: all, why: format!("{} of {} images, all status 0: {all}", r.loaded.len(), inp.list.len()) });
        let auto = r.steps.contains(&Step::AutoloadRlc);
        v.push(Check { name: "M2 AUTOLOAD_RLC", pass: auto || !inp.autoload, why: format!("sent: {auto}") });
        // Every command moved the fence by one: commands sent = fence value.
        let cmds = r.steps.iter().filter(|s| matches!(s, Step::StaleTmrDestroyed { .. } | Step::Toc { .. } | Step::Tmr { .. } | Step::Load(_) | Step::AutoloadRlc)).count();
        v.push(Check { name: "M2 fence per command", pass: fence as usize == cmds, why: format!("fence {fence}, commands {cmds}") });
    }
    v
}

/// The M3 checks of a GC stage (or one failed check when it did not run).
pub fn gfx_checks(g: Option<&GfxReport>) -> Vec<Check> {
    match g {
        None => alloc::vec![Check { name: "M3 GC stage", pass: false, why: String::from("not run: the PSP part did not pass") }],
        Some(g) => gfxup::checks(g).into_iter().map(|c| Check { name: c.name, pass: c.pass, why: c.why }).collect(),
    }
}

/// The GMC checks of a GMC stage (or one failed check when it did not run).
pub fn gmc_checks(g: Option<&GmcReport>) -> Vec<Check> {
    match g {
        None => alloc::vec![Check { name: "GMC stage", pass: false, why: String::from("not run: the GC stage did not pass") }],
        Some(g) => gmc::checks(g).into_iter().map(|c| Check { name: c.name, pass: c.pass, why: c.why }).collect(),
    }
}

/// The SDMA checks of an SDMA stage (or one failed check when it did not run).
pub fn sdma_checks(g: Option<&SdmaReport>) -> Vec<Check> {
    match g {
        None => alloc::vec![Check { name: "SDMA stage", pass: false, why: String::from("not run: the GMC stage did not pass") }],
        Some(g) => sdma::checks(g).into_iter().map(|c| Check { name: c.name, pass: c.pass, why: c.why }).collect(),
    }
}

/// The GFX ring checks of a GFX stage (or one failed check when it did not run).
pub fn gfx_ring_checks(g: Option<&gfxring::GfxReport>) -> Vec<Check> {
    match g {
        None => alloc::vec![Check { name: "GFX ring stage", pass: false, why: String::from("not run: the GMC stage did not pass") }],
        Some(g) => gfxring::checks(g).into_iter().map(|c| Check { name: c.name, pass: c.pass, why: c.why }).collect(),
    }
}

/// GX-7k: the MES checks of a MES stage (or one failed check when it did not run).
pub fn mes_checks(g: Option<&MesReport>) -> Vec<Check> {
    match g {
        None => alloc::vec![Check { name: "MES stage", pass: false, why: String::from("not run: the GFX stage (Linux's CP unhalt) did not pass") }],
        Some(g) => mes::checks(g).into_iter().map(|c| Check { name: c.name, pass: c.pass, why: c.why }).collect(),
    }
}

/// GX-7m: the VM checks of a VM stage (or one failed check when it did not run).
pub fn vm_checks(g: Option<&VmReport>) -> Vec<Check> {
    match g {
        None => alloc::vec![Check { name: "VM stage", pass: false, why: String::from("not run: the gfx_mes queues (or the SDMA stage) did not pass") }],
        Some(g) => vmring::checks(g).into_iter().map(|c| Check { name: c.name, pass: c.pass, why: c.why }).collect(),
    }
}

/// M3 while the TMR holds the images the RLC autoload reads: PSP first, then the SMU (fixed order).
#[inline(never)]
fn stage_gc<H: Hw>(owner: &mut Bringup<Watch<H>>, inp: &Inputs, stale: bool, pass: bool, out: &mut Outcome) {
    match inp.gfx {
        Some(g) if pass => {
            let known = matches!(inp.start, Start::AfterLinux | Start::AfterBitsOs);
            out.gfx = Some(gfxup::power_up(owner, g.smu, &g.gc, stale, known, &g.t));
        }
        Some(_) => {}
        None => klog::event!(Error, "gx", "bringup.no_gfx_addrs"; "until gfx without SMU/GC addresses"),
    }
    let c = gfx_checks(out.gfx.as_ref());
    out.checks.extend(c);
}

/// GX-7h: the hubs need GC on (GFXHUB lives in it): only after a passed GC stage.
#[inline(never)]
fn stage_gmc<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &Inputs, pass: bool, out: &mut Outcome) {
    match inp.gmc {
        Some(g) if pass => out.gmc = Some(gmc::enable(owner, bo, &g)),
        Some(_) => {}
        None => klog::event!(Error, "gx", "bringup.no_gmc_addrs"; "until gart without hub addresses"),
    }
    let c = gmc_checks(out.gmc.as_ref());
    out.checks.extend(c);
}

/// GX-7i: SDMA is a GFXHUB client: only after a passed GMC stage.
#[inline(never)]
fn stage_sdma<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &Inputs, pass: bool, seq0: u64, out: &mut Outcome) {
    let gu = out.gmc.as_ref().zip(inp.gmc.as_ref()).and_then(|(g, m)| gart_use(g, m));
    match (inp.sdma.as_ref(), gu) {
        (Some(s), Some(gu)) if pass => {
            let rep = sdma::enable(owner, bo, s, &gu);
            out.sdma_resumes = rep.resumes.iter().map(|&(db, _, a, b)| (db, (a - seq0) as usize, (b - seq0) as usize)).collect();
            out.sdma = Some(rep);
        }
        (None, _) => klog::event!(Error, "gx", "bringup.no_sdma_addrs"; "until sdma without SDMA addresses"),
        _ => {}
    }
    let c = sdma_checks(out.sdma.as_ref());
    out.checks.extend(c);
}

/// GX-7k: a MES found running is stopped before the GFX stage; `false` when that stop failed.
#[inline(never)]
fn stage_mes_pre<H: Hw>(owner: &mut Bringup<Watch<H>>, inp: &Inputs, out: &mut Outcome) -> bool {
    let Some(st) = inp.mes.as_ref().and_then(|x| mes::pre_stop(owner, x)) else { return true };
    out.mes_pre_stranded = !st.stranded.is_empty();
    out.mes_pre = st.lines;
    st.ok
}

/// GX-7j: the CP is a GFXHUB client: only after a passed GMC stage (no SDMA needed).
#[inline(never)]
fn stage_gfx_ring<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &Inputs, pass: bool, seq0: u64, out: &mut Outcome) {
    match (inp.gfx_ring.as_ref(), inp.gmc.as_ref(), out.gmc.as_ref()) {
        (Some(g), Some(m), Some(_)) if pass => {
            let rep = gfxring::enable(owner, bo, &gfx_stage_inputs(g, inp.until), &m.addrs.gfx);
            out.gfx_marks = gfx_marks(&rep, seq0);
            out.gfx_ring = Some(rep);
        }
        (None, ..) => klog::event!(Error, "gx", "bringup.no_gfx_ring_addrs"; "until gfx_ring without CP addresses"),
        _ => {}
    }
    let c = gfx_ring_checks(out.gfx_ring.as_ref());
    out.checks.extend(c);
}

/// GX-7k: MES after Linux's CP unhalt (the GFX stage passed and left the CP running).
#[inline(never)]
fn stage_mes<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &Inputs, pass: bool, seq0: u64, out: &mut Outcome) {
    match (inp.mes.as_ref(), inp.gmc.as_ref(), out.gfx_ring.as_ref()) {
        (Some(x), Some(m), Some(g)) if pass => {
            let mi = MesInputs { queues: inp.until.maps_queues(), ..*x };
            let rep = mes::enable(owner, bo, &mi, &m.addrs.gfx, g.layout.as_ref());
            out.gfx_marks.extend(rep.marks.iter().map(|&(n, a, b)| (n, (a - seq0) as usize, (b - seq0) as usize)));
            out.mes = Some(rep);
        }
        (None, ..) => klog::event!(Error, "gx", "bringup.no_mes_addrs"; "until mes without MES addresses"),
        _ => {}
    }
    let c = mes_checks(out.mes.as_ref());
    out.checks.extend(c);
}

/// GX-7m: the VM stage on the queues gfx_mes mapped and the SDMA queue the SDMA stage left (its own frame: the
/// run's is large in debug builds and ktest stacks are small).
#[inline(never)]
fn vm_stage<H: Hw>(owner: &mut Bringup<Watch<H>>, inp: &Inputs, out: &mut Outcome, pass: bool) {
    let vi = inp.vm.zip(inp.gmc.map(|g| g.carve)).and_then(|(a, c)| vm_inputs(a, c, inp.sdma.as_ref().map(|s| s.addrs), out));
    match (inp.vm, vi) {
        (Some(_), Some(vi)) if pass => out.vm = Some(Box::new(vmring::enable(owner, &vi, None))),
        (None, _) => klog::event!(Error, "gx", "bringup.no_vm_addrs"; "until vm without hub addresses"),
        _ => {}
    }
    let c = vm_checks(out.vm.as_deref());
    out.checks.extend(c);
}

/// GX-7m: the VM stage's inputs from the stages of this run (the MES and GFX buffers, SDMA's kept queue).
pub fn vm_inputs(a: VmAddrs, carve: amdgpu_fmt::addr::Carveout, sdma_addrs: Option<sdma::SdmaAddrs>, out: &Outcome) -> Option<vmring::VmInputs> {
    let m = out.mes.as_ref().filter(|m| m.left_running && m.queues)?;
    let g = out.gfx_ring.as_ref()?.layout?;
    let sdma = match (out.sdma.as_ref(), sdma_addrs) {
        (Some(r), Some(addrs)) if r.passed() => r.layout.map(|layout| vmring::SdmaSide { addrs, layout, burst_nop: r.burst_nop }),
        _ => None,
    };
    Some(vmring::VmInputs::new(a, carve, m.bo?, m.layout?, g, sdma, m.frames.len() as u64 + 1))
}

/// GX-7k: the GFX stage's inputs as the run needs them: the MES order for `until=mes`/`gfx_mes`.
pub fn gfx_stage_inputs<'a>(g: &GfxRingInputs<'a>, until: Until) -> GfxRingInputs<'a> {
    if until.fake_chain_mes() { GfxRingInputs { variant: gfxring::Variant::Mes, ..*g } } else { *g }
}

/// GX-7j: a GFX stage's marks (owner write counts) as indexes into a run's entries that began at `seq0`.
pub fn gfx_marks(r: &gfxring::GfxReport, seq0: u64) -> Vec<(&'static str, usize, usize)> {
    r.marks.iter().map(|&(n, a, b)| (n, (a - seq0) as usize, (b - seq0) as usize)).collect()
}

/// What the SDMA stage needs of a passed GMC stage.
pub fn gart_use(g: &GmcReport, m: &GmcInputs) -> Option<GartUse> {
    Some(GartUse { geo: g.geo?, gfx: m.addrs.gfx, flush_us: m.flush_us })
}

/// The run: the PSP's buffers from `bo`, the bring-up until `inp.until`, then (unless kept) the undo and the
/// buffers back. Nothing is written when `admit` refuses (the caller checks it first; this checks it again).
///
/// GX-7m2: the outcome is boxed and every stage runs in a frame of its own (`#[inline(never)]` below): the live
/// run is on MCP's 64 KiB request thread and the fake chains' ktests must fit 48 KiB there (CONTRACTS section 5).
pub fn run<H: Hw>(owner: &mut Bringup<Watch<H>>, bo: &mut dyn BoAlloc, inp: &Inputs) -> Box<Outcome> {
    let bo = &mut Pieces(bo);
    let seq0 = owner.log().totals().writes;
    let v0 = owner.hw().total();
    let mut out = Box::new(Outcome {
        report: Report::default(),
        error: None,
        checks: Vec::new(),
        undo: None,
        kept: None,
        quarantined: None,
        entries: Vec::new(),
        totals: Totals::default(),
        violations: Vec::new(),
        violations_total: 0,
        fence: 0,
        undo_from: None,
        gfx: None,
        gmc: None,
        sdma: None,
        sdma_resumes: Vec::new(),
        gfx_ring: None,
        gfx_marks: Vec::new(),
        mes: None,
        mes_pre: Vec::new(),
        mes_pre_stranded: false,
        vm: None,
        held_for_gc: false,
        pmfw: None,
        catch_up: None,
    });
    let stale = match admit(inp.start) {
        Ok(s) => s,
        Err(why) => {
            klog::event!(Warn, "gx", "bringup.refused", start = inp.start.name(), why = why);
            if inp.start == Start::PspDown {
                out.error = Some(PspError::SosNotRunning);
            }
            out.checks.push(Check { name: "admitted", pass: false, why: String::from(why) });
            return out;
        }
    };
    if inp.until.runs_gc()
        && let Some(g) = inp.gfx
        && !pmfw_first(owner, &g, inp.unload_first, inp.start, &mut out)
    {
        return finish(owner, out, seq0, v0, inp);
    }
    match Psp::new(bo, Mailbox13 { addrs: inp.addrs }, inp.carve, inp.t) {
        Err(e) => out.error = Some(e),
        Ok(mut p) => {
            p.stale_hint = stale;
            let r = p.start_until(owner, bo, inp.fw, inp.list, inp.autoload, inp.until);
            out.report = p.last.clone();
            out.fence = p.fence_value();
            out.error = r.err();
            out.checks.extend(checks(&out.report, inp, out.fence));
            let mut pass = out.error.is_none() && out.checks.iter().all(|c| c.pass) && owner.hw().total() == v0;
            if inp.until.runs_gc() {
                stage_gc(owner, inp, stale, pass, &mut out);
                pass = pass && out.checks.iter().all(|c| c.pass) && owner.hw().total() == v0;
            }
            if inp.until.runs_gmc() {
                stage_gmc(owner, bo, inp, pass, &mut out);
                pass = pass && out.checks.iter().all(|c| c.pass) && owner.hw().total() == v0;
            }
            if inp.until.fake_chain_sdma() {
                stage_sdma(owner, bo, inp, pass, seq0, &mut out);
                pass = pass && out.checks.iter().all(|c| c.pass) && owner.hw().total() == v0;
            }
            if inp.until.fake_chain_mes() && pass {
                pass &= stage_mes_pre(owner, inp, &mut out);
            }
            if inp.until.runs_gfx_stage() || inp.until == Until::Vm {
                stage_gfx_ring(owner, bo, inp, pass, seq0, &mut out);
                pass = pass && out.checks.iter().all(|c| c.pass) && owner.hw().total() == v0;
            }
            if inp.until.fake_chain_mes() {
                stage_mes(owner, bo, inp, pass, seq0, &mut out);
                pass = pass && out.checks.iter().all(|c| c.pass) && owner.hw().total() == v0;
            }
            if inp.until == Until::Vm {
                vm_stage(owner, inp, &mut out, pass);
                pass = pass && out.checks.iter().all(|c| c.pass) && owner.hw().total() == v0;
            }
            // GX-7f: once EnableGfxImu reached PMFW the IMU/RLC may read the TMR (booting, or booted and kept
            // running): no DESTROY_TMR under it. Linux keeps ring and TMR after its autoload; its teardown after a
            // failed GC load (amdgpu_psp.c:2756-2758) came on 28.10. while GC may have been booting.
            let gc_touched = out.gfx.as_ref().is_some_and(|g| g.gc_touched);
            if gc_touched && p.running() && !(inp.keep && pass) {
                klog::event!(Warn, "gx", "bringup.held_for_gc", failed = out.gc_failed(); "EnableGfxImu reached PMFW: TMR and ring stay (until=stop, Quiesce when GC booted, or a reboot)");
                out.held_for_gc = true;
                out.kept = Some(p);
            } else if inp.keep && pass {
                klog::event!(Info, "gx", "bringup.kept", until = format!("{:?}", inp.until).as_str(), fence = out.fence; "ring kept: Quiesce destroys it");
                out.kept = Some(p);
            } else {
                // A failed start undid itself already (`stop` is then a no-op).
                if p.running() {
                    out.undo_from = Some((owner.log().totals().writes - seq0) as usize);
                    out.undo = Some(p.stop(owner));
                }
                if p.quiet() {
                    p.free(bo);
                } else {
                    klog::event!(Error, "gx", "bringup.quarantined", buffers = p.buffers().count(); "the PSP may still hold its buffers: kept out of the BO manager until a reboot");
                    out.quarantined = Some(p);
                }
            }
        }
    }
    finish(owner, out, seq0, v0, inp)
}

fn finish<H: Hw>(owner: &mut Bringup<Watch<H>>, mut out: Box<Outcome>, seq0: u64, v0: u64, inp: &Inputs) -> Box<Outcome> {
    out.entries = owner.log().since(seq0).copied().collect();
    out.totals = owner.log().totals();
    out.violations = owner.hw().violations().to_vec();
    out.violations_total = owner.hw().total() - v0;
    klog::event!(Info, "gx", "bringup.run", until = format!("{:?}", inp.until).as_str(), passed = out.passed(), writes = out.entries.len(),
        violations = out.violations_total);
    out
}

/// GX-7j3: whether a GC stage unloads GC toward PMFW before the load: after a BitsOS kernel (a stop, or a crash)
/// whose note does not say it unloaded. Cold: PMFW never saw a GFX driver; after Linux: its shutdown sent
/// PrepareMp1ForUnload (amdgpu_pci_shutdown with mp1_state UNLOAD).
pub fn unload_first(start: Start, note_unloaded: bool) -> bool {
    matches!(start, Start::AfterBitsOs | Start::AfterCrash) && !note_unloaded
}

/// GX-7j3: before a GC stage touches the PSP, PMFW must answer: a message found in flight is waited for
/// (`smu::settle`, Linux's pre-poll bound); then a GC an earlier BitsOS kernel left booted without telling PMFW
/// is unloaded (`unload_first`). `false`: PMFW is stuck, the run ends here with nothing loaded (the 29.10. Mini-PC:
/// loading the firmware again under such a PMFW was followed by a PMFW that never answered).
fn pmfw_first(hw: &mut dyn Hw, g: &GfxInputs, unload_first: bool, start: Start, out: &mut Outcome) -> bool {
    let st = super::smu::settle(hw, &g.smu, g.t.smu);
    match st {
        Ok(MailboxState::LateAnswer { msg, code, after_us }) => {
            klog::event!(Warn, "gx", "smu.late_answer", msg = klog::Hex(u64::from(msg)), code = klog::Hex(u64::from(code)), after_us = after_us; "a message left in flight was answered late: PMFW recovered")
        }
        Ok(MailboxState::Stuck { msg, waited_us }) => {
            klog::event!(Error, "gx", "smu.stuck_before_load", msg = klog::Hex(u64::from(msg)), waited_us = waited_us; "nothing loaded: reboot")
        }
        Ok(MailboxState::Free { .. }) => {}
        Err(e) => klog::event!(Error, "gx", "smu.unreadable", error = format!("{e:?}").as_str()),
    }
    out.pmfw = Some(st);
    let free = matches!(st, Ok(MailboxState::Free { .. } | MailboxState::LateAnswer { .. }));
    let mut why = match st {
        Ok(s) => mailbox_text(&s),
        Err(e) => format!("the mailbox did not read: {e:?}"),
    };
    let mut pass = free;
    if free && unload_first {
        // The RLC only when GC is known booted (a stop's note or its registers); after a crash its power is unknown.
        let u = gfxup::unload(hw, &g.smu, (start == Start::AfterBitsOs).then_some(&g.gc), g.t.smu);
        why += &format!("; GC left booted without an unload: caught up first: {}", u.text());
        // Only a PMFW that stopped answering ends the run (Linux goes on after a refused unload too).
        pass = !matches!(u.prepared, Err(e) if e.needs_reboot());
        out.catch_up = Some(u);
    }
    out.checks.push(Check { name: "PMFW answers before the load", pass, why });
    pass
}

/// The mailbox state as one line.
pub fn mailbox_text(s: &MailboxState) -> String {
    match *s {
        MailboxState::Free { code, msg } => format!("mailbox free (answer {code:#x} to message {msg:#x} {})", smu13::msg_name(msg)),
        MailboxState::LateAnswer { msg, code, after_us } => {
            format!("message {msg:#x} {} was left in flight; PMFW answered {code:#x} after {after_us} us (recovered, nothing re-sent)", smu13::msg_name(msg))
        }
        MailboxState::Stuck { msg, waited_us } => format!(
            "message {msg:#x} {} is in flight and PMFW gave no answer in {waited_us} us: PMFW is stuck; nothing loaded or sent (Linux sends nothing into a busy mailbox, smu_cmn.c:368-373; mode 2 is refused on 13.0.4): reboot",
            smu13::msg_name(msg)
        ),
    }
}

/// The per-boot "GC stage failed" latch after a run (`prev` before it): only a failure that reached GC sets it
/// and only a GC stage that booted GC clears it. A later run that stops before EnableGfxImu (`again=true`) says
/// nothing about GC and must not hide the earlier failure from `until=reset`.
pub fn gc_latch(prev: bool, out: &Outcome) -> bool {
    match &out.gfx {
        _ if out.gc_failed() => true,
        Some(g) if g.error.is_none() => false,
        _ => prev,
    }
}

/// Errors after which the PSP may still be busy with our frame or never gets ready: a reboot, not a retry.
pub fn needs_reboot(e: &PspError) -> bool {
    matches!(e, PspError::Timeout { what: "ring.ready" | "fence" | "ring.create" | "ring.stop", .. } | PspError::Wedged | PspError::RingNotReset { .. })
}

/// The register's name for the answer (the PSP's mailbox; anything else by its byte).
pub fn reg_name(a: &Addrs, g: Option<&GfxInputs>, m: Option<&GmcInputs>, byte: u32) -> String {
    reg_name_sdma(a, g, m, None, byte)
}

/// `reg_name` with SDMA 0's and NBIO's doorbell registers (GX-7i).
pub fn reg_name_sdma(a: &Addrs, g: Option<&GfxInputs>, m: Option<&GmcInputs>, s: Option<&sdma::SdmaAddrs>, byte: u32) -> String {
    reg_name_all(a, g, m, s, None, byte)
}

/// `reg_name_sdma` with GC's CP and GFX tail registers (GX-7j).
pub fn reg_name_all(a: &Addrs, g: Option<&GfxInputs>, m: Option<&GmcInputs>, s: Option<&sdma::SdmaAddrs>, x: Option<&GfxAddrs>, byte: u32) -> String {
    if let Some(n) = s.and_then(|s| s.name_at(byte)) {
        return String::from(n);
    }
    if let Some(n) = x.and_then(|x| x.name_at(byte)) {
        return String::from(n);
    }
    if let Some(m) = m {
        for h in [m.addrs.gfx, m.addrs.mm] {
            if let Some(r) = h.reg_at(byte) {
                return r.name(h.hub);
            }
        }
    }
    if let Some(g) = g {
        let s = g.smu;
        for (b, n) in [(s.msg, "MP1_SMN_C2PMSG_66"), (s.arg, "MP1_SMN_C2PMSG_82"), (s.resp, "MP1_SMN_C2PMSG_90"), (s.smn_index, "BIF_BX0_PCIE_INDEX2")] {
            if b == byte {
                return String::from(n);
            }
        }
    }
    let named = [
        (a.c2pmsg_35, "MP0_SMN_C2PMSG_35"),
        (a.c2pmsg_64, "MP0_SMN_C2PMSG_64"),
        (a.c2pmsg_67, "MP0_SMN_C2PMSG_67"),
        (a.c2pmsg_69, "MP0_SMN_C2PMSG_69"),
        (a.c2pmsg_70, "MP0_SMN_C2PMSG_70"),
        (a.c2pmsg_71, "MP0_SMN_C2PMSG_71"),
        (a.c2pmsg_81, "MP0_SMN_C2PMSG_81"),
    ];
    named.iter().find(|(b, _)| *b == byte).map_or_else(|| format!("{byte:#x}"), |(_, n)| String::from(*n))
}

fn step_text(s: &Step) -> String {
    match s {
        Step::SosAlive(v) => format!("sOS alive (C2PMSG_81 {v:#x})"),
        Step::StaleRingDestroyed { size, wptr } => format!("stale ring destroyed (DESTROY_RINGS; it had size {size:#x}, write pointer {wptr:#x})"),
        Step::RingCreated { mc } => format!("ring created at MC {mc:#x}"),
        Step::StaleTmrDestroyed { status } => format!("stale TMR destroyed (DESTROY_TMR status {status:#x}{})", if *status == 0 { "" } else { ": tolerated, maybe no TMR was left" }),
        Step::Toc { tmr_size } => format!("LOAD_TOC: TMR size {tmr_size:#x}"),
        Step::Tmr { mc, size } => format!("SETUP_TMR: MC {mc:#x}, {size:#x} bytes"),
        Step::Load(id) => format!("LOAD_IP_FW {id:?} (type {})", id.fw_type()),
        Step::AutoloadRlc => String::from("AUTOLOAD_RLC"),
    }
}

/// The answer's lines (the tool adds the live parts: bus master, DCN snapshots). The `W` lines are the run's
/// writes in `regtrace diff --ours` format (`W <byte> <value>`, the read-back as a comment).
pub fn render(out: &Outcome, inp: &Inputs) -> Vec<String> {
    let mut v = Vec::new();
    v.push(format!(
        "run: until {}, start {}, prior {}, keep {}",
        match inp.until {
            Until::Ring => "ring (M1)",
            Until::Fw => "psp (M2)",
            Until::Gfx => "gfx (M3)",
            Until::Gart => "gart (M3 + GMC)",
            Until::Sdma => "sdma (M3 + GMC + SDMA, M4)",
            Until::GfxRing => "gfx_ring (M3 + GMC + GFX tail and ring 0, direct CP_RB0, M5)",
            Until::Mes => "mes (M3 + GMC + GFX tail, Linux's CP unhalt, MES: KIQ, scheduler, SET_HW_RESOURCES, self test)",
            Until::GfxMes => "gfx_mes (mes + a compute ring and GFX ring 0 mapped through MES, ring/IB/fence tests)",
            Until::Vm => "vm (sdma + gfx_mes, then a test VM in the rings: VMID, in-ring flush, PASID entry, an intended fault)",
        },
        inp.start.name(),
        out.report.prior.map_or("-", |p| if p == psp::Prior::Stale { "stale: destroy first" } else { "fresh" }),
        inp.keep
    ));
    v.push(format!("carve-out: MC {:#x}, CPU {:#x}; stale ring size {:#x}, write pointer {:#x}", inp.carve.mc_base, inp.carve.phys_base, out.report.stale_ring_size, out.report.stale_wptr));
    for (i, s) in out.report.steps.iter().enumerate() {
        let mut l = format!("step {}: {}", i + 1, step_text(s));
        if let Some(sn) = out.report.snaps.get(i) {
            for (n, val) in &sn.regs {
                l += &format!(" {n}={}", val.map_or(String::from("?"), |x| format!("{x:#x}")));
            }
            l += &format!(" fence={}", sn.fence.map_or(String::from("?"), |f| format!("{f}")));
        }
        v.push(l);
    }
    for l in &out.report.loaded {
        v.push(format!("loaded {:?} type {} from MC {:#x}, {:#x} bytes, in the TMR at {:#x}", l.id, l.fw_type, l.mc, l.size, l.tmr_addr));
    }
    for &(c, s) in &out.report.warnings {
        v.push(format!("tolerated: command {c:#x} answered {s:#x}"));
    }
    if let Some(g) = &out.gfx {
        v.extend(gfxup::render(g));
    }
    if let Some(g) = &out.gmc {
        v.extend(gmc::render(g));
    }
    if let Some(g) = &out.sdma {
        v.extend(sdma::render(g));
    }
    if let Some(g) = &out.gfx_ring {
        v.extend(gfxring::render(g));
    }
    v.extend(out.mes_pre.iter().cloned());
    if let Some(g) = &out.mes {
        v.extend(mes::render(g));
    }
    if let Some(g) = &out.vm {
        v.extend(vmring::render(g));
    }
    for c in &out.checks {
        v.push(format!("{}: {} ({})", c.name, if c.pass { "PASS" } else { "FAIL" }, c.why));
    }
    if let Some(e) = &out.error {
        v.push(format!("error: {e:?} (errno {}){}", e.errno(), advice(e)));
    }
    v.push(match (&out.undo, &out.kept) {
        (_, Some(_)) if out.held_for_gc && out.gc_failed() => String::from(
            "undo: none: EnableGfxImu reached PMFW and GC did not boot, so TMR and ring stay (no DESTROY_TMR under a possibly running RLC/IMU); GC unsettled: reboot (the Quiesce hook leaves them too; until=stop destroys them on the desk's word)"
        ),
        (_, Some(_)) if out.held_for_gc => String::from(
            "undo: none: EnableGfxImu reached PMFW, so TMR and ring stay as Linux keeps them after its autoload (until=stop frees them; the Quiesce hook at Reload/reboot)"
        ),
        (_, Some(_)) => String::from("undo: none, the ring is kept (the Quiesce hook destroys TMR and ring at Reload/reboot)"),
        (_, _) if out.quarantined.is_some() => format!(
            "undo: INCOMPLETE{}: the PSP may still hold its buffers, they stay out of the BO manager; reboot",
            match &out.undo {
                Some(Err(e)) => format!(" ({e:?})"),
                _ => String::new(),
            }
        ),
        (Some(Ok(())), _) => String::from("undo: done (DESTROY_TMR if a TMR was set, DESTROY_RINGS)"),
        (Some(Err(e)), _) => format!("undo: FAILED {e:?}"),
        (None, None) if out.error.is_some() => String::from("undo: done inside the failed start (or nothing was created)"),
        (None, None) => String::from("undo: nothing to undo"),
    });
    if out.gfx.as_ref().is_some_and(|g| g.error.is_none()) {
        v.push(String::from("GC: stays on and booted (GFXOFF not allowed); TMR and ring stay with it"));
    } else if out.gc_failed() {
        v.push(String::from("GC: NOT booted; EnableGfxImu reached PMFW, so the RLC/IMU may still run: nothing undone; reboot (mode 2 is refused on 13.0.4)"));
    }
    v.push(format!("watch: {} writes to display registers or the scanout blocked", out.violations_total));
    for x in &out.violations {
        v.push(format!("watch blocked: {:?} = {:#x} ({:?})", x.at, x.value, x.class));
    }
    let bad = out.entries.iter().filter(|e| e.outcome != WriteOutcome::Ok).count();
    v.push(format!("owner: {} writes in this run ({bad} not Ok), {} in this owner's life", out.entries.len(), out.totals.writes));
    let sa = inp.sdma.as_ref().map(|s| s.addrs);
    let xa = inp.gfx_ring.as_ref().map(|x| x.addrs);
    for (i, e) in out.entries.iter().enumerate() {
        if out.undo_from == Some(i) {
            v.push(String::from("# undo (leave these out for regtrace diff)"));
        }
        v.extend(sdma_marks(&out.sdma_resumes, i));
        v.extend(gfx_mark_lines(&out.gfx_marks, i));
        v.push(w_line(e, &reg_name_all(&inp.addrs, inp.gfx.as_ref(), inp.gmc.as_ref(), sa.as_ref(), xa.as_ref(), e.byte)));
    }
    v.extend(sdma_marks(&out.sdma_resumes, out.entries.len()));
    v.extend(gfx_mark_lines(&out.gfx_marks, out.entries.len()));
    if out.passed() && out.gfx.as_ref().is_some_and(|g| g.booted_before()) {
        v.push(String::from("M3 not proven: GC was booted before EnableGfxImu; this PASS shows the load path keeps it booted, not that our firmware booted it (that takes a cold start)"));
    }
    v.push(format!("verdict: {}", if out.passed() { "PASS" } else { "FAIL" }));
    v
}

/// One write as `regtrace diff --ours` reads it (the read-back as a comment).
pub fn w_line(e: &Entry, name: &str) -> String {
    format!("W {:#x} {:#x} # {} read {} {:?}", e.byte, e.value, name, e.read_back.map_or(String::from("-"), |r| format!("{r:#x}")), e.outcome)
}

/// GX-7i: comment lines around each SDMA resume among the `W` lines (`resumes`: doorbell, first and end index).
pub fn sdma_marks(resumes: &[(bool, usize, usize)], i: usize) -> Vec<String> {
    let mut v = Vec::new();
    for (k, &(db, a, b)) in resumes.iter().enumerate() {
        if b == i && a != b {
            v.push(format!("# sdma resume {} end", k + 1));
        }
        if a == i {
            v.push(format!(
                "# sdma resume {} ({}): regtrace diff's phase sdma compares it with Linux's start",
                k + 1,
                if db { "doorbell on, as Linux" } else { "doorbell off: DOORBELL and DOORBELL_OFFSET differ from Linux by design" }
            ));
        }
    }
    v
}

/// GX-7j: comment lines around the GFX tail and resume among the `W` lines (name, first and end index).
pub fn gfx_mark_lines(marks: &[(&'static str, usize, usize)], i: usize) -> Vec<String> {
    let mut v = Vec::new();
    for &(n, a, b) in marks {
        if b == i && a != b {
            v.push(format!("# {n} end"));
        }
        if a == i {
            v.push(format!("# {n}: regtrace diff's phase {} compares it with Linux", match n {
                "gfx tail" => "gfx_tail",
                "mes start" => "mes (CP_MES_CNTL through the KIQ's CP_HQD_ACTIVE: the capture's 29 writes)",
                _ => "gfx_ring (direct CP_RB0: the CP_RB0 writes are extra by design, Linux 7.1 maps the ring through MES)",
            }));
        }
    }
    v
}

/// What the desk does next for this error.
pub fn advice(e: &PspError) -> &'static str {
    match e {
        PspError::Timeout { what: "ring.ready", .. } => ": C2PMSG_64 never got ready: reboot, do not retry (gx-roadmap wave 24)",
        PspError::Timeout { what: "fence", .. } | PspError::Wedged => ": the PSP may still run that frame: reboot before another run",
        PspError::RingNotReset { .. } => ": the write pointer survived create: report it (gx-GX-7d open point)",
        PspError::CmdStatus { .. } => ": the PSP refused a command: stopped and undone",
        _ => "",
    }
}
