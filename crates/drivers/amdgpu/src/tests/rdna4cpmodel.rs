//! Wave 44, item CP: the model's command processor refuses what Linux would not do (`rdna4::fakecp`). Each
//! refusal is shown on its own, on a model that would otherwise take the write: removing a refusal turns its
//! line here red.

use super::rdna4cp::{hub_up, n_of};
use super::rdna4fix::*;
use super::rdna4gc::{chain_before, loaded};
use super::rdna4nbmodel::refused;
use crate::gpucore::psp::Hw;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::stage::{Session, Stage};
use std::vec::Vec;
use rdna4_core::{clearstate12, doorbell12 as db, gc12, gc12cp as cp};

fn took(m: &mut Model, byte: u32, v: u32) {
    assert!(m.reg_write(byte, v), "{byte:#x} = {v:#x}: {:?}", m.refused);
}

#[test]
fn no_unhalt_before_power_the_autoload_and_the_engines_starts() {
    let rig = Rig::new();
    let n = n_of(&rig);
    let (mec, me) = (n.byte(gc12::CP_MEC_RS64_CNTL).unwrap(), n.byte(gc12::CP_ME_CNTL).unwrap());
    // The section's plain registers, each with the value Linux writes there later: refused on the same gate.
    let plain = [(cp::SH_MEM_CONFIG, cp::DEFAULT_SH_MEM_CONFIG), (cp::GRBM_GFX_INDEX, cp::GFX_INDEX_ALL), (cp::RLC_CSIB_LENGTH, clearstate12::size_dw())];
    let asleep = |m: &mut Model| {
        for (r, v) in plain {
            refused(m, n.byte(r).unwrap(), v, "before the graphics block has power and its autoload is complete");
        }
    };
    // (a) A cold card: no power.
    let mut m = model();
    refused(&mut m, mec, cp::unhalt_mec(0x4000_0000), "before the graphics block has power and its autoload is complete");
    asleep(&mut m);
    // (b) Power, the autoload not complete.
    let mut s = Session::default();
    let (_, mut early) = rig.pass(&mut s, model(), &mut Stick::full(), chain_before(Stage::Autoload));
    early.pmfw.enabled = true;
    refused(&mut early, me, cp::unhalt_gfx(0x1514_A000), "before the graphics block has power and its autoload is complete");
    asleep(&mut early);
    // (c) Power and the autoload, but no starts written and pulsed in (`gcboot` did not run).
    let (_, _, mut m) = loaded();
    refused(&mut m, mec, cp::unhalt_mec(0x4003_0000), "not written and pulsed in on every pipe");
    refused(&mut m, me, cp::unhalt_gfx(0x1514_A000), "not written and pulsed in on every pipe");
}

#[test]
fn the_unhalt_is_linuxs_form_after_the_doorbell_ranges_and_ce_stays_halted() {
    let (rig, _, mut m) = hub_up();
    let n = n_of(&rig);
    let b = |r| n.byte(r).unwrap();
    let (mec, me) = (b(gc12::CP_MEC_RS64_CNTL), b(gc12::CP_ME_CNTL));
    // The model's order rule: the ranges first (gfx_v12_0.c:3543 before :3546).
    refused(&mut m, mec, 0x3C00_0000, "before its doorbell range holds Linux's values");
    refused(&mut m, me, 0x0100_A000, "before its doorbell range holds Linux's values");
    // Each range register takes Linux's value only.
    for (r, v) in [(cp::CP_RB_DOORBELL_RANGE_LOWER, db::RB_LOWER), (cp::CP_RB_DOORBELL_RANGE_UPPER, db::RB_UPPER), (cp::CP_MEC_DOORBELL_RANGE_LOWER, db::MEC_LOWER), (cp::CP_MEC_DOORBELL_RANGE_UPPER, db::MEC_UPPER)] {
        refused(&mut m, b(r), v + 8, "Linux writes");
    }
    // Half the compute pair: still no MEC unhalt.
    took(&mut m, b(cp::CP_MEC_DOORBELL_RANGE_LOWER), db::MEC_LOWER);
    refused(&mut m, mec, 0x3C00_0000, "before its doorbell range holds Linux's values");
    took(&mut m, b(cp::CP_MEC_DOORBELL_RANGE_UPPER), db::MEC_UPPER);
    // Not Linux's form: a pipe still in reset, a pipe not active, the instruction cache invalidated.
    refused(&mut m, mec, 0x3C01_0000, "Linux's unhalt writes 0x3c000000");
    refused(&mut m, mec, 0x1C00_0000, "Linux's unhalt writes 0x3c000000");
    refused(&mut m, mec, 0x3C00_0010, "Linux's unhalt writes 0x3c000000");
    took(&mut m, mec, 0x3C00_0000);
    took(&mut m, b(cp::CP_RB_DOORBELL_RANGE_LOWER), db::RB_LOWER);
    took(&mut m, b(cp::CP_RB_DOORBELL_RANGE_UPPER), db::RB_UPPER);
    // CE_HALT is GFX 12's fact of the register only: a write that clears it is refused.
    refused(&mut m, me, 0x0000_A000, "CE_HALT cleared; GFX 12 has no constant engine");
    refused(&mut m, me, 0x1100_A000, "Linux's unhalt writes 0x100a000");
    took(&mut m, me, 0x0100_A000);
    assert!(m.cp.mec_on && m.cp.gfx_on);
}

#[test]
fn the_constants_take_linuxs_selections_and_values_only() {
    let (rig, _, mut m) = hub_up();
    let n = n_of(&rig);
    let b = |r| n.byte(r).unwrap();
    let (index, grbm) = (b(cp::GRBM_GFX_INDEX), b(gc12::GRBM_GFX_CNTL));
    // GRBM_GFX_INDEX: one shader array of the card's, or all.
    refused(&mut m, index, cp::select_se_sa(Some(4), Some(0)), "one shader array of the card's at a time");
    refused(&mut m, index, cp::select_se_sa(Some(0), Some(2)), "one shader array of the card's at a time");
    refused(&mut m, index, 0x4000_0001, "one shader array of the card's at a time");
    took(&mut m, index, cp::select_se_sa(Some(3), Some(1)));
    // Left on a selection: every other register refuses until all are selected again.
    refused(&mut m, b(cp::GRBM_CNTL), 0x0030_00FF, "GRBM_GFX_INDEX still selects 0x40030100");
    took(&mut m, index, cp::GFX_INDEX_ALL);
    // Linux's one change of what the register holds, nothing else; registers Linux only reads.
    refused(&mut m, b(cp::GRBM_CNTL), 0x0030_0FFF, "Linux writes 0x3000ff there");
    refused(&mut m, b(cp::DB_RING_CONTROL), 0, "Linux writes 0x1 there");
    for r in [cp::GRBM_CC_GC_SA_UNIT_DISABLE, cp::CC_RB_BACKEND_DISABLE, cp::GC_USER_SHADER_ARRAY_CONFIG, cp::TA_CNTL2] {
        refused(&mut m, b(r), 0, "Linux only reads it here");
    }
    // The per-VMID registers: under a VMID of ME 0 pipe 0 only, with that VMID's values.
    took(&mut m, grbm, gc12::grbm_select(1, 0, 0, 0));
    refused(&mut m, b(cp::SH_MEM_CONFIG), cp::DEFAULT_SH_MEM_CONFIG, "no VMID of ME 0 pipe 0");
    took(&mut m, grbm, gc12::grbm_select(0, 0, 0, 0));
    refused(&mut m, b(cp::SH_MEM_CONFIG), 0xC008, "Linux writes 0xc00c there");
    refused(&mut m, b(cp::SH_MEM_BASES), cp::SH_MEM_BASES_GFX, "under VMID 0: Linux writes it under no such VMID");
    took(&mut m, grbm, gc12::grbm_select(0, 0, 0, 3));
    refused(&mut m, b(cp::SH_MEM_BASES), cp::SH_MEM_BASES_KFD, "under VMID 3: Linux writes 0x20001000");
    refused(&mut m, b(cp::SPI_GDBG_PER_VMID_CNTL), 8, "under VMID 3: Linux writes it under no such VMID");
    // Any other register while a VMID other than 0 is selected.
    refused(&mut m, b(cp::GRBM_CNTL), 0x0030_00FF, "GRBM_GFX_CNTL still selects 0x30");
    took(&mut m, grbm, gc12::grbm_select(0, 0, 0, 9));
    refused(&mut m, b(cp::SH_MEM_BASES), cp::SH_MEM_BASES_GFX, "under VMID 9: Linux writes 0x10002");
    refused(&mut m, b(cp::SPI_GDBG_PER_VMID_CNTL), 9, "Linux writes 0x8 there");
    took(&mut m, b(cp::SPI_GDBG_PER_VMID_CNTL), 8);
    took(&mut m, grbm, 0);
    // NBIF's entries take Linux's value only; the KIQ's place is MES's form only.
    refused(&mut m, b(db::ENTRY_0), 0x3000_0005, "Linux writes 0x30000007");
    refused(&mut m, b(db::ENTRY_3), db::ENTRY_0_GFX, "Linux writes 0x3000000d");
    refused(&mut m, b(cp::RLC_CP_SCHEDULERS), 0x30E0, "Linux writes 0x30e8 there");
    refused(&mut m, b(cp::CP_INT_CNTL_RING0), 0x0010_0000, "Linux writes 0x0 there");
}

#[test]
fn the_clear_state_buffer_is_linuxs_where_nothing_else_lies() {
    let (rig, s, mut m) = hub_up();
    let n = n_of(&rig);
    let b = |r| n.byte(r).unwrap();
    let l = s.layout.clone().unwrap();
    let free = s.bump.as_ref().unwrap().taken.iter().map(|t| t.1.end).max().unwrap() + 0x10_0000;
    let put = |m: &mut Model, mc: u64| {
        let (lo, hi) = clearstate12::address_regs(mc);
        took(m, b(cp::RLC_CSIB_ADDR_HI), hi);
        took(m, b(cp::RLC_CSIB_ADDR_LO), lo);
    };
    // The save-restore machine before the buffer.
    refused(&mut m, b(cp::RLC_SRM_CNTL), 3, "before the clear-state buffer was set up");
    // Not Linux's size; not Linux's bytes; off a page; over the GART table and the picture.
    put(&mut m, l.mc(free));
    refused(&mut m, b(cp::RLC_CSIB_LENGTH), 0x4C, "Linux's clear-state buffer is 0x4b dwords");
    refused(&mut m, b(cp::RLC_CSIB_LENGTH), 0x4B, "does not hold Linux's clear state");
    let mut bytes = clearstate12::bytes();
    assert!(m.mem_write(free, &bytes) && m.mem_write(free + 0x2000, &bytes));
    put(&mut m, l.mc(free + 0x800));
    refused(&mut m, b(cp::RLC_CSIB_LENGTH), 0x4B, "not a page of VRAM");
    // A page above VRAM's end, and one below VRAM's start (in the GART's range).
    let end = m.vram_bytes();
    put(&mut m, l.mc(end));
    refused(&mut m, b(cp::RLC_CSIB_LENGTH), 0x4B, "not a page of VRAM");
    put(&mut m, 0x1000);
    refused(&mut m, b(cp::RLC_CSIB_LENGTH), 0x4B, "not a page of VRAM");
    let table = s.nb.gart.as_ref().unwrap().table.start;
    put(&mut m, l.mc(table));
    refused(&mut m, b(cp::RLC_CSIB_LENGTH), 0x4B, "lies over the GART table");
    put(&mut m, l.mc(0));
    refused(&mut m, b(cp::RLC_CSIB_LENGTH), 0x4B, "lies over the firmware's picture");
    bytes[8] = 1;
    assert!(m.mem_write(free + 0x2000, &bytes));
    put(&mut m, l.mc(free + 0x2000));
    refused(&mut m, b(cp::RLC_CSIB_LENGTH), 0x4B, "does not hold Linux's clear state");
    put(&mut m, l.mc(free));
    took(&mut m, b(cp::RLC_CSIB_LENGTH), 0x4B);
    refused(&mut m, b(cp::RLC_SRM_CNTL), 0x7, "Linux sets AUTO_INCR_ADDR and SRM_ENABLE and nothing else");
    took(&mut m, b(cp::RLC_SRM_CNTL), 3);
    assert_eq!(m.cp.csb, Some((l.mc(free), 0x4B)));
}

#[test]
fn an_autoload_puts_the_command_processor_back_cold() {
    let (rig, _, mut m) = super::rdna4cp::cp_up();
    let n = n_of(&rig);
    assert!(m.cp.gfx_on && m.cp.csb.is_some());
    m.cp.queues.clear();
    let (cold, sched) = (m.reg_read(n.byte(cp::GRBM_CNTL).unwrap()), m.reg_read(n.byte(cp::RLC_CP_SCHEDULERS).unwrap()));
    assert_eq!((cold, sched), (Some(0x0030_00FF), Some(0x30E8)));
    // The secure OS's AUTOLOAD_RLC starts the block anew (`Model::gc_autoloaded`, which `fakecmd` calls).
    m.gc_autoloaded();
    assert!(!m.cp.gfx_on && !m.cp.mec_on && m.cp.csb.is_none() && m.cp.entries == [Some(db::ENTRY_0_GFX), Some(db::ENTRY_3_MES)], "NBIF's entries are no GC register");
    assert_eq!(m.reg_read(n.byte(cp::RLC_CP_SCHEDULERS).unwrap()), Some(0x3038));
}

/// The model as the Reload hook's registers.
struct Off<'a>(&'a mut Model);

impl crate::rdna4::ihoff::OffIo for Off<'_> {
    fn read(&mut self, byte: u32) -> Option<u32> {
        self.0.reg_read(byte)
    }
    fn write(&mut self, byte: u32, v: u32) -> bool {
        self.0.reg_write(byte, v)
    }
    fn delay_us(&mut self, us: u64) {
        self.0.delay_us(us)
    }
}

/// A Reload after `gccp` halts the engines as Linux's fini does (`cpoff`): graphics first with Linux's wait,
/// then MEC, then the idle interrupt off. Before the stage set out to write the block the hook touches nothing;
/// the model takes a halt only in Linux's form.
#[test]
fn a_reload_halts_the_command_processor_as_linuxs_fini_does() {
    use crate::rdna4::cpoff::{self, OffRegs};
    use crate::rdna4::sdmaoff::{Stopped, Written};
    let (rig, _, mut m) = super::rdna4cp::cp_up();
    let n = n_of(&rig);
    let regs = OffRegs::of(&n).unwrap();
    let w = Written::default();
    m.writes.clear();
    assert_eq!(cpoff::halt(&mut Off(&mut m), &regs, &w), Stopped::Untouched);
    assert!(m.writes.is_empty());
    w.set();
    assert_eq!(cpoff::halt(&mut Off(&mut m), &regs, &w), Stopped::Halted);
    assert!(!m.cp.gfx_on && !m.cp.mec_on && m.refused.is_empty(), "{:?}", m.refused);
    let wrote: Vec<(u32, u32)> = m.writes.clone();
    assert_eq!(wrote, [(regs.me_cntl, 0x1500_A000), (regs.mec_cntl, 0x400F_0010), (regs.int_cntl, 0)]);
    // Not Linux's halt: refused.
    let (rig, _, mut m) = super::rdna4cp::cp_up();
    let n = n_of(&rig);
    refused(&mut m, n.byte(gc12::CP_MEC_RS64_CNTL).unwrap(), 0x7C00_0000, "Linux's halt writes 0x400f0010");
}

/// An unhalt or a halt while GRBM_GFX_CNTL selects anything but VMID 0 of pipe 0 is refused, as `fakegc` refuses
/// every other graphics register then.
#[test]
fn no_unhalt_while_a_selection_is_left() {
    let (rig, _, mut m) = hub_up();
    let n = n_of(&rig);
    let b = |r| n.byte(r).unwrap();
    for (r, v) in [(cp::CP_RB_DOORBELL_RANGE_LOWER, db::RB_LOWER), (cp::CP_RB_DOORBELL_RANGE_UPPER, db::RB_UPPER), (cp::CP_MEC_DOORBELL_RANGE_LOWER, db::MEC_LOWER), (cp::CP_MEC_DOORBELL_RANGE_UPPER, db::MEC_UPPER)] {
        took(&mut m, b(r), v);
    }
    took(&mut m, b(gc12::GRBM_GFX_CNTL), 0x20);
    refused(&mut m, b(gc12::CP_MEC_RS64_CNTL), 0x3C00_0000, "GRBM_GFX_CNTL still selects 0x20");
    took(&mut m, b(gc12::GRBM_GFX_CNTL), 0);
    took(&mut m, b(gc12::CP_MEC_RS64_CNTL), 0x3C00_0000);
    took(&mut m, b(gc12::GRBM_GFX_CNTL), 0x90);
    refused(&mut m, b(gc12::CP_MEC_RS64_CNTL), cp::halt_mec(0x3C00_0000), "GRBM_GFX_CNTL still selects 0x90");
}
