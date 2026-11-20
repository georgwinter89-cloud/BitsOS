//! Wave 43: the model's SDMA engines and the memory they reach (`rdna4::fakesdmaeng`, `fakesdma`): every address
//! goes through the hub, in the kernel's address space only; an address with no valid, flushed entry is a fault
//! raised to GFXHUB's source and stops the engine, also when it is the engine's own read-pointer slot; and no
//! ring or slot may lie on another one's memory. Each refusal stands beside the step the model takes.

use super::rdna4fix::*;
use super::rdna4sdma::{sr, up};
use super::rdna4sdmafake::{b, push, refused, sys32, took};
use crate::gpucore::psp::Hw;
use crate::rdna4::fakeih::ModelIo;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::fakesdma::STATUS_IDLE;
use crate::rdna4::guard::Mc;
use crate::rdna4::host::Host;
use crate::rdna4::nbregs::NbRegs;
use crate::rdna4::sdmaih;
use crate::rdna4::sdmaq::lay;
use amdgpu_fmt::{sdma as pkt, sdma7 as pkt7};
use rdna4_core::ih7::{Iv, Ring};
use rdna4_core::{fault12, sdma7};
use std::vec::Vec;

/// Every address of an engine goes through the hub: one with no valid, flushed entry is a fault raised to the
/// hub's fault source and stops the engine; never a silent success.
#[test]
fn an_address_without_a_valid_entry_is_a_fault_raised_to_the_hub() {
    let rig = Rig::new();
    let (mut s, mut m, _) = up(&rig);
    let up_place = s.sdma.up.as_ref().unwrap().place;
    // The stage's test pages are in no entry: a write there, a read from there.
    let unbound = up_place.mc + lay::TEST;
    let mut w = Vec::new();
    pkt::write_linear(&mut w, unbound, &[0x1234_5678]).unwrap();
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &w));
    assert_eq!(m.sdma.hub_faults, [(unbound, true)]);
    assert!(m.sdma.e[0].halted.as_ref().is_some_and(|h| h.contains("Fault(MemFault { vmid: 0, addr: ")), "{:?}", m.sdma.e[0].halted);
    assert_eq!(sys32(&mut m, up_place.sys + lay::TEST), 0, "nothing was written behind the unbound address");
    // It stands in GFXHUB's status until somebody reads and clears it, and arrived in the IH as GFXHUB's fault
    // (client 0x0a, source 0: GC 12.0.1) through ring 1, with the IH's own entry on ring 0 (`ih`'s redirect).
    // The IH's reader hands it to the fault decode (`ihfault`), which says it in plain words.
    assert_eq!(m.faults_open(), [true, false]);
    let ih = s.nb.ih.as_mut().unwrap();
    let sys = ih.place.sys;
    let passes = ih.reader.process(&mut ModelIo { m: &mut m, sys });
    assert_eq!((passes[0].entries, passes[1].entries, ih.reader.faults.count, m.faults_open()), (1, 1, [1, 0], [false, false]));
    assert_eq!(ih.reader.unclaimed.get(&(fault12::CLIENT_GFX, fault12::SRC_FAULT)), None, "the fault has a taker");
    let said = std::format!("{}", ih.reader.faults.last.unwrap());
    assert_eq!(said, std::format!("gfxhub page fault at {unbound:#018x} (vmid 0, pasid 0): SDMA0 (client id 0xd) tried to write; the page does not allow it (permission faults 0x5)"));
    let mut c = Vec::new();
    pkt7::copy_linear(&mut c, unbound + 4096, up_place.ib_mc(), 64).unwrap();
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 1, &c));
    assert_eq!(m.sdma.hub_faults, [(unbound, true), (unbound + 4096, false)]);
    let ih = s.nb.ih.as_mut().unwrap();
    ih.reader.process(&mut ModelIo { m: &mut m, sys });
    let said = std::format!("{}", ih.reader.faults.last.unwrap());
    assert_eq!(said, std::format!("gfxhub page fault at {:#018x} (vmid 0, pasid 0): SDMA1 (client id 0xe) tried to read; the page does not allow it (permission faults 0x3)", unbound + 4096));
    assert_eq!((ih.reader.faults.count, ih.reader.unclaimed.get(&(0x1B, 0))), ([2, 0], None), "the UTCL2 client is GC 12.1.0's (gmc_v12_0.c:873-876)");
    assert_eq!(m.refused.len(), 2);
    // An address in neither the GART nor VRAM, and a write into what the card keeps in VRAM.
    let (s, mut m, _) = up(&rig);
    let l = s.layout.clone().unwrap();
    let mut w = Vec::new();
    pkt::write_linear(&mut w, 0x40_0000_0000, &[1]).unwrap();
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &w) && m.sdma.hub_faults == [(0x40_0000_0000, true)]);
    let table = l.mc(s.nb.gart.as_ref().unwrap().table.start);
    let mut w = Vec::new();
    pkt::write_linear(&mut w, table + 64, &[0xFFFF_FFFF]).unwrap();
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 1, &w));
    assert!(m.refused.iter().any(|r| r.contains("an SDMA engine told to write into the GART table")), "{:?}", m.refused);
    // The picture: an engine told to fill it.
    let (s, mut m, _) = up(&rig);
    let mut f = Vec::new();
    pkt7::const_fill(&mut f, l.mc(4096), 0, 4096).unwrap();
    assert!(push(&mut m, s.sdma.up.as_ref().unwrap(), 0, &f));
    assert!(m.refused.iter().any(|r| r.contains("an SDMA engine told to write into the firmware's picture")), "{:?}", m.refused);
    // The stage's whole bind unbound and flushed: the poll slot's page is gone with it, and a doorbell's value has
    // nothing to stand on (the read-pointer slot alone is the test after the next).
    let (mut s, m, _) = up(&rig);
    let n = NbRegs::from(&rig.att.discovery, &super::rdna4nb::regs(&rig)).unwrap();
    // Both hubs' flush, as every unbind makes it once GFXHUB is on: the engines' walker is GFXHUB's.
    let flush = s.flush_regs(&n).unwrap();
    assert!(flush.gc.is_some());
    let mc = s.sdma.up.as_ref().unwrap().place.mc;
    let mut card = Mc { hw: m, base: l.mc_base, bytes: l.vram_bytes };
    s.nb.gart.as_mut().unwrap().unmap(&mut card, &flush, mc, &mut Vec::new()).unwrap();
    let mut m = card.hw;
    assert!(!m.doorbell_write64(0x200, 0x80), "the poll slot's page is gone: the doorbell's value has nothing to stand on");
    assert!(m.refused.pop().unwrap().contains("write-pointer poll slot holds None"));
}

/// Only the kernel's address space exists for these engines (VMID 0 on the GART's table): an IB the ring names
/// in another VMID faults at its first fetch and reaches no memory, where the same IB in VMID 0 runs.
#[test]
fn an_ib_in_another_vmid_is_a_fault_and_reaches_no_memory() {
    let rig = Rig::new();
    let (s, mut m, _) = up(&rig);
    let up = s.sdma.up.as_ref().unwrap();
    // The IB writes the last slot of the write-back page, which no ring holds.
    let (slot_mc, slot_sys) = (up.place.wb_mc() + 4096 - 32, up.place.sys + lay::WB + 4096 - 32);
    let mut ib = Vec::new();
    pkt::write_linear(&mut ib, slot_mc, &[0x1234_5678]).unwrap();
    ib.resize(8, pkt::NOP);
    let bytes: Vec<u8> = ib.iter().flat_map(|d| d.to_le_bytes()).collect();
    assert!(m.sys_write(up.place.sys + lay::IB, &bytes));
    let call = |vmid| {
        let mut w = Vec::new();
        pkt::indirect(&mut w, vmid, up.place.ib_mc(), 8, 0).unwrap();
        w
    };
    assert!(push(&mut m, up, 0, &call(1)));
    assert!(m.sdma.e[0].halted.as_ref().is_some_and(|h| h.contains("Fault(MemFault { vmid: 1, addr: ")), "{:?}", m.sdma.e[0].halted);
    assert_eq!((m.sdma.hub_faults.clone(), sys32(&mut m, slot_sys), m.refused.len()), (std::vec![(up.place.ib_mc(), false)], 0, 1));
    assert_eq!(m.reg_read(b(&rig, sdma7::STATUS_REG, 0)), Some(0));
    // The same IB in the kernel's VMID, on the other engine: it runs and its write lands.
    assert!(push(&mut m, up, 1, &call(0)));
    assert_eq!((m.sdma.e[1].halted.clone(), sys32(&mut m, slot_sys), m.refused.len()), (None, 0x1234_5678, 1));
    assert_eq!(m.reg_read(b(&rig, sdma7::STATUS_REG, 1)), Some(STATUS_IDLE));
}

/// The read pointer goes back through the hub like every access of the engine. With its slot on a page that is
/// in no entry the engine cannot say where it stands: a fault to the hub, and it stops.
#[test]
fn a_read_pointer_slot_that_is_not_bound_stops_the_engine() {
    let rig = Rig::new();
    let (mut s, mut m, _) = up(&rig);
    let up = s.sdma.up.as_ref().unwrap();
    let slot = up.place.slot_sys(up.q[0].slots.rptr);
    // The slot's address moved onto the stage's test pages, which are not bound.
    let unbound = up.place.mc + lay::TEST;
    took(&mut m, b(&rig, sdma7::RB_RPTR_ADDR_LO, 0), unbound as u32);
    assert!(push(&mut m, up, 0, &[pkt::NOP]));
    // The ring itself ran (its pages are bound); the report did not arrive, and the old slot holds what it held.
    assert_eq!((m.sdma.e[0].rptr, sys32(&mut m, slot), sys32(&mut m, up.place.sys + lay::TEST)), (0x80, 0x40, 0));
    assert_eq!(m.sdma.e[0].halted.as_deref(), Some(std::format!("SDMA0's read-pointer slot at mc {unbound:#x} is not bound any more").as_str()));
    assert_eq!((m.sdma.hub_faults.clone(), m.refused.len()), (std::vec![(unbound, true)], 1));
    assert_eq!(m.reg_read(b(&rig, sdma7::STATUS_REG, 0)), Some(0));
    let ih = s.nb.ih.as_mut().unwrap();
    let sys = ih.place.sys;
    let passes = ih.reader.process(&mut ModelIo { m: &mut m, sys });
    assert_eq!((passes[0].entries, passes[1].entries, ih.reader.faults.count), (1, 1, [1, 0]));
    assert!(std::format!("{}", ih.reader.faults.last.unwrap()).contains("SDMA0 (client id 0xd) tried to write; the page does not allow it"), "{}", ih.reader.faults.last.unwrap());
    // More work is taken by the doorbell and not run.
    assert!(push(&mut m, up, 0, &[pkt::NOP]));
    assert_eq!((m.sdma.e[0].rptr, m.sdma.e[0].wptr, m.sdma.hub_faults.len()), (0x80, 0xC0, 1));
    // The other engine's slot is bound: it runs and reports.
    assert!(push(&mut m, up, 1, &[pkt::NOP]));
    assert_eq!((m.sdma.e[1].halted.clone(), sys32(&mut m, up.place.slot_sys(up.q[1].slots.rptr))), (None, 0x80));
}

/// Linux gives every ring its own pages and every slot its own place. A ring or a slot on memory another one
/// uses is refused although every page of it is bound and inside the GART (the trace's address rules hold for
/// it): two engines on one ring would each fetch the other's packets.
#[test]
fn a_ring_or_a_slot_on_another_ones_memory_is_refused() {
    let rig = Rig::new();
    let (s, mut m, _) = up(&rig);
    let up = s.sdma.up.as_ref().unwrap();
    let r = |x| b(&rig, x, 1);
    let (on, off) = (0x0084_1817, 0x0084_1816);
    // Engine 1 switched off and set up again as Linux sets a ring up, with these three addresses.
    let set = |m: &mut Model, ring: u64, rptr: u64, poll: u64| {
        took(m, r(sdma7::RB_CNTL), off);
        took(m, r(sdma7::MINOR_PTR_UPDATE), 1);
        for reg in [sdma7::RB_RPTR, sdma7::RB_RPTR_HI, sdma7::RB_WPTR, sdma7::RB_WPTR_HI, sdma7::RB_BASE_HI, sdma7::RB_RPTR_ADDR_HI, sdma7::RB_WPTR_POLL_ADDR_HI] {
            took(m, r(reg), 0);
        }
        took(m, r(sdma7::RB_BASE), (ring >> 8) as u32);
        took(m, r(sdma7::RB_RPTR_ADDR_LO), rptr as u32);
        took(m, r(sdma7::RB_WPTR_POLL_ADDR_LO), poll as u32);
    };
    let p = up.place;
    let slots = |i: usize| (p.slot_mc(up.q[i].slots.rptr), p.slot_mc(up.q[i].slots.wptr));
    let (ring0, (rptr0, poll0)) = (p.ring_mc(0), slots(0));
    let (ring1, (rptr1, poll1)) = (p.ring_mc(1), slots(1));
    let cases = [
        // The other engine's ring (also its second page alone), and each of its slots.
        ((ring0, rptr1, poll1), "SDMA1's ring at mc 0x", "overlapping SDMA0's ring at mc 0x"),
        ((ring0 + 4096, rptr1, poll1), "SDMA1's ring at mc 0x", "overlapping SDMA0's ring at mc 0x"),
        ((ring1, rptr0, poll1), "SDMA1's read-pointer slot at mc 0x", "overlapping SDMA0's read-pointer slot at mc 0x"),
        ((ring1, rptr1, poll0), "SDMA1's write-pointer poll slot at mc 0x", "overlapping SDMA0's write-pointer poll slot at mc 0x"),
        ((ring1, poll0, poll1), "SDMA1's read-pointer slot at mc 0x", "overlapping SDMA0's write-pointer poll slot at mc 0x"),
        // Its own: a slot inside its ring, both slots in one place.
        ((ring1, ring1 + 0x100, poll1), "SDMA1's ring at mc 0x", "overlapping SDMA1's read-pointer slot at mc 0x"),
        ((ring1, rptr1, rptr1), "SDMA1's read-pointer slot at mc 0x", "overlapping SDMA1's write-pointer poll slot at mc 0x"),
    ];
    for ((ring, rptr, poll), mine, theirs) in cases {
        set(&mut m, ring, rptr, poll);
        refused(&mut m, r(sdma7::RB_CNTL), on, mine);
        refused(&mut m, r(sdma7::RB_CNTL), on, theirs);
    }
    // As the stage laid it out: the ring starts and runs.
    set(&mut m, ring1, rptr1, poll1);
    took(&mut m, r(sdma7::RB_CNTL), on);
    took(&mut m, r(sdma7::MINOR_PTR_UPDATE), 0);
    assert!(push(&mut m, up, 1, &[pkt::NOP]) && m.sdma.e[1].rptr == 0x40 && m.refused.is_empty(), "{:?}", m.refused);
    // An engine that does not run holds nothing: engine 0 off, and engine 1 may take its ring.
    took(&mut m, b(&rig, sdma7::RB_CNTL, 0), off);
    set(&mut m, ring0, rptr1, poll1);
    took(&mut m, r(sdma7::RB_CNTL), on);
    let _ = sr(&rig);
}

/// Linux's trap handler refuses an entry whose ring id names an engine the card does not have
/// (`sdma_v7_0_process_trap_irq`: "wrong ring_ID detected"): the session's reader counts it as refused, never as
/// an engine's trap and never as a source without a handler.
#[test]
fn a_trap_that_names_no_engine_of_this_card_is_refused_by_the_handler() {
    let rig = Rig::new();
    let (mut s, mut m, _) = up(&rig);
    let trap = |ring_id| Iv { client_id: sdma7::CLIENT_GFX, src_id: sdma7::SRC_TRAP, ring_id, ..Iv::default() };
    // Engine 1's queue 0, an engine 2 and an engine 15 (twice), and engine 1's queue 3, which Linux takes and
    // does nothing with.
    for ring_id in [0x10, 0x20, 0xF0, 0xF0, 0x13] {
        m.raise(Ring::Ring0, &trap(ring_id)).unwrap();
    }
    let ih = s.nb.ih.as_mut().unwrap();
    let sys = ih.place.sys;
    assert_eq!(ih.reader.process(&mut ModelIo { m: &mut m, sys })[0].entries, 5);
    assert_eq!((sdmaih::traps(ih, 0), sdmaih::traps(ih, 1), ih.reader.taken(10, 49, 0x13)), (0, 1, 1));
    assert_eq!((ih.reader.taken(10, 49, 0x20), ih.reader.taken(10, 49, 0xF0), ih.reader.unclaimed.get(&(10, 49))), (0, 0, None));
    let said = ih.reader.describe();
    assert!(said.contains("taken: sdma trap client 10 source 49 ring id 16: 1, sdma trap client 10 source 49 ring id 19: 1; refused by their handler (a ring id it does not know): sdma trap client 10 source 49 ring id 32: 1, sdma trap client 10 source 49 ring id 240: 2; no handler for: nothing"), "{said}");
    // The check is the trap's own: the user fence of the same client carries a doorbell offset, not an engine.
    let fence = Iv { src_id: sdma7::SRC_FENCE, ..trap(0x20) };
    assert!(!sdmaih::wrong_ring_id(&fence) && sdmaih::wrong_ring_id(&trap(0x20)) && !sdmaih::wrong_ring_id(&trap(0x1F)));
}
