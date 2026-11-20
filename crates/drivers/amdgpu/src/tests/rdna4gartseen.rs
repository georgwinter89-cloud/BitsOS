//! Wave 42: the model's GART as the card's walker sees it (`rdna4::fakegart`), and what else the model holds a
//! driver to that Linux does: a bound page counts only after Linux's HDP flush and invalidation; an acknowledge
//! answers one request; every page of a ring must be bound; an entry above the card's 44 bits is refused whenever
//! it was written and wherever the write began; the two registers behind the index pair take Linux's one change
//! each, DF's only with AtomicOp requester enable on.

use super::rdna4fix::*;
use super::rdna4nbmodel::{after, n, refused};
use crate::gpucore::psp::{self, Cond, Hw, LINUX_POLLS, PspError};
use crate::rdna4::fakenb::SYS_BASE;
use crate::rdna4::gart12::{self, FlushRegs};
use crate::rdna4::guard::{Allow, Guard, Mc, Refusal};
use crate::rdna4::host::Host;
use crate::rdna4::ihstage::HOST_BYTES;
use crate::rdna4::nb::pages_of;
use crate::rdna4::stage::Stage;
use rdna4_core::ih7::{self, Iv, Ring};
use rdna4_core::nbreg::member;
use rdna4_core::{hub41, pte12};
use std::vec::Vec;

/// Ring 0's control with base and size named, switched on.
fn ring_on() -> u32 {
    ih7::toggled(Ring::Ring0, ih7::rb_cntl(Ring::Ring0, 0x4061_0000, true), true)
}

#[test]
fn a_bound_page_counts_only_after_linuxs_flush_and_invalidation() {
    let rig = Rig::new();
    let n = n(&rig);
    let b = |r| n.byte(r).unwrap();
    let (mut s, m) = after(&rig, &[Stage::Nbif, Stage::Gmc]);
    let (base, bytes) = s.layout.as_ref().map(|l| (l.mc_base, l.vram_bytes)).unwrap();
    let f = FlushRegs::of(&n, s.nb.hdp_flush(n.hdp_flush)).unwrap();
    let gart = s.nb.gart.as_mut().unwrap();
    let mut card = Mc { hw: m, base, bytes };
    let sys = card.pages(HOST_BYTES).unwrap();
    // Bound, and nothing after it: the table in VRAM does not hold the entries yet.
    let mc = gart.bind(&mut card, "IH rings", &pages_of(sys, HOST_BYTES)).unwrap();
    assert_eq!(card.hw.nb.entries_pending(), 1);
    assert!(card.reg_write(b(ih7::RB_BASE), (mc >> 8) as u32) && card.reg_write(b(ih7::RB_BASE_HI), 0) && card.reg_write(b(ih7::RB_CNTL), ring_on() & !1));
    // The write-back slot behind both rings, as the stage places it.
    assert!(card.reg_write(b(ih7::RB_WPTR_ADDR_LO), (mc + 2 * u64::from(ih7::RING_BYTES)) as u32) && card.reg_write(b(ih7::RB_WPTR_ADDR_HI), 0));
    refused(&mut card.hw, b(ih7::RB_CNTL), ring_on(), "its pages are not bound in the GART (page 0, as the card's walker sees the table now)");
    // The HDP flush alone: the entries are in VRAM, the hub still holds what it held.
    gart12::flush_hdp(&mut card, &f).unwrap();
    assert_eq!(card.hw.nb.entries_pending(), 1);
    refused(&mut card.hw, b(ih7::RB_CNTL), ring_on(), "its pages are not bound in the GART");
    // Linux's flush after a bind: now the ring may start.
    let mut out = Vec::new();
    gart12::invalidate_tlb(&mut card, &f, &mut out).unwrap();
    assert_eq!(card.hw.nb.entries_pending(), 0);
    assert!(card.reg_write(b(ih7::RB_CNTL), ring_on()) && card.hw.nb.ih_on[0]);
    card.hw.raise(Ring::Ring0, &Iv { client_id: 4, src_id: 9, ..Iv::default() }).unwrap();
    let mut first = [0u8; 4];
    assert!(card.sys_read(sys, &mut first) && first == [4, 9, 0, 0]);
    // The other way round: pages taken out of the table are still written by the card until the flush. A driver
    // that frees them before it would hand the card somebody else's memory.
    gart.unbind(&mut card, mc).unwrap();
    card.hw.raise(Ring::Ring0, &Iv { client_id: 20, src_id: 181, ..Iv::default() }).unwrap();
    assert!(card.sys_read(sys + 32, &mut first) && first == [20, 181, 0, 0], "the card still reaches the page");
    gart12::invalidate_tlb(&mut card, &f, &mut out).unwrap();
    assert_eq!(card.hw.raise(Ring::Ring0, &Iv::default()), Err(std::string::String::from("the ring's page is not bound")));
    // An invalidation with no HDP flush before it leaves a later write unseen: only what reached VRAM counts.
    let again = gart.bind(&mut card, "IH rings", &pages_of(sys, HOST_BYTES)).unwrap();
    assert_eq!(again, mc);
    let eng = |r| b(member(r, hub41::GART_ENGINE, 1));
    assert_eq!(card.reg_read(eng(hub41::INVALIDATE_ENG0_SEM)), Some(1));
    assert!(card.reg_write(eng(hub41::INVALIDATE_ENG0_REQ), hub41::invalidate_req(0)) && card.reg_write(eng(hub41::INVALIDATE_ENG0_SEM), 0));
    assert_eq!(card.hw.nb.entries_pending(), 1);
    assert!(card.hw.raise(Ring::Ring0, &Iv::default()).is_err());
    assert!(out.is_empty() && card.hw.refused.len() == 2, "{:?}", card.hw.refused);
}

/// Linux's `mb()` before the flush after a bind (amdgpu_gart.c:499): the model takes no HDP flush while entries of
/// the table were stored and no barrier came since. The barrier reaches the card through what stands between a
/// stage and it: the guard with the stage's list and the MC translation. An entry stored as 8 bytes at once goes
/// through the same list (no stage does that yet).
#[test]
fn table_writes_need_a_store_barrier_before_the_flush() {
    let rig = Rig::new();
    let n = n(&rig);
    let (s, m) = after(&rig, &[Stage::Nbif, Stage::Gmc]);
    let (base, bytes) = s.layout.as_ref().map(|l| (l.mc_base, l.vram_bytes)).unwrap();
    let f = FlushRegs::of(&n, s.nb.hdp_flush(n.hdp_flush)).unwrap();
    let table = s.nb.gart.as_ref().unwrap().table_mc();
    let (barriers, regs) = (m.nb.barriers, m.regs.clone());
    assert_eq!(barriers, 1, "gmc's own, after it filled the table");
    let list = Allow { stage: "test", regs: f.written().to_vec(), mem: std::vec![table.clone()] };
    let mut card = Guard::new(Mc { hw: m, base, bytes }, list, regs);
    let entry = pte12::bound(SYS_BASE).unwrap();
    assert!(card.mem_write(table.start + 8, &entry.to_le_bytes()));
    assert_eq!(gart12::flush_hdp(&mut card, &f), Err(PspError::WriteRefused { byte: f.hdp }));
    assert!(card.hw.hw.refused.last().unwrap().contains("no store barrier"), "{:?}", card.hw.hw.refused);
    card.store_barrier();
    gart12::flush_hdp(&mut card, &f).unwrap();
    assert_eq!(card.hw.hw.nb.barriers, barriers + 1);
    // One entry at once: inside the stage's list it lands, outside it is refused before the card.
    assert!(card.mem_write64(table.start + 16, pte12::bound(SYS_BASE + 4096).unwrap()));
    assert!(!card.mem_write64(table.end, entry) && !card.mem_write64(table.start + 4, entry));
    assert_eq!(card.refused, [Refusal::Mem { mc: table.end, bytes: 8 }]);
    assert_eq!(gart12::flush_hdp(&mut card, &f), Err(PspError::WriteRefused { byte: f.hdp }), "that store is a table write too");
    card.store_barrier();
    gart12::flush_hdp(&mut card, &f).unwrap();
    let mut e = [0u8; 16];
    card.hw.hw.vram_peek(table.start - base + 8, &mut e);
    assert_eq!(e[..8], entry.to_le_bytes());
    assert_eq!(e[8..], pte12::bound(SYS_BASE + 4096).unwrap().to_le_bytes());
    assert_eq!(card.hw.hw.refused.len(), 2);
}

#[test]
fn an_acknowledge_answers_one_request() {
    let rig = Rig::new();
    let n = n(&rig);
    let eng = |r| n.byte(member(r, hub41::GART_ENGINE, 1)).unwrap();
    let (sem, req, ack) = (eng(hub41::INVALIDATE_ENG0_SEM), eng(hub41::INVALIDATE_ENG0_REQ), eng(hub41::INVALIDATE_ENG0_ACK));
    let (_, mut m) = after(&rig, &[Stage::Nbif, Stage::Gmc]);
    let bit0 = Cond::Masked { mask: 1, value: 1 };
    // gmc's flush is over: a wait for the acknowledge with no request of its own runs to Linux's bound.
    let t0 = m.card.now_us;
    assert!(matches!(psp::wait_reads(&mut m, ack, bit0, LINUX_POLLS, "acknowledge"), Err(PspError::Timeout { last: 0, .. })));
    assert!(m.card.now_us - t0 >= u64::from(LINUX_POLLS) - 1, "{}", m.card.now_us - t0);
    // Neither does taking the semaphore bring one.
    assert_eq!(psp::wait_reads(&mut m, sem, bit0, LINUX_POLLS, "semaphore"), Ok(1));
    assert!(psp::wait_reads(&mut m, ack, bit0, 16, "acknowledge").is_err());
    // The request does, at once, as the card answered Linux.
    assert!(m.reg_write(req, hub41::invalidate_req(0)));
    assert_eq!(psp::wait_reads(&mut m, ack, bit0, LINUX_POLLS, "acknowledge"), Ok(0x0001_0001));
    assert!(m.reg_write(sem, 0) && m.refused.is_empty());
    assert!(psp::wait_reads(&mut m, ack, bit0, 16, "acknowledge").is_err());
}

#[test]
fn every_page_of_a_ring_must_be_bound() {
    let rig = Rig::new();
    let n = n(&rig);
    let b = |r| n.byte(r).unwrap();
    let (s, m) = after(&rig, &[Stage::Nbif, Stage::Gmc]);
    let (base, bytes) = s.layout.as_ref().map(|l| (l.mc_base, l.vram_bytes)).unwrap();
    let f = FlushRegs::of(&n, s.nb.hdp_flush(n.hdp_flush)).unwrap();
    let gart = s.nb.gart.as_ref().unwrap();
    let mut card = Mc { hw: m, base, bytes };
    let sys = card.pages(u64::from(ih7::RING_BYTES)).unwrap();
    let entry = |i: u64| pte12::bound(sys + i * pte12::PAGE).unwrap().to_le_bytes();
    let at = |page: u64| gart.table_mc().start + 8 * page;
    let (mut out, last) = (Vec::new(), u64::from(ih7::RING_BYTES) / pte12::PAGE - 1);
    // A ring at GART page 1 with only its first and its last page bound. Entries by hand, so Linux's `mb()` by
    // hand before each flush (the model takes no flush without it).
    assert!(card.mem_write(at(1), &entry(0)) && card.mem_write(at(1 + last), &entry(last)));
    card.store_barrier();
    gart12::invalidate_tlb(&mut card, &f, &mut out).unwrap();
    assert!(card.reg_write(b(ih7::RB_BASE), 0x1000 >> 8) && card.reg_write(b(ih7::RB_BASE_HI), 0) && card.reg_write(b(ih7::RB_CNTL), ring_on() & !1));
    refused(&mut card.hw, b(ih7::RB_CNTL), ring_on(), "its pages are not bound in the GART (page 1,");
    // All but one in the middle.
    for i in (1..last).filter(|i| *i != 40) {
        assert!(card.mem_write(at(1 + i), &entry(i)));
    }
    card.store_barrier();
    gart12::invalidate_tlb(&mut card, &f, &mut out).unwrap();
    refused(&mut card.hw, b(ih7::RB_CNTL), ring_on(), "its pages are not bound in the GART (page 40,");
    assert!(card.mem_write(at(41), &entry(40)));
    card.store_barrier();
    gart12::invalidate_tlb(&mut card, &f, &mut out).unwrap();
    assert!(card.reg_write(b(ih7::RB_CNTL), ring_on()) && card.hw.nb.ih_on[0] && out.is_empty());
}

#[test]
fn an_entry_above_44_bits_is_refused_whenever_and_wherever_it_was_written() {
    let rig = Rig::new();
    let n = n(&rig);
    let high = pte12::entry(1 << 44, pte12::BOUND_FLAGS).to_le_bytes();
    let fine = pte12::bound(SYS_BASE).unwrap().to_le_bytes();
    // Written before the card was told where the table lies, as Linux fills its table: refused when context 0 is
    // switched on.
    let table = 64u64 << 20;
    let ctx0 = [hub41::CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32, hub41::CONTEXT0_PAGE_TABLE_BASE_ADDR_HI32, hub41::CONTEXT0_PAGE_TABLE_START_ADDR_LO32, member(hub41::CONTEXT0_PAGE_TABLE_START_ADDR_LO32, 1, 1), hub41::CONTEXT0_PAGE_TABLE_END_ADDR_LO32, member(hub41::CONTEXT0_PAGE_TABLE_END_ADDR_LO32, 1, 1)];
    let cntl = n.byte(hub41::CONTEXT0_CNTL).unwrap();
    for (entry, asked, why) in [(high, true, "GART entry 5 points at 0x100000000000, above the card's 44 bits"), (fine, false, "a system page put into the GART before the DMAR survey allowed it")] {
        let mut m = model();
        assert!(!asked || m.dma_survey().is_ok());
        assert!(m.mem_write(table, &[pte12::FILL_BYTE; 64]) && m.mem_write(table + 8 * 5, &entry) && m.refused.is_empty());
        for (i, r) in ctx0.iter().enumerate() {
            assert!(m.reg_write(n.byte(*r).unwrap(), if i == 0 { table as u32 | 1 } else { 0 }));
        }
        assert_eq!(m.nb.table(), Some(table));
        refused(&mut m, cntl, 0x03FF_FC01, why);
        assert!(!m.nb.context0);
        // Put right, the context starts.
        assert!(m.mem_write(table + 8 * 5, &[pte12::FILL_BYTE; 8]) && m.reg_write(cntl, 0x03FF_FC01) && m.nb.context0);
    }
    // With the table's place known a write is judged on the table's grid, whatever its own start: the upper half
    // of an entry alone, and a write that begins before the table.
    let (s, mut m) = after(&rig, &[Stage::Nbif, Stage::Gmc]);
    let t = s.nb.gart.as_ref().unwrap().table.start;
    assert!(m.mem_write(t + 8 * 9, &fine));
    assert!(!m.mem_write(t + 8 * 9 + 4, &high[4..]) && m.refused.last().unwrap().contains("GART entry 9 points at 0x100056780000, above the card's 44 bits"), "{:?}", m.refused);
    let mut across = [0u8; 12];
    across[4..].copy_from_slice(&high);
    assert!(!m.mem_write(t - 4, &across) && m.refused.last().unwrap().contains("GART entry 0 points at 0x100000000000"), "{:?}", m.refused);
    // The last entry, by a write that runs past the table's end.
    let end = s.nb.gart.as_ref().unwrap().table.end;
    across[..8].copy_from_slice(&high);
    assert!(!m.mem_write(end - 8, &across) && m.refused.last().unwrap().contains("GART entry 131071 points at"), "{:?}", m.refused);
    assert_eq!(m.refused.len(), 3);
}

#[test]
fn the_registers_behind_the_index_pair_take_linuxs_one_change_each() {
    let rig = Rig::new();
    let n = n(&rig);
    let mut m = model();
    // DF's register: Linux writes it only with have_atomics_support, which is AtomicOp requester enable.
    assert!(m.reg_write(n.index, n.df));
    refused(&mut m, n.data, 0x0A41_0031, "written with AtomicOp requester enable off");
    assert!(m.atomics().is_ok() && m.nb.atomics_req);
    refused(&mut m, n.data, 0x0A41_0131, "Linux sets DisIntAtomicsLclProcessing of 0xa400001 and nothing else");
    assert!(m.reg_write(n.data, 0x0A41_0031) && m.nb.df == 0x0A41_0031);
    // Written again it is the same value (a stage that runs again after a stop).
    assert!(m.reg_write(n.data, 0x0A41_0031));
    // The strap: bit 7 cleared, nothing else.
    assert!(m.reg_write(n.index, n.strap2));
    refused(&mut m, n.data, 0x0313_2001, "Linux clears STRAP_NO_SOFT_RESET_DEV0_F2 of 0x3132000 and nothing else");
    m.nb.strap2 = 0x0313_2080;
    refused(&mut m, n.data, 0x0313_2080, "Linux clears");
    assert!(m.reg_write(n.data, 0x0313_2000) && m.nb.strap2 == 0x0313_2000);
    // A path that does not grant AtomicOps: requester enable stays off, and so does the model's DF.
    let mut m = model();
    m.nb.atomics_path = Err("the switch port 0000:02:00.0 does not route AtomicOps");
    assert!(m.atomics().is_err() && !m.nb.atomics_req && m.nb.host_changes.is_empty());
}
