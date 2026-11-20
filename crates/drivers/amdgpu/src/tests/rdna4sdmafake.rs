//! Wave 43: what the model's SDMA engines refuse (`rdna4::fakesdma`, `fakesdmaeng`), each refusal on its own with
//! the step right beside it that the model takes, so a test fails when its refusal is gone: no power, no
//! firmware, a ring enabled before everything it needs was written or on pages the card's walker does not see, a
//! pointer moved while the ring runs or backwards without MINOR_PTR_UPDATE, every way a doorbell can be wrong, a
//! packet Linux does not send, a trap with its enable off, and VRAM the CPU wrote without an HDP flush. What the
//! hub and the engines' memory refuse is `rdna4sdmahub`.

use super::rdna4fix::*;
use super::rdna4sdma::{loaded, sr, up};
use crate::gpucore::psp::Hw;
use crate::rdna4::fakeih::ModelIo;
use crate::rdna4::fakemodel::{MC_BASE, Model};
use crate::rdna4::fakesdma::STATUS_IDLE;
use crate::rdna4::gart12::FlushRegs;
use crate::rdna4::guard::Mc;
use crate::rdna4::host::Host;
use crate::rdna4::nbregs::NbRegs;
use crate::rdna4::sdmaih;
use crate::rdna4::sdmaq::lay;
use crate::rdna4::sdmastage::Up;
use crate::rdna4::stage::Stage;
use amdgpu_fmt::{sdma as pkt, sdma7 as pkt7};
use rdna4_core::regmap::Reg;
use rdna4_core::sdma7;
use rdna4_core::{layout, nbreg};
use std::vec::Vec;

pub(super) fn b(rig: &Rig, r: Reg, i: u32) -> u32 {
    sr(rig).byte(r, i).unwrap()
}

/// The model took this write, or refused it with these words.
pub(super) fn took(m: &mut Model, byte: u32, v: u32) {
    assert!(m.reg_write(byte, v), "{byte:#x} = {v:#x}: {:?}", m.refused);
}

pub(super) fn refused(m: &mut Model, byte: u32, v: u32, why: &str) {
    let before = m.refused.len();
    assert!(!m.reg_write(byte, v), "{byte:#x} = {v:#x} was taken; it must be refused ({why})");
    assert!(m.refused.len() == before + 1 && m.refused[before].contains(why), "{:?}", &m.refused[before..]);
    m.refused.truncate(before);
}

/// One submission straight to the model, as a driver makes it: the words at the write pointer, padded to the
/// fetch size, the write pointer into its poll slot, the doorbell.
pub(super) fn push(m: &mut Model, up: &Up, i: u32, words: &[u32]) -> bool {
    let at = m.sdma.e[i as usize].wptr;
    let mut all = words.to_vec();
    all.resize(words.len().next_multiple_of(16), pkt::NOP);
    let bytes: Vec<u8> = all.iter().flat_map(|d| d.to_le_bytes()).collect();
    assert!(m.sys_write(up.place.sys + lay::ring(i) + at % u64::from(sdma7::RING_BYTES), &bytes));
    let new = at + bytes.len() as u64;
    assert!(m.sys_write(up.place.slot_sys(up.q[i as usize].slots.wptr), &new.to_le_bytes()));
    m.doorbell_write64(sdma7::doorbell_dw(i), new)
}

pub(super) fn sys32(m: &mut Model, at: u64) -> u32 {
    let mut x = [0u8; 4];
    assert!(m.sys_read(at, &mut x));
    u32::from_le_bytes(x)
}

#[test]
fn no_write_and_no_doorbell_without_the_gc_blocks_power() {
    let rig = Rig::new();
    let (s, mut m, _) = up(&rig);
    // The power controller's features off again behind the driver's back: the model's one power answer.
    m.pmfw.enabled = false;
    for i in 0..2 {
        for r in [sdma7::MCU_CNTL, sdma7::RB_CNTL, sdma7::RB_BASE, sdma7::DOORBELL, sdma7::WATCHDOG_CNTL, sdma7::IB_CNTL, sdma7::CNTL] {
            let v = m.sdma.e[i as usize].val(r);
            refused(&mut m, b(&rig, r, i), v, "while the GC block has no power");
        }
    }
    refused(&mut m, sr(&rig).nbif(sdma7::DOORBELL_ENTRY_2).unwrap(), 0x3051_001D, "while the GC block has no power");
    assert!(!push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &[pkt::NOP]) && m.refused.pop().unwrap().contains("rung while the GC block has no power"));
    // A read changes nothing and passes: the stage's first access is one.
    assert_eq!(m.reg_read(b(&rig, sdma7::RB_CNTL, 0)), Some(0x0084_1817));
    // With power the same writes and the same doorbell are taken.
    m.pmfw.enabled = true;
    took(&mut m, b(&rig, sdma7::WATCHDOG_CNTL, 0), 1);
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &[pkt::NOP]) && m.refused.is_empty());
}

/// The other half of the one gate: the autoload. The secure processor loads the engines' firmware (type 71) and
/// then sends AUTOLOAD_RLC; before that the block takes no write at all, whatever the power controller says,
/// and with the autoload but without the power controller's features neither.
#[test]
fn no_write_before_the_autoload_is_complete_and_the_features_are_on() {
    let rig = Rig::new();
    let why = "while the GC block has no power or its autoload is not complete";
    // The three blocks started, nothing loaded; then up to RLC_G but no autoload: with the features forced on.
    for stages in [&[][..], &[Stage::Sos, Stage::Ring, Stage::Toc, Stage::Tmr, Stage::Fw][..]] {
        let (_, mut m) = rig.up_to(stages);
        m.pmfw.enabled = true;
        assert!(m.gc_powered() && !m.gc_awake());
        refused(&mut m, b(&rig, sdma7::MCU_CNTL, 0), 0, why);
        refused(&mut m, b(&rig, sdma7::RB_CNTL, 1), 0x0084_1817, why);
        // Not even a halt or a ring's control without its enable: Linux writes nothing here yet.
        refused(&mut m, b(&rig, sdma7::MCU_CNTL, 0), 1, why);
        refused(&mut m, b(&rig, sdma7::RB_CNTL, 1), 0x0084_0816, why);
    }
    // The firmware loaded and the autoload sent, the power controller not started: the chain's state before `pmfw`.
    let (_, mut m) = rig.up_to(&ORDER);
    assert!(!m.gc_powered() && !m.gc_awake());
    refused(&mut m, b(&rig, sdma7::MCU_CNTL, 0), 0, why);
    // Both: the unhalt is taken, also once the ring of the secure processor is gone again.
    let (mut s, mut m) = loaded(&rig);
    assert!(m.gc_awake());
    took(&mut m, b(&rig, sdma7::MCU_CNTL, 0), 0);
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy]);
    took(&mut m, b(&rig, sdma7::MCU_CNTL, 1), 0);
}

/// The ring's start, register by register: switched off and on again on the engine the stage started.
#[test]
fn a_ring_starts_only_with_everything_written_since_it_was_last_off() {
    let rig = Rig::new();
    let (s, mut m, _) = up(&rig);
    let up = s.sdma.up.as_ref().unwrap();
    let r = |x| b(&rig, x, 0);
    let (on, off) = (0x0084_1817, 0x0084_1816);
    let base = (up.place.ring_mc(0) >> 8) as u32;
    // While the ring runs no pointer moves.
    for p in [sdma7::RB_RPTR, sdma7::RB_RPTR_HI, sdma7::RB_WPTR, sdma7::RB_WPTR_HI] {
        refused(&mut m, r(p), 0, "a ring pointer written while the ring runs");
    }
    took(&mut m, r(sdma7::RB_CNTL), off);
    refused(&mut m, r(sdma7::RB_CNTL), on, "before its base, its size and its four pointers were written since it was last off");
    // The write pointer stands at 0x40 from the ring test: back to 0 only under MINOR_PTR_UPDATE.
    refused(&mut m, r(sdma7::RB_WPTR), 0, "a write pointer below the 0x40 it holds without MINOR_PTR_UPDATE");
    took(&mut m, r(sdma7::MINOR_PTR_UPDATE), 1);
    let all: [(Reg, u32); 6] = [(sdma7::RB_RPTR, 0), (sdma7::RB_RPTR_HI, 0), (sdma7::RB_WPTR, 0), (sdma7::RB_WPTR_HI, 0), (sdma7::RB_BASE, base), (sdma7::RB_BASE_HI, 0)];
    // With any one of the pointers or either half of the base left out the ring does not start; the size is in
    // the ring's control itself, and a control with no size starts nothing either.
    for skip in 0..all.len() {
        for (k, (reg, v)) in all.iter().enumerate() {
            if k != skip {
                took(&mut m, r(*reg), *v);
            }
        }
        refused(&mut m, r(sdma7::RB_CNTL), on, "before its base, its size and its four pointers were written since it was last off");
        // The refused enable did not use up what was written; off again does.
        took(&mut m, r(sdma7::RB_CNTL), off);
    }
    for (reg, v) in all {
        took(&mut m, r(reg), v);
    }
    refused(&mut m, r(sdma7::RB_CNTL), on & !sdma7::f::RB_SIZE.mask, "before its base, its size and its four pointers were written since it was last off");
    took(&mut m, r(sdma7::RB_CNTL), off);
    for (reg, v) in all {
        took(&mut m, r(reg), v);
    }
    // A halted engine, a base outside the GART, a slot that is not bound, a card that may not send.
    took(&mut m, r(sdma7::MCU_CNTL), 1);
    refused(&mut m, r(sdma7::RB_CNTL), on, "the ring enabled on a halted engine");
    took(&mut m, r(sdma7::MCU_CNTL), 0);
    took(&mut m, r(sdma7::RB_BASE), ((layout::GART_BYTES - 4096) >> 8) as u32);
    refused(&mut m, r(sdma7::RB_CNTL), on, "not inside the GART");
    took(&mut m, r(sdma7::RB_BASE), (MC_BASE >> 8) as u32);
    took(&mut m, r(sdma7::RB_BASE_HI), (MC_BASE >> 40) as u32);
    refused(&mut m, r(sdma7::RB_CNTL), on, "not inside the GART");
    took(&mut m, r(sdma7::RB_BASE_HI), 0);
    // The test pages of the stage's own piece are not bound: a ring there is refused, page 0 named.
    took(&mut m, r(sdma7::RB_BASE), ((up.place.mc + lay::TEST) >> 8) as u32);
    refused(&mut m, r(sdma7::RB_CNTL), on, "its pages are not bound in the GART (page 0, as the card's walker sees the table now)");
    // The ring's second page behind the bound ones: page 1 named.
    took(&mut m, r(sdma7::RB_BASE), ((up.place.mc + lay::BOUND - 4096) >> 8) as u32);
    refused(&mut m, r(sdma7::RB_CNTL), on, "its pages are not bound in the GART (page 1,");
    took(&mut m, r(sdma7::RB_BASE), base);
    let slot = m.sdma.e[0].val(sdma7::RB_RPTR_ADDR_LO);
    took(&mut m, r(sdma7::RB_RPTR_ADDR_LO), (up.place.mc + lay::TEST) as u32);
    refused(&mut m, r(sdma7::RB_CNTL), on, "its read-pointer slot at mc");
    took(&mut m, r(sdma7::RB_RPTR_ADDR_LO), slot);
    let poll = m.sdma.e[0].val(sdma7::RB_WPTR_POLL_ADDR_LO);
    took(&mut m, r(sdma7::RB_WPTR_POLL_ADDR_LO), 0);
    refused(&mut m, r(sdma7::RB_CNTL), on, "its write-pointer poll slot at mc 0x0 not bound");
    took(&mut m, r(sdma7::RB_WPTR_POLL_ADDR_LO), poll);
    m.nb.bus_master = false;
    refused(&mut m, r(sdma7::RB_CNTL), on, "while the card may not send");
    m.nb.bus_master = true;
    // Everything as Linux leaves it: the ring starts, and runs a submission from its new start.
    took(&mut m, r(sdma7::RB_CNTL), on);
    took(&mut m, r(sdma7::MINOR_PTR_UPDATE), 0);
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &[pkt::NOP]) && m.sdma.e[0].rptr == 0x40 && m.refused.is_empty(), "{:?}", m.refused);
    // Registers a driver only reads take no write; NBIF's entry only Linux's value for what it holds.
    refused(&mut m, r(sdma7::STATUS_REG), 0, "a register a driver only reads");
    refused(&mut m, sr(&rig).nbif(sdma7::DOORBELL_ENTRY_2).unwrap(), 0x3051_001F, "Linux writes 0x3051001d for what it holds");
}

/// A bind with no flush: the driver's GART holds the pages, the card's walker does not see them yet, and a ring
/// there is refused until Linux's flush after the bind was made.
#[test]
fn a_ring_on_pages_bound_without_linuxs_flush_is_refused() {
    let rig = Rig::new();
    let (mut s, mut m) = loaded(&rig);
    let n = NbRegs::from(&rig.att.discovery, &super::rdna4nb::regs(&rig)).unwrap();
    let flush = FlushRegs::of(&n, s.nb.hdp_flush(n.hdp_flush)).unwrap();
    let l = s.layout.clone().unwrap();
    let sys = m.pages(4 * 4096).unwrap();
    let g = s.nb.gart.as_mut().unwrap();
    let mut card = Mc { hw: m, base: l.mc_base, bytes: l.vram_bytes };
    let mc = g.bind(&mut card, "a ring", &crate::rdna4::nb::pages_of(sys, 4 * 4096)).unwrap();
    let mut m = card.hw;
    let r = |x| b(&rig, x, 0);
    took(&mut m, r(sdma7::RB_CNTL), 0x0084_0816);
    for reg in [sdma7::RB_RPTR, sdma7::RB_RPTR_HI, sdma7::RB_WPTR, sdma7::RB_WPTR_HI, sdma7::RB_BASE_HI, sdma7::RB_RPTR_ADDR_HI, sdma7::RB_WPTR_POLL_ADDR_HI] {
        took(&mut m, r(reg), 0);
    }
    took(&mut m, r(sdma7::RB_BASE), (mc >> 8) as u32);
    took(&mut m, r(sdma7::RB_RPTR_ADDR_LO), (mc + 0x2000) as u32);
    took(&mut m, r(sdma7::RB_WPTR_POLL_ADDR_LO), (mc + 0x2020) as u32);
    refused(&mut m, r(sdma7::RB_CNTL), 0x0084_1817, "its pages are not bound in the GART (page 0, as the card's walker sees the table now)");
    // Linux's flush on MMHUB alone, as before GFXHUB was on: MMHUB's walker sees the pages now, the engines'
    // walker is GFXHUB's and does not.
    let mut card = Mc { hw: m, base: l.mc_base, bytes: l.vram_bytes };
    crate::rdna4::gart12::invalidate_tlb(&mut card, &flush, &mut Vec::new()).unwrap();
    let mut m = card.hw;
    assert!(m.gart_walk(mc).is_some() && m.gc_walk(mc).is_none());
    refused(&mut m, r(sdma7::RB_CNTL), 0x0084_1817, "its pages are not bound in the GART (page 0, as the card's walker sees the table now)");
    // The flush of both hubs, as this boot's session makes it once GFXHUB is on.
    let both = s.flush_regs(&n).unwrap();
    assert!(both.gc.is_some() && flush.gc.is_none());
    let mut card = Mc { hw: m, base: l.mc_base, bytes: l.vram_bytes };
    crate::rdna4::gart12::invalidate_tlb(&mut card, &both, &mut Vec::new()).unwrap();
    let mut m = card.hw;
    assert!(m.gc_walk(mc).is_some());
    took(&mut m, r(sdma7::RB_CNTL), 0x0084_1817);
    assert!(m.refused.is_empty() && m.sdma.e[0].on);
}

#[test]
fn a_doorbell_is_taken_only_as_linux_rings_it() {
    let rig = Rig::new();
    // Before the stage: no aperture entry, no queue's doorbell.
    let (_, mut m) = loaded(&rig);
    assert!(!m.doorbell_write64(0x200, 0x40) && m.refused.pop().unwrap().contains("before the aperture and NBIF's doorbell entry for the engines were set up to cover it"));
    let (s, mut m, _) = up(&rig);
    let up = s.sdma.up.as_ref().unwrap();
    let said = |m: &mut Model, why: &str| {
        let got = m.refused.pop().unwrap_or_default();
        assert!(got.contains(why), "{got}");
    };
    let slot = up.place.slot_sys(up.q[0].slots.wptr);
    assert!(m.sys_write(slot, &0x80u64.to_le_bytes()));
    // A dword outside NBIF's range (below 0x200 the doorbells are the command processor's, `fakecpeng`), and one
    // inside it that is no queue's.
    assert!(!m.doorbell_write64(0x300, 0x80));
    said(&mut m, "before the aperture and NBIF's doorbell entry for the engines were set up to cover it");
    assert!(!m.doorbell_write64(0x228, 0x80));
    said(&mut m, "set up to cover it");
    assert!(!m.doorbell_write64(0x202, 0x80));
    said(&mut m, "doorbell 0x202: no engine's queue has this doorbell enabled");
    // The aperture closed again.
    let aper = m.nb.n.byte(nbreg::DOORBELL_APER_EN).unwrap();
    assert!(m.reg_write(aper, 0) && !m.doorbell_write64(0x200, 0x80));
    said(&mut m, "before the aperture");
    assert!(m.reg_write(aper, 1));
    // The queue's own doorbell register off; then on at another offset.
    took(&mut m, b(&rig, sdma7::DOORBELL, 0), 0);
    assert!(!m.doorbell_write64(0x200, 0x80));
    said(&mut m, "no engine's queue has this doorbell enabled");
    took(&mut m, b(&rig, sdma7::DOORBELL, 0), 0x1000_0000);
    took(&mut m, b(&rig, sdma7::DOORBELL_OFFSET, 0), 0x808);
    assert!(!m.doorbell_write64(0x200, 0x80));
    said(&mut m, "no engine's queue has this doorbell enabled");
    took(&mut m, b(&rig, sdma7::DOORBELL_OFFSET, 0), 0x800);
    // A value that is no byte count of dwords; a value the poll slot does not hold.
    assert!(!m.doorbell_write64(0x200, 0x82));
    said(&mut m, "no write pointer in bytes");
    assert!(!m.doorbell_write64(0x200, 0xC0));
    said(&mut m, "engine 0's write-pointer poll slot holds Some(80): Linux writes the slot first");
    // A 32-bit doorbell is the IH's kind: the engines' are 64 bits.
    assert!(!m.doorbell_write32(0x200, 0x80));
    said(&mut m, "not an IH doorbell");
    // As Linux rings it: taken, and the engine ran to it.
    assert!(m.doorbell_write64(0x200, 0x80) && m.refused.is_empty() && m.sdma.e[0].rptr == 0x80, "{:?}", m.refused);
    assert_eq!(m.sdma.doorbells.last(), Some(&(0x200, 0x80)));
    // The ring off: its doorbell rings nothing.
    took(&mut m, b(&rig, sdma7::RB_CNTL, 0), 0x0084_1816);
    assert!(!m.doorbell_write64(0x200, 0x80));
    said(&mut m, "rung with engine 0's ring off");
}

/// A packet Linux does not send stops the engine at it: the read pointer and its slot stay there, the status
/// loses IDLE, nothing behind it runs, and the model says so. Switching the ring off and on is the way back.
#[test]
fn a_packet_linux_does_not_send_stops_the_engine_at_it() {
    let rig = Rig::new();
    let mut old_copy = Vec::new();
    pkt::copy_linear(&mut old_copy, 0x82000, 0x83000, 64, false).unwrap();
    let mut old_fill = Vec::new();
    pkt::const_fill(&mut old_fill, 0x82000, 1, 64, pkt::FILL_BYTE).unwrap();
    let mut dcc = Vec::new();
    pkt7::copy_linear(&mut dcc, 0x82000, 0x83000, 64).unwrap();
    dcc[7] = 1;
    let cases: [(&[u32], &str); 5] = [
        (&[0x0000_00FF], "Unknown { header: 255 }"),
        (&old_copy, "Sdma(Unknown { header: 1 })"),
        (&old_fill, "CONST_FILL without COMPRESS"),
        (&dcc, "Unsupported"),
        // A write to an address that is no dword's.
        (&[2, 0x8_6101, 0, 0, 5], "WRITE_LINEAR address alignment"),
    ];
    for (words, why) in cases {
        let (s, mut m, _) = up(&rig);
        let up = s.sdma.up.as_ref().unwrap();
        let mut all = std::vec![pkt::NOP; 16];
        all.extend_from_slice(words);
        assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 1, &all), "{:?}", m.refused);
        let e = &m.sdma.e[1];
        // The NOPs before it ran (0x40 from the ring test, 16 dwords more), the packet did not.
        assert_eq!((e.rptr, e.wptr > e.rptr), (0x80, true), "{why}");
        assert!(e.halted.as_ref().is_some_and(|h| h.starts_with("SDMA1 stopped at read pointer 0x80: ") && h.contains(why)), "{:?}", e.halted);
        assert_eq!(m.refused.len(), 1);
        let slot = up.place.slot_sys(up.q[1].slots.rptr);
        assert_eq!(sys32(&mut m, slot), 0x80);
        let r = |x| b(&rig, x, 1);
        assert_eq!((m.reg_read(r(sdma7::STATUS_REG)), m.reg_read(r(sdma7::RB_RPTR)), m.reg_read(r(sdma7::RB_RPTR_FETCH))), (Some(0), Some(0x80), Some(0x80)));
        // The other engine is idle and goes on.
        assert_eq!(m.reg_read(b(&rig, sdma7::STATUS_REG, 0)), Some(STATUS_IDLE));
        assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &[pkt::NOP]) && m.sdma.e[0].rptr == 0x80);
        // More work for the stopped engine is taken by its doorbell and not run.
        assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 1, &[pkt::NOP]) && m.sdma.e[1].rptr == 0x80 && m.refused.len() == 1);
    }
}

#[test]
fn a_trap_arrives_only_with_its_enable_and_names_its_engine() {
    let rig = Rig::new();
    let (mut s, mut m, _) = up(&rig);
    let mut t = Vec::new();
    pkt::trap(&mut t, 0).unwrap();
    // The cold card has the trap enabled (SDMA_CNTL 0x2441, init.txt line 9823).
    assert_eq!(m.reg_read(b(&rig, sdma7::CNTL, 0)), Some(0x2441));
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &t) && push(&mut m, s.sdma.up.as_ref().unwrap(), 1, &t) && push(&mut m, s.sdma.up.as_ref().unwrap(), 1, &t));
    assert_eq!((m.sdma.e[0].traps, m.sdma.e[1].traps, m.sdma.e[0].traps_lost + m.sdma.e[1].traps_lost), (1, 2, 0));
    let ih = s.nb.ih.as_mut().unwrap();
    let sys = ih.place.sys;
    // A reader nobody registered at counts the entries as unhandled, per (client, source); it leaves the card's
    // read pointer where the session's own reader then starts.
    let mut plain = crate::rdna4::ihring::Reader::default();
    assert_eq!(plain.process(&mut ModelIo { m: &mut m, sys })[0].entries, 3);
    assert_eq!((plain.unclaimed.get(&(10, 49)), plain.taken(10, 49, 0), plain.taken(10, 49, 16)), (Some(&3), 0, 0));
    // The stage `sdma` registered the engines at the session's reader: taken per engine, as the ring id says
    // (0 and 16), and not unhandled.
    assert_eq!(ih.reader.process(&mut ModelIo { m: &mut m, sys })[0].entries, 3);
    assert_eq!((ih.reader.unclaimed.get(&(10, 49)), sdmaih::traps(ih, 0), sdmaih::traps(ih, 1)), (None, 1, 2));
    assert_eq!((ih.reader.taken(10, 49, 0), ih.reader.taken(10, 49, 16), ih.reader.taken(10, 49, 1)), (1, 2, 0));
    assert!(ih.reader.describe().contains("taken: sdma trap client 10 source 49 ring id 0: 1, sdma trap client 10 source 49 ring id 16: 2"), "{}", ih.reader.describe());
    // The enable off: the engine runs the packet and no entry is written.
    took(&mut m, b(&rig, sdma7::CNTL, 0), 0x2440);
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &t));
    assert_eq!((m.sdma.e[0].traps, m.sdma.e[0].traps_lost, m.sdma.e[0].halted.clone()), (1, 1, None));
    assert_eq!(ih.reader.process(&mut ModelIo { m: &mut m, sys })[0].entries, 0);
    assert!(m.refused.is_empty(), "{:?}", m.refused);
}

/// What the CPU wrote into VRAM through BAR0 an engine reads only after an HDP flush: the driver's, or the one
/// the engine makes from its ring as Linux frames every IB with.
#[test]
fn vram_the_cpu_wrote_is_stale_for_an_engine_until_an_hdp_flush() {
    let rig = Rig::new();
    let (mut s, mut m, _) = up(&rig);
    let l = s.layout.clone().unwrap();
    let r = s.bump.as_mut().unwrap().take("test", 3 * 4096, 4096).unwrap();
    let (src, dst) = (l.mc(r.start), l.mc(r.start + 4096));
    let fresh = [0x5Au8; 64];
    assert!(m.mem_write(r.start, &fresh));
    let mut c = Vec::new();
    pkt7::copy_linear(&mut c, src, dst, 64).unwrap();
    // No flush: the engine copies what lay there before (zeros).
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &c));
    let mut got = [0xFFu8; 64];
    m.vram_peek(r.start + 4096, &mut got);
    assert_eq!(got, [0u8; 64], "the engine saw the CPU's bytes without an HDP flush");
    // The in-ring flush Linux emits (the request register's bit, the done register polled), then the same copy.
    let (done, req) = sr(&rig).hdp_flush_dwords().unwrap();
    let mut f = Vec::new();
    pkt::poll_regmem(&mut f, &pkt::PollRegMem { hdp_flush: true, mem: false, function: 3, addr_lo: done << 2, addr_hi: req << 2, reference: 0x400, mask: 0x400, interval: 10, retry: 0xFFF }).unwrap();
    f.extend_from_slice(&c);
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &f));
    m.vram_peek(r.start + 4096, &mut got);
    assert_eq!((got, m.sdma.ring_flushes), (fresh, 1));
    // A second write, the driver's own flush register this time.
    assert!(m.mem_write(r.start, &[0xC3; 64]));
    assert!(m.reg_write(nbreg::REMAPPED_MEM_FLUSH_BYTE, 0));
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 1, &c));
    m.vram_peek(r.start + 4096, &mut got);
    assert_eq!(got, [0xC3u8; 64]);
    assert!(m.refused.is_empty() && m.sdma.e.iter().all(|e| e.halted.is_none()), "{:?}", m.refused);
}
