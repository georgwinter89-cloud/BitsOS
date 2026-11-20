//! GP-GPU2: the card's bring-up stages, each one MCP call on ONE device (`gx_bringup {"device":..,"stage":..}`),
//! bounded, ending in one plain German line and the technical lines, behind the bring-up owner (every write logged,
//! `gpucore::bringup`) and the stage's own write list (`guard`). The chain is Linux's order of blocks
//! (`amdgpu_device_ip_init`, amdgpu_device.c:2369-2480: common, GMC, IH, then the secure processor with
//! `psp_load_fw`, amdgpu_psp.c:3595-3660, for 14.0.3), which is the order of the trace's sections nbif, mmhub, ih,
//! psp-boot and psp-ring (`linuxseq`):
//!
//! | stage | Linux | writes |
//! |---|---|---|
//! | nbif, gmc, ih | wave 42, `nb`: the three blocks Linux starts before the secure processor | `nb`'s table |
//! | sos | the nine bootloader loads of `psp_hw_start` amdgpu_psp.c:2961-3048 (`stage_sos`) | C2PMSG_36/35 through the gate, HDP flush; the window |
//! | ring | `psp_ring_create` psp_v14_0.c:298-336, then `psp_update_fw_reservation` amdgpu_psp.c:1065 | C2PMSG_69/70/71/64, 67, HDP flush; the window |
//! | toc | `psp_load_toc` :861-885 | 67, HDP flush; the window |
//! | tmr | `psp_tmr_init`/`psp_tmr_load` :887-962: 14.0.3 keeps a boot-time TMR, nothing is sent | none |
//! | fw | `psp_load_non_psp_fw` :3515-3593 up to RLC_G, the SMU's firmware first | 67, HDP flush; the window; on a second ring with GFXHUB on, that hub's switch-off first (`gcstage::before_fw`) |
//! | autoload | `psp_rlc_autoload_start` :3902-3915 | 67, HDP flush; the window |
//! | fwrest | the rest of `psp_load_non_psp_fw`'s walk: VCN, DMCUB | 67, HDP flush; the window |
//! | ta | `psp_asd_initialize` :1681, `psp_rl_load` :1177, RAS :2368, HDCP :2550, DTM :2624 | 67, HDP flush; the window |
//! | pmfw | wave 43: `smu_hw_init` ../pm/swsmu/amdgpu_smu.c:1955-2021, the power controller's start (`stage_pmfw`) | the index pair's address, the SMU's mailbox through its gate, the thermal alert's four registers |
//! | gcboot, gchub | wave 43, `gcstage`: the graphics block's start and its memory hub (`gfx_v12_0_hw_init` gfx_v12_0.c:3767-3818) | `gcstage`'s table |
//! | gccp | wave 44, `cpstage`: the command processor's resume (`gfx_v12_0_hw_init` :3835-3850) | `cpstage`'s table |
//! | mes, mesq | wave 44, `messtage`, `meskq`: `mes_v12_0_kiq_hw_init` mes_v12_0.c:1956 with `mes_v12_0_hw_init` :2036, then the kernel queues mapped through MES (gfx_v12_0.c:3566-3575) | `messtage::allow` |
//! | gcring | wave 44, `ringstage`: `gfx_v12_0_cp_gfx_start` :2699, the five ring tests (:3571-3583), the fault interrupts (:3852-3862) | `ringstage::allow` |
//! | sdma | wave 43, `sdmastage`: `sdma_v7_0_hw_init` sdma_v7_0.c:1409, both engines with their ring tests | `sdmastage::allow` |
//! | destroy | `psp_v14_0_ring_destroy` psp_v14_0.c:340-356 | C2PMSG_64 |
//!
//! A stage of the chain runs only when the one before it passed in this boot. That is checked in one place for
//! all of them (`in_order`, asked by `run` before any access, with `Session::passed`), so a stage added to
//! `Stage::CHAIN` is held to its place by being there; the check crosses the blocks (`sos` asks for `ih`, which
//! stands on `gmc`, which stands on `nbif`), and the fake (`fakemodel`) refuses the same out-of-order steps on its
//! side. What a stage refuses besides is its own: a second run, a run that stopped half-way. Outside the chain, at
//! any time and needed by no stage, are the looks, which follow no sequence of Linux's:
//!
//! | look | what | writes |
//! |---|---|---|
//! | alive | sign of life, mailbox, the ring's registers, the HDP flush's remap as the card holds it | none |
//! | bootloader | `psp_v14_0_wait_for_bootloader` psp_v14_0.c:106-125 alone | none |
//! | smu | `smu_v14_0_check_fw_status` + `smu_cmn_get_smc_version` + read-only questions | index pair, SMU mailbox |
//! | state | what this boot did so far, and the blocks' registers as they read now | none |
//! | ringtest | wave 44, `ringrungs`: the end-of-pipe interrupts on, Linux's IB test on each kernel ring, a fence's interrupt through the IH reader, a compute queue unmapped and mapped again through MES; needs `gcring`; held against Linux's forms (`ringseq`) | `ringstage::allow` |
//! | sdmatest | wave 43, `sdmarungs`: Linux's IB test on both engines, then BitsOS's own rungs (fill, copy, trap, system pages); needs `sdma`; the rungs follow no section of the trace, its two bind flushes are held against sw-init's (`sdmaseq`) | `sdmastage::allow` |
//!
//! The stages' names, the chain and its order check are `stages`. The stages' bodies: `nbifstage`, `gmcstage`,
//! `ihstage` (over `nb`), `stage_sos`, `stage_ring` (ring, toc, tmr, destroy), `stage_fw` (fw, autoload, fwrest,
//! ta), `stage_pmfw` (pmfw), `gcstage` (gcboot, gchub), `cpstage` (gccp), `sdmastage` and `sdmarungs` (sdma, sdmatest), `stage_ro`
//! (the looks).

use super::attach::{Attached, PciFacts};
use super::chainseq::Chosen;
use super::gcstage;
use super::guard::{Allow, Guard, HdpFlush, Mc, Refusal};
use super::host::Host;
use super::nb;
use super::nbregs::NbRegs;
use super::regs14::Regs14;
use super::{messtage, sdmastage};

pub use super::session::{Session, SessionSlot, Sos, TaLoaded};
pub(super) use super::stage_util::{Stop, need_pri, need_ring, psp_stop, read, read_file, ring_regs, smu_stop, stop};
use super::vram::Layout;
use crate::attach::Gop;
use crate::gpucore::bringup::{Bringup, Gate, LOG_KEEP, Totals};
use crate::gpucore::trail::Access;
use crate::gpucore::fwload::FwSource;
use crate::gpucore::handoff::StopNote;
use crate::gpucore::psp::{Hw, Timeouts};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::bootseq;

pub use super::stages::{Stage, in_order};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opts {
    pub stage: Stage,
}

/// A stage's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    pub stage: Stage,
    pub pass: bool,
    /// The one plain line (German, no code words).
    pub line: String,
    pub lines: Vec<String>,
    pub totals: Totals,
    pub refused: Vec<Refusal>,
    /// The note to hand on (a ring change); the caller writes it under the card's tag.
    pub note: Option<StopNote>,
    /// GX-10: every register access the card saw from this stage, in order (`rdna4::linuxseq` compares it with
    /// Linux's trace: `chainseq::verdict` in every answer's `linuxseq` line, the tests in full), and how many of
    /// them the owner could not keep (0, or the trail proves nothing).
    pub trail: Vec<Access>,
    pub trail_lost: u64,
    /// What the call's `linuxseq` line says, typed (`None`: a look, the destroy, a call refused before any
    /// access). The line itself is among `lines`.
    pub seq: Option<super::chainseq::Verdict>,
}

/// What the stage needs besides the session and the card.
pub struct Ctx<'a> {
    pub index: usize,
    pub display: bool,
    pub facts: &'a PciFacts,
    pub att: &'a Attached,
    pub gop: Option<Gop>,
    /// The note the kernel before handed on for this card (`handoff::NoteIo::take_note_for`).
    pub inherited: Option<StopNote>,
    pub t: Timeouts,
}

/// What a stage may write: its registers and, for the stages that hand the secure processor memory, the window.
/// The bootloader's two registers are on `sos`'s list and pass only as its gate lets them (`guard`).
pub fn allow(stage: Stage, r: &Regs14, l: Option<&Layout>) -> Allow {
    let p = &r.psp;
    let window = || l.map(|l| alloc::vec![l.window_mc()]).unwrap_or_default();
    let (regs, mem) = match stage {
        // `nb::allow` lists the three stages of wave 42, `gcstage::allow` the graphics block's, `sdmastage::allow`
        // the engines' two.
        Stage::Nbif | Stage::Gmc | Stage::Ih | Stage::GcBoot | Stage::GcHub | Stage::GcCp | Stage::Mes | Stage::MesQ | Stage::GcRing | Stage::Sdma | Stage::SdmaTest | Stage::RingTest | Stage::Alive | Stage::Bootloader | Stage::Tmr | Stage::State => (Vec::new(), Vec::new()),
        Stage::Smu => (alloc::vec![r.rsmu_index, r.smu_answer, r.smu_argument, r.smu_message], Vec::new()),
        // The address, the command, and the flush that carries the firmware buffer's bytes to the card.
        Stage::Sos => (alloc::vec![r.c2pmsg_36, p.c2pmsg_35, r.hdp_flush], window()),
        // Ring: create, then two frames (C2PMSG_67) for the reserved-region questions; the window's ring, fence
        // and command pages. DESTROY_RINGS only when a check after create fails.
        Stage::Ring => (alloc::vec![p.c2pmsg_69, p.c2pmsg_70, p.c2pmsg_71, p.c2pmsg_64, p.c2pmsg_67, r.hdp_flush], window()),
        Stage::Toc | Stage::Fw | Stage::Autoload | Stage::FwRest | Stage::Ta => (alloc::vec![p.c2pmsg_67, r.hdp_flush], window()),
        Stage::Destroy => (alloc::vec![p.c2pmsg_64], Vec::new()),
        Stage::Pmfw => return super::stage_pmfw::allow(r),
    };
    Allow { stage: stage.name(), regs, mem }
}

/// Linux's name of a register for an answer line.
fn name_of(r: &Regs14, byte: u32) -> &'static str {
    r.reg_at(byte).map(|x| x.name).or_else(|| super::linuxseq::name_of(byte / 4)).unwrap_or("?")
}

/// Runs one stage on `hw` (the card's registers by BAR5 byte, VRAM by offset, and its host side).
pub fn run<H: Hw + Host>(s: &mut Session, o: Opts, c: &Ctx, hw: H, files: &mut dyn FwSource) -> (Done, H) {
    let both = Regs14::from(&c.att.discovery).and_then(|r| NbRegs::from(&c.att.discovery, &r).map(|n| (r, n)));
    let Some((regs, nbregs)) = both else {
        let d = finish(o.stage, c, Vec::new(), Err(stop("Bauteilliste nennt die Register nicht", "MP0/MP1/NBIF/MMHUB/OSSSYS/GC bases missing or past BAR5")), Totals::default(), Vec::new(), None);
        return (d, hw);
    };
    let mut hw = hw;
    // Before any access, `gmc`'s planning reads included. The second card is told that it stays untouched instead
    // (`step`).
    if c.display && let Err(e) = in_order(s, o.stage) {
        return (finish(o.stage, c, Vec::new(), Err(e), Totals::default(), Vec::new(), None), hw);
    }
    let (mut trail, mut lost) = (Vec::new(), 0);
    let mut lines = Vec::new();
    // `gmc` plans from reads before the owner exists, so its guard knows what was planned (the GART table and the
    // default page; with them the session's layout and its VRAM manager, which every later stage uses). The reads
    // go through an owner of their own with its gate shut, so they are in the stage's trail like the rest.
    if c.display && o.stage == Stage::Gmc {
        let mut reader = Bringup::new(hw, Gate::Refuse("the stage plans from reads"), 1).with_trail(LOG_KEEP);
        let mut io = nb::Io { card: &mut reader, n: &nbregs, flush: None, driven: c.display };
        let planned = super::gmcstage::plan(&mut s.nb, &mut io, c.facts, c.gop, c.att.vram_bytes, &mut s.layout, &mut s.bump, &mut lines);
        trail.extend(reader.trail().into_iter().flat_map(|t| t.recent().copied()));
        lost += reader.trail().map_or(0, |t| t.lost());
        hw = reader.into_hw();
        if let Err(e) = planned {
            let chosen = Chosen { l: s.layout.as_ref(), taken: s.bump.as_ref(), ..Chosen::default() };
            let seq = super::chainseq::judge(&mut s.seq, o.stage, false, &trail, lost, &regs, c.att.vram_bytes, chosen);
            lines.extend(seq.as_ref().map(|v| v.line.clone()));
            let mut d = finish(o.stage, c, lines, Err(e), Totals::default(), Vec::new(), None);
            (d.trail, d.trail_lost, d.seq) = (trail, lost, seq);
            return (d, hw);
        }
    }
    let (base, bytes) = s.layout.as_ref().map_or((0, 0), |l| (l.mc_base, l.vram_bytes));
    // Only the stage that loads the secure OS gets the card's bootloader gate; it exists once the window is known.
    // After a Reload that follows a kernel which sent to this bootloader it is a gate that lets nothing through.
    let lend = o.stage == Stage::Sos && c.display;
    if lend && s.boot.is_none() {
        let gate = if s.boot_before.is_some_and(|n| n.spent()) { bootseq::Gate::spent } else { bootseq::Gate::new };
        s.boot = s.layout.as_ref().map(|l| gate(l.window_mc()));
    }
    let gate = if lend { s.boot.take() } else { None };
    // The stage that starts the power controller gets the SMU's gate (`smugate`): its own copy of Linux's
    // sequence, moved by the bus alone.
    let smu_gate = if o.stage == Stage::Pmfw && c.display { super::stage_pmfw::gate(s, c) } else { None };
    let sdma = super::sdmaregs::SdmaRegs::from(&c.att.discovery);
    if super::cpstage::is_mine(o.stage) && c.display {
        super::cpstage::prepare(s, &c.att.discovery);
    }
    let list = if nb::is_nb(o.stage) {
        nb::allow(o.stage, &s.nb, &nbregs, |r| base + r.start..base + r.end)
    } else if gcstage::is_gc(o.stage) {
        gcstage::allow(o.stage, &s.nb, &nbregs)
    } else if super::cpstage::is_mine(o.stage) {
        super::cpstage::allow(s, &nbregs)
    } else if sdmastage::is_mine(o.stage) && c.display {
        sdmastage::allow(o.stage, s, &nbregs, sdma.as_ref())
    } else if messtage::is_mine(o.stage) && c.display {
        messtage::allow(o.stage, s, &nbregs)
    } else if super::ringstage::is_mine(o.stage) && c.display {
        super::ringstage::allow(o.stage, s, &nbregs)
    } else {
        let mut list = allow(o.stage, &regs, s.layout.as_ref());
        list.regs.extend(gcstage::off_regs(o.stage, &s.gc, &nbregs));
        list
    };
    // As Linux: no read-back after a write (a bus access Linux never makes), and every access kept in order.
    let guard = Guard::new(Mc { hw, base, bytes }, list, regs.clone()).with_index_rules(nb::index_rules(o.stage, &nbregs)).with_host(nb::host(o.stage)).with_gate(gate).with_smu(smu_gate);
    let guard = if c.display { super::ringstage::guarded(messtage::guarded(sdmastage::guarded(guard, o.stage, s), o.stage, s), o.stage, s) } else { guard };
    let mut owner = Bringup::new(guard, Gate::Allow, 4096).with_trail(LOG_KEEP).without_read_back();
    let before = s.note;
    let r = if nb::is_nb(o.stage) && c.display {
        let mut io = nb::Io { card: &mut owner, n: &nbregs, flush: s.flush_regs(&nbregs), driven: c.display };
        match o.stage {
            Stage::Nbif => super::nbifstage::run(&mut s.nb, &mut io, &mut lines),
            Stage::Gmc => super::gmcstage::run(&mut s.nb, &mut io, &mut lines),
            _ => super::ihstage::run(&mut s.nb, &mut io, &mut lines),
        }
        .map(|_| None)
    } else if gcstage::is_gc(o.stage) && c.display {
        let mut io = nb::Io { card: &mut owner, n: &nbregs, flush: gcstage::flush_regs(o.stage, s.flush_regs(&nbregs), &nbregs), driven: c.display };
        gcstage::run(&mut s.gc, &s.nb, o.stage, &mut io, c, files, &mut lines).map(|_| None)
    } else if super::cpstage::is_mine(o.stage) && c.display {
        let mut io = nb::Io { card: &mut owner, n: &nbregs, flush: s.flush_regs(&nbregs), driven: c.display };
        super::cpstage::run(s, &mut io, c, &mut lines).map(|_| None)
    } else if sdmastage::is_mine(o.stage) && c.display {
        // The engines' stages take the card with its host side too (system pages), and bind through the GART:
        // with the flush of this boot's hubs, both once GFXHUB is on (`Session::flush_regs`).
        let mut io = nb::Io { card: &mut owner, n: &nbregs, flush: s.flush_regs(&nbregs), driven: c.display };
        sdmastage::run(s, o.stage, c, &mut io, files, &mut lines).map(|_| None)
    } else if messtage::is_mine(o.stage) && c.display {
        // MES's stages take the card with its host side (system pages, the doorbells): `mes` binds with the
        // register form of the flush, `mesq` makes only the HDP flushes of its own.
        let mut io = nb::Io { card: &mut owner, n: &nbregs, flush: s.flush_regs(&nbregs), driven: c.display };
        messtage::run(s, o.stage, c, &mut io, files, &mut lines).map(|_| None)
    } else if super::ringstage::is_mine(o.stage) && c.display {
        // The rings' stages write the kernel queues' rings and slots in MES's piece and ring their doorbells; the
        // rungs bind their IB page with the flush through MES.
        let mut io = nb::Io { card: &mut owner, n: &nbregs, flush: s.flush_regs(&nbregs), driven: c.display };
        if o.stage == Stage::GcRing { super::ringstage::run(s, &mut io, &mut lines) } else { super::ringrungs::run(s, c, &mut io, &mut lines) }.map(|_| None)
    } else {
        // Every stage of the secure processor flushes at Linux's remapped place: none of them runs before `ih`.
        step(s, o, c, &regs, &mut HdpFlush { hw: &mut owner, flush: regs.hdp_flush, memsize: regs.memsize }, files, &mut lines)
    };
    if o.stage == Stage::State {
        lines.push(nb::state_line(&nbregs, &mut owner));
        lines.extend(gcstage::state_line(&s.gc, &nbregs, &mut owner));
        lines.extend(super::cpstage::state_line(&s.cp, &nbregs, &mut owner));
        // The engines' registers only once the stage `sdma` wrote them in this boot: before that the GC block
        // may be without power, and a look reads nothing of a block Linux has not touched yet.
        lines.extend(sdma.as_ref().filter(|_| s.sdma.tried).map(|sr| super::sdmaregs::state_line(sr, &mut owner)));
    }
    // What a stop of the power controller's start leaves to do is not Linux's cold start: its accesses are in
    // the trail and outside the comparison.
    let compared = trail.len() + owner.trail().map_or(0, |t| t.recent().count());
    if o.stage == Stage::Pmfw && r.is_err() {
        super::stage_pmfw::after_stop(s, &mut owner, &regs, &mut lines);
    }
    if messtage::is_mine(o.stage) && r.is_err() && compared > 0 {
        messtage::after_stop(&mut owner, &nbregs, &mut lines);
    }
    if o.stage == Stage::GcRing && r.is_err() && compared > 0 {
        super::ringstage::after_stop(&mut owner, &nbregs, &mut lines);
    }
    let totals = owner.log().totals();
    for e in owner.log().recent() {
        lines.push(format!("W {:#07x} {:#010x} # {} read {} {:?}", e.byte, e.value, name_of(&regs, e.byte), e.read_back.map_or(String::from("-"), |v| format!("{v:#010x}")), e.outcome));
    }
    // A ring change is handed on also when the stage failed after it (a ring left up).
    let note = r.as_ref().ok().and_then(|n| *n).or(s.note.filter(|n| Some(*n) != before));
    // Each poll with what it took: the one thing a fake cannot show and the hardware round compares with Linux's
    // times (plan/spec/gpu-rdna4.md section 7).
    for a in owner.trail().into_iter().flat_map(|t| t.recent()) {
        if let Access::Wait { byte, cond, reads, last, us, .. } = *a {
            lines.push(format!("WAIT {byte:#07x} # {} until {cond}: {reads} reads in {us} us, last {}", name_of(&regs, byte), last.map_or(String::from("-"), |v| format!("{v:#010x}"))));
        }
    }
    lost += owner.trail().map_or(0, |t| t.lost());
    trail.extend(owner.trail().into_iter().flat_map(|t| t.recent().copied()));
    let mut guard = owner.into_hw();
    if lend {
        s.boot = guard.boot.take();
    }
    // What the card saw from this call against Linux's trace: the last technical line of the answer, on the real
    // card as on the fake (plan/spec/gpu-rdna4.md section 6).
    let told = s.nb.gart.as_ref().map(gcstage::Told::of);
    let irq = sdmastage::irq_regs(&nbregs);
    let chosen = Chosen { l: s.layout.as_ref(), taken: s.bump.as_ref(), told: told.as_ref(), irq: &irq, mes: s.mes.kiq, sdma: s.sdma.bound };
    let seq = super::chainseq::judge(&mut s.seq, o.stage, r.is_ok(), &trail[..compared.min(trail.len())], lost, &regs, c.att.vram_bytes, chosen);
    lines.extend(seq.as_ref().map(|v| v.line.clone()));
    if trail.len() > compared {
        let what = match o.stage {
            Stage::Pmfw => "what Linux's err_out powers down again, the read-back of the running features",
            Stage::GcRing => "the look at a hung engine after the stop",
            _ => "the selection put back after MES's stop",
        };
        lines.push(format!("linuxseq: {} accesses after the stop ({what}) are not compared", trail.len() - compared));
    }
    let mut d = finish(o.stage, c, lines, r.map(|_| ()), totals, guard.refused, note);
    (d.trail, d.trail_lost, d.seq) = (trail, lost, seq);
    (d, guard.hw.hw)
}

/// `lines` are what the stage collected (its own lines, then the guard's writes); a stop keeps them and adds
/// `stopped: <detail>` as the last line, so what the stage saw before it stopped reaches the answer.
fn finish(stage: Stage, c: &Ctx, mut lines: Vec<String>, r: Result<(), Stop>, totals: Totals, refused: Vec<Refusal>, note: Option<StopNote>) -> Done {
    let card = if c.display { format!("Grafikkarte {} (zeigt das Bild)", c.index + 1) } else { format!("Grafikkarte {}", c.index + 1) };
    match r {
        Ok(()) => Done { stage, pass: true, line: format!("{card}: {}: fertig.", stage.german()), lines, totals, refused, note, trail: Vec::new(), trail_lost: 0, seq: None },
        Err(Stop(plain, detail)) => {
            klog::event!(Warn, "gx", "rdna4.stage.stopped", stage = stage.name(), why = detail.as_str(); "the stage stopped by itself");
            lines.push(format!("stopped: {detail}"));
            Done { stage, pass: false, line: format!("{card}: {}: angehalten: {plain}.", stage.german()), lines, totals, refused, note, trail: Vec::new(), trail_lost: 0, seq: None }
        }
    }
}

pub(super) type Step = Result<Option<StopNote>, Stop>;

fn step(s: &mut Session, o: Opts, c: &Ctx, r: &Regs14, hw: &mut dyn Hw, files: &mut dyn FwSource, out: &mut Vec<String>) -> Step {
    use super::{stage_fw as fw, stage_ring as ring, stage_ro as ro};
    if !c.display && o.stage != Stage::State {
        // plan/spec/gpu-rdna4.md section 5: the second card stays read-only until Georg asks.
        return Err(stop("die zweite Karte bleibt unberührt, bis Georg es anders sagt", "not the display card: read-only (R1)"));
    }
    match o.stage {
        // Wave 42's three, the graphics block's and the engines' run from `run` itself (they take the card with
        // its host side).
        Stage::Nbif | Stage::Gmc | Stage::Ih | Stage::GcBoot | Stage::GcHub | Stage::GcCp | Stage::Mes | Stage::MesQ | Stage::GcRing | Stage::Sdma | Stage::SdmaTest | Stage::RingTest => Err(stop("Reihenfolge stimmt nicht", "a stage of rdna4::nb, rdna4::gcstage, rdna4::cpstage, rdna4::messtage, rdna4::ringstage or rdna4::sdmastage does not run here")),
        Stage::State => ro::state(s, hw, r, out).map(|_| None),
        Stage::Alive => ro::alive(s, hw, r, c, out).map(|_| None),
        Stage::Bootloader => ro::bootloader(hw, r, out).map(|_| None),
        Stage::Smu => ro::smu(s, hw, r, out).map(|_| None),
        Stage::Sos => super::stage_sos::sos(s, c, hw, r, files, out).map(|_| None),
        Stage::Ring => ring::ring(s, c, hw, r, files, out),
        Stage::Toc => ring::toc(s, c, hw, files, out).map(|_| None),
        Stage::Tmr => ring::tmr(s, out).map(|_| None),
        Stage::Fw => fw::fw(s, c, hw, files, out).map(|_| None),
        Stage::Autoload => fw::autoload(s, hw, out).map(|_| None),
        Stage::FwRest => fw::fwrest(s, hw, out).map(|_| None),
        Stage::Ta => fw::ta(s, c, hw, files, out).map(|_| None),
        Stage::Pmfw => super::stage_pmfw::pmfw(s, c, hw, r, out).map(|_| None),
        Stage::Destroy => ring::destroy(s, hw, r, out).map(Some),
    }
}
