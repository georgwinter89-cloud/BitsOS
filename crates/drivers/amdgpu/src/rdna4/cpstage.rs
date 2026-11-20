//! Wave 44, item CP (piece 13): the stage `gccp`, the command processor's resume as Linux's
//! `gfx_v12_0_hw_init` makes it after GFXHUB's start (gfx_v12_0.c:3835-3850), the trace's section gc-cp
//! (init.txt lines 9362-9501), in Linux's order:
//!
//! | step | Linux | writes |
//! |---|---|---|
//! | constants | `gfx_v12_0_constants_init` :1824-1857 | GRBM_CNTL, GRBM_GFX_INDEX per active shader array, DB_RING_CONTROL, per VMID GRBM_GFX_CNTL, SH_MEM_CONFIG, SH_MEM_BASES, SPI_GDBG_PER_VMID_CNTL |
//! | doorbell entries | `nbif_v6_3_1_gc_doorbell_init` nbif_v6_3_1.c:300-309 | NBIF's entries 0 (GFX) and 3 (MES) |
//! | rlc resume | `gfx_v12_0_rlc_resume` :2104-2113: `init_csb` :1938-1948, `rlc_enable_srm` :1999-2006 | the clear-state buffer in VRAM, then RLC_CSIB_ADDR_HI, _LO, _LENGTH; RLC_SRM_CNTL |
//! | cp resume | `gfx_v12_0_cp_resume` :3524-3550 | CP_INT_CNTL_RING0, the four doorbell ranges, the unhalt of MEC (with Linux's `udelay(50)`), of PFP and ME |
//! | the KIQ's place | `amdgpu_mes_kiq_hw_init` :3550, MES's `mes_v12_0_kiq_setting` mes_v12_0.c:1944-1953 | RLC_CP_SCHEDULERS |
//!
//! The last step is MES's code, reached from `cp_resume` before MES's own registers (the trace's section mes starts
//! at the next access, init.txt 9502): the stage makes it, and MES finds the KIQ's place set. After it Linux's
//! `cp_resume` goes on into MES (`mes`) and the rings (`gc-ring`), which are later stages.
//!
//! Linux's wait for CP_STAT after the graphics unhalt logs a timeout and goes on (`DRM_ERROR("failed to unhalt cp
//! gfx")`, `return 0`, :2371-2382): so do the stage's accesses, the KIQ's place included, but the stage then
//! stops instead of passing. Linux's trace never shows a CP that did not settle, every later stage would stand on
//! a state Linux's driver did not make, and the card has no watchdog: the stop keeps MES, the rings and SDMA off
//! the card, and the Reload hook halts the engines. The clear-state buffer lies in VRAM as Linux's did (MC 0x83_daafc000), a page the card's VRAM
//! manager hands out once per boot and never takes back (Linux keeps it until its fini); its address is the
//! section's one declared rule (`cpseq`). No HDP flush lies between the buffer's bytes and its registers, as in
//! Linux's trace: the RLC reads the buffer later, at a save-restore.

use super::guard::Allow;
use super::nb::Io;
use super::nbregs::NbRegs;
use super::sdmaoff::Written;
use super::session::Session;
use super::stage::{Ctx, Stage, Stop, psp_stop, stop};
use super::vram::Layout;
use crate::gpucore::disc::Discovery;
use crate::gpucore::psp::{self, Cond, LINUX_POLLS};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use rdna4_core::gc12cp::{self as cp, Bus, Config, Learned};
use rdna4_core::hubseq::Op;
use rdna4_core::regmap::Reg;
use rdna4_core::{clearstate12, doorbell12 as db, gc12, pte12};

/// What the VRAM manager's record names the clear-state buffer.
pub const CSB: &str = "the clear-state buffer";

/// The command processor of one card in this boot.
#[derive(Clone, Debug, Default)]
pub struct State {
    /// `gccp` came to its end: the engines are unhalted and the KIQ's place is set.
    pub passed: bool,
    /// `gccp` set out to write the block in this boot (it does not run again: the engines may be unhalted).
    pub tried: bool,
    /// The clear-state buffer's place (VRAM offsets), taken at the first `gccp` of a boot and never given back.
    pub csb: Option<Range<u64>>,
    /// What the constants learned.
    pub learned: Option<Learned>,
    /// CP_STAT after the graphics unhalt: `Ok(us)` it read 0 after that long, `Err(last)` Linux's loop ran out.
    pub cp_stat: Option<Result<u32, u32>>,
    /// The Reload hook's word (`livecp`), set together with `tried`; none on a card with no hook (the fake).
    pub reload: Option<Written>,
}

impl State {
    pub const fn new() -> State {
        State { passed: false, tried: false, csb: None, learned: None, cp_stat: None, reload: None }
    }

    pub fn describe(&self) -> String {
        match (self.passed, self.tried, self.cp_stat) {
            (true, _, _) => String::from("command processor unhalted (CP_STAT 0)"),
            (false, true, Some(Err(last))) => format!("command processor unhalted, CP_STAT still {last:#x} after Linux's loop: gccp stopped (it does not run again in this boot)"),
            (false, true, _) => String::from("command processor stopped half-way (gccp does not run again in this boot)"),
            (false, false, _) => String::from("command processor halted"),
        }
    }
}

pub fn is_mine(stage: Stage) -> bool {
    stage == Stage::GcCp
}

/// The shader-engine layout from the discovery table's GC info, as Linux reads it (amdgpu_discovery.c:2016-2022
/// for version 1: CUs per array are twice the WGPs; :2063-2066 for version 2). `None`: no table, or a version
/// whose fields Linux reads otherwise.
pub fn config(d: &Discovery) -> Option<Config> {
    let g = d.gc.as_ref()?;
    let get = |n| g.get(n);
    match g.major {
        1 => Some(Config { shader_engines: get("gc_num_se")?, sa_per_se: get("gc_num_sa_per_se")?, rb_per_se: get("gc_num_rb_per_se")?, cu_per_sa: 2 * (get("gc_num_wgp0_per_sa")? + get("gc_num_wgp1_per_sa")?) }),
        2 => Some(Config { shader_engines: get("gc_num_se")?, sa_per_se: get("gc_num_sh_per_se")?, rb_per_se: get("gc_num_rb_per_se")?, cu_per_sa: get("gc_num_cu_per_sh")? }),
        _ => None,
    }
    .filter(|c| (1..=16).contains(&c.shader_engines) && (1..=4).contains(&c.sa_per_se))
}

/// The stage's own checks, known before any access: once in a boot.
pub fn order(s: &State) -> Result<(), Stop> {
    if s.passed {
        return Err(stop("der Befehlsprozessor ist schon gestartet", "gccp ran in this boot: a second run is refused"));
    }
    if s.tried {
        return Err(stop("der Befehlsprozessor wurde in diesem Start schon angefasst: erst neu starten", "a gccp of this boot stopped after it wrote the block: its engines may be unhalted, and Linux never resumes them twice without its fini (gfx_v12_0.c:3914-3915, not built); Reload first"));
    }
    Ok(())
}

/// Before the guard is made: the clear-state buffer's page from the card's VRAM manager, once per boot (the
/// guard lists it as the stage's memory). A stage that will refuse (a second run, no GC info) takes nothing.
pub fn prepare(s: &mut Session, d: &Discovery) {
    if order(&s.cp).is_err() || config(d).is_none() || s.cp.csb.is_some() {
        return;
    }
    if let Some(b) = s.bump.as_mut() {
        s.cp.csb = b.take(CSB, clearstate12::BYTES, pte12::PAGE).ok();
    }
}

/// What the stage may write: Linux's writes of its section and the clear-state buffer's page.
pub fn allow(s: &Session, n: &NbRegs) -> Allow {
    let regs = cp::WRITTEN.iter().chain(&[db::ENTRY_0, db::ENTRY_3]).filter_map(|r| n.byte(*r)).collect();
    let mem = match (s.cp.csb.as_ref(), s.layout.as_ref()) {
        (Some(r), Some(l)) => alloc::vec![l.mc(r.start)..l.mc(r.end)],
        _ => Vec::new(),
    };
    Allow { stage: Stage::GcCp.name(), regs, mem }
}

impl Bus for Io<'_> {
    type E = Stop;
    fn read(&mut self, r: Reg) -> Result<u32, Stop> {
        Io::read(self, r)
    }
    fn op(&mut self, op: Op) -> Result<(), Stop> {
        Io::op(self, op)
    }
}

/// A register read and written with Linux's change; both values.
fn change(io: &mut Io, r: Reg, f: fn(u32) -> u32) -> Result<(u32, u32), Stop> {
    let v = io.read(r)?;
    io.write(r, f(v))?;
    Ok((v, f(v)))
}

/// The stage `gccp`.
pub fn run(s: &mut Session, io: &mut Io, c: &Ctx, out: &mut Vec<String>) -> Result<(), Stop> {
    order(&s.cp)?;
    let Some(cfg) = config(&c.att.discovery) else {
        return Err(stop("die Bauteilliste beschreibt den Grafikteil nicht", "the discovery table has no GC info Linux reads the shader engines from (amdgpu_discovery.c:2016-2066): the constants' loops have no bounds"));
    };
    let Some(l) = s.layout.clone() else { return Err(stop("zuerst den Speicher der Karte einrichten", "no VRAM layout in this boot: the stage gmc plans it")) };
    let Some(csb) = s.cp.csb.clone() else { return Err(stop("kein Platz im Speicher der Karte", "the VRAM manager has no page for the clear-state buffer")) };
    s.cp.tried = true;
    if let Some(w) = &s.cp.reload {
        w.set();
    }
    out.push(format!("gccp: shader engines {}, arrays per engine {}, render backends per engine {}, CUs per array {} (the discovery table's GC info version {})", cfg.shader_engines, cfg.sa_per_se, cfg.rb_per_se, cfg.cu_per_sa, c.att.discovery.gc.as_ref().map_or(0, |g| g.major)));
    let learned = match cp::constants(io, &cfg).and_then(|l| cp::vmids(|op| io.op(op)).map(|()| l)) {
        Ok(l) => l,
        Err(e) => return Err(unselect(io, e)),
    };
    out.push(format!("gccp: active shader arrays {:#x}, render backends {:#x} ({}), CUs {}; SH_MEM_CONFIG {:#x} on VMIDs 0 to 15, the compute VMIDs 8 to 15 with their apertures and trap enable", learned.active_sa, learned.active_rb, learned.rbs, learned.cus, cp::DEFAULT_SH_MEM_CONFIG));
    s.cp.learned = Some(learned);
    io.write(db::ENTRY_0, db::ENTRY_0_GFX)?;
    io.write(db::ENTRY_3, db::ENTRY_3_MES)?;
    csb_resume(io, &l, &csb, out)?;
    io.op(Op::Modify(cp::RLC_SRM_CNTL, cp::srm_enable))?;
    // Not for an APU (:3529-3530); this card is none.
    io.op(Op::Modify(cp::CP_INT_CNTL_RING0, cp::gui_idle_off))?;
    for (r, v) in [(cp::CP_RB_DOORBELL_RANGE_LOWER, db::RB_LOWER), (cp::CP_RB_DOORBELL_RANGE_UPPER, db::RB_UPPER), (cp::CP_MEC_DOORBELL_RANGE_LOWER, db::MEC_LOWER), (cp::CP_MEC_DOORBELL_RANGE_UPPER, db::MEC_UPPER)] {
        io.write(r, v)?;
    }
    let mec = change(io, gc12::CP_MEC_RS64_CNTL, cp::unhalt_mec)?;
    // Linux's udelay(50) after the compute enable (:2841): below a millisecond, so no line of the trail.
    io.card.hw().delay_us(50);
    let me = change(io, gc12::CP_ME_CNTL, cp::unhalt_gfx)?;
    out.push(format!("gccp: MEC unhalted (CP_MEC_RS64_CNTL {:#x} to {:#x}), PFP and ME unhalted (CP_ME_CNTL {:#x} to {:#x}; CE_HALT stays set, GFX 12 has no constant engine)", mec.0, mec.1, me.0, me.1));
    let settled = cp_stat_wait(io, out)?;
    s.cp.cp_stat = Some(settled);
    let kiq = change(io, cp::RLC_CP_SCHEDULERS, cp::kiq_setting)?;
    out.push(format!("gccp: the KIQ's place for MES (ME {}, pipe {}, queue {}) in RLC_CP_SCHEDULERS {:#x} to {:#x}", cp::KIQ_ME, cp::KIQ_PIPE, cp::KIQ_QUEUE, kiq.0, kiq.1));
    if let Err(last) = settled {
        return Err(stop(
            "der Befehlsprozessor kommt nach dem Start nicht zur Ruhe",
            format!("CP_STAT still {last:#x} after Linux's {LINUX_POLLS} reads: Linux logs it and goes on (gfx_v12_0.c:2380-2382), the stage stops after Linux's accesses, since no later stage may stand on a command processor Linux's trace never shows; Reload first"),
        ));
    }
    s.cp.passed = true;
    Ok(())
}

/// A stop inside the constants' loops leaves GRBM_GFX_INDEX on one shader array or GRBM_GFX_CNTL on one VMID;
/// Linux's loops always end on all arrays and VMID 0, and the registers after them, the Reload hook's halt
/// among them, assume it. Both are put back as Linux's loops end, best effort: the stop stands either way.
fn unselect(io: &mut Io, e: Stop) -> Stop {
    let _ = io.write(cp::GRBM_GFX_INDEX, cp::GFX_INDEX_ALL);
    let _ = io.write(gc12::GRBM_GFX_CNTL, gc12::grbm_select(0, 0, 0, 0));
    e
}

/// `gfx_v12_0_init_csb`: the buffer's bytes first, then its address with the low two bits cleared and its size.
fn csb_resume(io: &mut Io, l: &Layout, csb: &Range<u64>, out: &mut Vec<String>) -> Result<(), Stop> {
    let mc = l.mc(csb.start);
    let bytes = clearstate12::bytes();
    if !io.card.hw().mem_write(mc, &bytes) {
        return Err(stop("der Speicher der Karte lässt sich nicht beschreiben", format!("the clear-state buffer's {} bytes at mc {mc:#x} did not reach the card", bytes.len())));
    }
    let (lo, hi) = clearstate12::address_regs(mc);
    io.write(cp::RLC_CSIB_ADDR_HI, hi)?;
    io.write(cp::RLC_CSIB_ADDR_LO, lo)?;
    io.write(cp::RLC_CSIB_LENGTH, clearstate12::size_dw())?;
    out.push(format!("gccp: the clear-state buffer, {} dwords, at vram {:#x} (mc {mc:#x}), as Linux builds it from clearstate_gfx12.h; no HDP flush before its registers, as in Linux's trace", clearstate12::size_dw(), csb.start));
    Ok(())
}

/// `gfx_v12_0_cp_gfx_enable`'s loop (:2371-2378): CP_STAT read until 0, at most 100000 reads 1 us apart. A loop
/// that runs out is Linux's DRM_ERROR: `Err(last)`, and the stage makes Linux's last access before it stops.
fn cp_stat_wait(io: &mut Io, out: &mut Vec<String>) -> Result<Result<u32, u32>, Stop> {
    let byte = io.byte(gc12::CP_STAT)?;
    let hw = io.card.hw();
    let t0 = hw.now_us();
    match psp::wait_reads(hw, byte, Cond::Masked { mask: u32::MAX, value: 0 }, LINUX_POLLS, "CP_STAT after the unhalt") {
        Ok(_) => {
            let us = hw.now_us().saturating_sub(t0);
            out.push(format!("gccp: CP_STAT reads 0 after the unhalt ({us} us; Linux's own start: 21 reads in 46 us)"));
            Ok(Ok(u32::try_from(us).unwrap_or(u32::MAX)))
        }
        Err(psp::PspError::Timeout { last, .. }) => {
            klog::event!(Error, "gx", "rdna4.cp.unhalt_timeout", cp_stat = klog::Hex(u64::from(last)); "failed to unhalt cp gfx");
            out.push(format!("gccp: failed to unhalt cp gfx: CP_STAT still {last:#x} after {LINUX_POLLS} reads; Linux logs it and goes on (gfx_v12_0.c:2380-2382); the stage makes Linux's last access, then stops"));
            Ok(Err(last))
        }
        Err(e) => Err(psp_stop(e)),
    }
}

/// The block's command processor registers as they read now, for the stage `state`: only once `gccp` wrote them.
pub fn state_line(s: &State, n: &NbRegs, hw: &mut dyn psp::Hw) -> Option<String> {
    if !s.tried {
        return None;
    }
    let mut rd = |r: Reg| n.byte(r).and_then(|b| hw.reg_read(b)).map_or(String::from("-"), |v| format!("{v:#x}"));
    Some(format!(
        "cp registers: CP_STAT {}, CP_ME_CNTL {}, CP_MEC_RS64_CNTL {}, RLC_CP_SCHEDULERS {}, CP_INT_CNTL_RING0 {}, RLC_CSIB_LENGTH {}",
        rd(gc12::CP_STAT),
        rd(gc12::CP_ME_CNTL),
        rd(gc12::CP_MEC_RS64_CNTL),
        rd(cp::RLC_CP_SCHEDULERS),
        rd(cp::CP_INT_CNTL_RING0),
        rd(cp::RLC_CSIB_LENGTH)
    ))
}

/// Before `fw` sends its first image a second time in one boot: refused once `gccp` wrote the block. The SMU's
/// firmware and the autoload would start the block anew under engines that may run; Linux halts them first in
/// its fini (`gfx_v12_0_cp_enable(adev, false)`, gfx_v12_0.c:3914-3915), and that stop with a second start in
/// one boot is not built. A Reload halts them (`cpoff`) and the next kernel begins at `nbif`.
pub fn before_fw(s: &State) -> Result<(), Stop> {
    if s.tried {
        return Err(stop(
            "der Befehlsprozessor der Karte wurde in diesem Start schon gestartet: erst neu starten",
            "gccp wrote the command processor in this boot: a second fw would load the SMU's firmware and send the autoload under engines that may run (Linux halts them first, gfx_v12_0.c:3914-3915; that stop and second start in one boot is not built); Reload first",
        ));
    }
    Ok(())
}
