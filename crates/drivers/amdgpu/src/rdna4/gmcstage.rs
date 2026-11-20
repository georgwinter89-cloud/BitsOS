//! Wave 42 (piece 10 without GFXHUB, start of 18): the stage `gmc`, GMC 12 with MMHUB 4.1.0. Trace section `mmhub`
//! (init.txt lines 17-205).
//!
//! `plan` is Linux's `gmc_v12_0_sw_init` as far as the card sees it: four reads (BIOS_SCRATCH_4, which Linux's
//! VBIOS parser reads at this place, amdgpu_atomfirmware.c:534, and BitsOS only repeats; the memory size; where
//! the card's firmware put VRAM; its offset), from which the layout follows (`hub41::layout`), and the two buffers
//! Linux allocates before `hw_init`: the GART table (`gmc_v12_0_gart_init` gmc_v12_0.c:771-806) and the default
//! page (`amdgpu_device_mem_scratch_init` amdgpu_device.c:996), both from the card's VRAM manager. It runs through
//! an owner with its gate shut, before the stage's guard exists, so the guard lists exactly these two.
//!
//! `run` is `gmc_v12_0_hw_init` (:1037-1060): `gmc_v12_0_init_golden_registers` writes nothing for this chip,
//! then `gmc_v12_0_gart_enable` (:1002-1035). The first system address reaches the card here (the fault's default
//! page), so the DMAR survey must have said yes before the first write (plan/spec/gpu-rdna4.md piece 22).

use super::attach::PciFacts;
use super::gart12::{self, Gart};
use super::nb::{Io, Plan, State};
use super::policy;
use super::stage::{Stop, psp_stop, stop};
use super::vram::{self, Bump, Layout};
use crate::attach::Gop;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::hub41::{self, Chosen};
use rdna4_core::{layout, nbreg, pte12};

/// `adev->gart.table_size`: one 8-byte entry per 4 KiB page of the GART.
pub const TABLE_BYTES: u64 = layout::GART_BYTES / pte12::PAGE * 8;
/// The table is filled in pieces of this size (each one mapping of BAR0 in the kernel's glue).
const FILL_PIECE: usize = 64 << 10;

/// The stage's own order check: once in a boot (that `nbif` passed before it is the chain's, `stage::in_order`).
pub fn order(s: &State) -> Result<(), Stop> {
    if s.gart.is_some() {
        return Err(stop("der Speicher der Karte ist schon eingerichtet", "gmc ran in this boot: a second run is refused"));
    }
    Ok(())
}

/// The reads and the plan. `layout` and `bump` are the session's: made here when no stage made them before.
pub fn plan(s: &mut State, io: &mut Io, facts: &PciFacts, gop: Option<Gop>, attached_vram: u64, layout: &mut Option<Layout>, bump: &mut Option<Bump>, out: &mut Vec<String>) -> Result<(), Stop> {
    order(s)?;
    let scratch4 = io.read(nbreg::BIOS_SCRATCH_4)?;
    let memsize = psp_read(io, io.n.memsize)?;
    let base = psp_read(io, io.n.fb_base)?;
    let offset = io.read(hub41::FB_OFFSET)?;
    let l = hub41::layout(memsize, base, offset).map_err(|e| stop("die Karte nennt ihren Speicher nicht", format!("{e:?}")))?;
    out.push(format!("gmc: BIOS_SCRATCH_4 {scratch4:#x} (read as Linux does; BitsOS parses no VBIOS), memory {memsize} MiB, VRAM at mc {:#x}..={:#x}, base offset {:#x}, GART at mc {:#x}..={:#x}", l.vram_start, l.vram_end(), l.base_offset, l.gart_start, l.gart_end()));
    if l.vram_bytes != attached_vram {
        return Err(stop("die Karte nennt ihren Speicher anders als beim ersten Lesen", format!("memory size {:#x} now, {attached_vram:#x} at the attach", l.vram_bytes)));
    }
    let bar0 = facts.bar0.ok_or_else(|| stop("Fenster zum Speicher der Karte fehlt", "no BAR0"))?;
    let placed = vram::place(l.vram_bytes, l.vram_start, bar0, gop).map_err(|e| stop("kein sicherer Platz im Speicher der Karte", format!("{e:?}")))?;
    match layout {
        Some(have) if *have != placed => return Err(stop("der Speicher der Karte liegt anders als vorher geplant", format!("planned {have:?}, the card now says {placed:?}"))),
        Some(_) => {}
        None => *layout = Some(placed.clone()),
    }
    let bump = bump.get_or_insert_with(|| Bump::new(&placed));
    // A run that stopped after its plan gives its two ranges back before it plans again.
    if let Some(old) = s.plan.take() {
        bump.give_back(&old.table);
        bump.give_back(&old.scratch);
    }
    let no_room = |e| stop("kein Platz im Speicher der Karte", format!("{e:?}"));
    let table = bump.take("GART table", TABLE_BYTES, pte12::PAGE).map_err(no_room)?;
    let scratch = match bump.take("default page", pte12::PAGE, pte12::PAGE) {
        Ok(r) => r,
        Err(e) => {
            bump.give_back(&table);
            return Err(no_room(e));
        }
    };
    out.push(format!("gmc: GART table {:#x} bytes at vram {:#x}, default page at vram {:#x}", TABLE_BYTES, table.start, scratch.start));
    s.plan = Some(Plan { l, table, scratch });
    Ok(())
}

fn psp_read(io: &mut Io, byte: u32) -> Result<u32, Stop> {
    crate::gpucore::psp::read(io.card.hw(), byte).map_err(psp_stop)
}

/// One hub's start on this boot's table, default page and dummy page, as Linux starts either hub
/// (`gmc_v12_0_gart_enable` gmc_v12_0.c:1014-1027 for MMHUB, `gfx_v12_0_gfxhub_enable` gfx_v12_0.c:3603-3622 for
/// GFXHUB): the hub's `gart_enable`, "Flush HDP after it is initialized", the hub's fault default, its first TLB
/// flush on engine 17. `gmc` runs it on `hub41::MM`, the stage `gchub` on `hub41::GC`: nothing is planned twice.
pub fn start_hub(io: &mut Io, h: &hub41::Hub, l: &hub41::Layout12, c: &Chosen, f: &gart12::FlushRegs, out: &mut Vec<String>) -> Result<(), Stop> {
    hub41::gart_enable(h, l, c, |op| io.op(op))?;
    gart12::flush_hdp(io.card.hw(), f).map_err(psp_stop)?;
    hub41::fault_enable_default(h, |op| io.op(op))?;
    gart12::flush_hub_tlb(io.card.hw(), f, h.kind, out).map_err(psp_stop)
}

pub fn run(s: &mut State, io: &mut Io, out: &mut Vec<String>) -> Result<(), Stop> {
    order(s)?;
    let Some(p) = s.plan.clone() else { return Err(stop("kein sicherer Platz im Speicher der Karte", "no plan: the stage's reads did not run")) };
    // Before any write: from this stage on the card is told system addresses.
    let survey = io.card.dma_survey();
    policy::may_send(io.driven, survey).map_err(|why| stop("der Rechner lässt die Karte nicht an den Arbeitsspeicher", why))?;
    let dummy = io.card.pages(pte12::PAGE).ok_or_else(|| stop("kein Arbeitsspeicher für die Karte frei", "no page for the fault's default page"))?;
    // INTERRUPT_CNTL2 will hold this address >> 8 in 32 bits (nbif_v6_3_1.c:392): below 1 TiB, inside the 44 bits.
    if !dummy.is_multiple_of(pte12::PAGE) || !policy::dma_reachable(dummy, pte12::PAGE) || dummy >> 40 != 0 {
        return Err(stop("der Arbeitsspeicher für die Karte liegt zu hoch", format!("dummy page at {dummy:#x}: not a page below 1 TiB")));
    }
    out.push(format!("gmc: DMAR survey allows DMA; dummy page (system) at {dummy:#x}"));
    // Linux's fresh table: memset_io with the flags' low byte (amdgpu_gart.c:278), and no flush after it. The
    // piece is as large as the whole stack of the thread a stage runs on (MCP's 64 KiB), so it lies on the heap.
    let fill = alloc::vec![pte12::FILL_BYTE; FILL_PIECE];
    let mut at = p.table.start;
    while at < p.table.end {
        let n = FILL_PIECE.min((p.table.end - at) as usize);
        if !io.card.hw().mem_write(p.l.mc(at), &fill[..n]) {
            return Err(stop("Speicher der Karte lässt sich nicht beschreiben", format!("GART table at vram {at:#x}")));
        }
        at += n as u64;
    }
    if !io.card.hw().mem_write(p.l.mc(p.scratch.start), &[0u8; pte12::PAGE as usize]) {
        return Err(stop("Speicher der Karte lässt sich nicht beschreiben", format!("default page at vram {:#x}", p.scratch.start)));
    }
    io.card.hw().store_barrier();
    let chosen = Chosen { table: p.table.start, scratch: p.scratch.start, dummy };
    let f = io.flush()?;
    start_hub(io, &hub41::MM, &p.l, &chosen, &f, out)?;
    out.push(format!("gmc: PCIE GART of {}M enabled (table at mc {:#x}), page table base {:#x}", p.l.gart_bytes >> 20, p.l.mc(p.table.start), hub41::pd_addr(&p.l, p.table.start)));
    s.gart = Some(Gart::new(p.l, p.table, p.scratch, dummy));
    Ok(())
}
