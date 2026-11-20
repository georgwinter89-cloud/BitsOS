//! Wave 43, item HB: the graphics block's start, GFXHUB as the second register set of the one hub, and a page
//! fault's words, against the trace of Linux's own start (plan/hw/gaming-pc-gpu-2026-11-18, init.txt lines
//! 9151-9361: the sections gc-boot and gc-hub) and Linux's print functions.

use crate::fault12::{self, Fault, RateLimit, st};
use crate::gc12::{self, Start, Starts};
use crate::hub41::{self, Chosen, Kind, Op};
use crate::ih7::Iv;
use crate::ipfacts::TABLE;
use crate::nbreg::member;
use crate::regmap::dword;
use std::format;
use std::string::String;
use std::vec::Vec;

#[test]
fn the_gc_registers_lie_where_the_trace_names_them() {
    let g = &hub41::GC;
    for (r, dw) in [
        (gc12::CP_STAT, 0x21A0),
        (gc12::RLC_RLCS_BOOTLOAD_STATUS, 0xEE7C),
        (gc12::DB_MEM_CONFIG, 0x2632),
        (gc12::GB_ADDR_CONFIG, 0x263E),
        (gc12::GRBM_GFX_CNTL, 0xA900),
        (gc12::CP_PFP_PRGRM_CNTR_START, 0x30A4),
        (gc12::CP_PFP_PRGRM_CNTR_START_HI, 0x30B9),
        (gc12::CP_ME_PRGRM_CNTR_START, 0x30A5),
        (gc12::CP_ME_PRGRM_CNTR_START_HI, 0x30D9),
        (gc12::CP_ME_CNTL, 0xA803),
        (gc12::CP_MEC_RS64_PRGRM_CNTR_START, 0xC900),
        (gc12::CP_MEC_RS64_PRGRM_CNTR_START_HI, 0xC938),
        (gc12::CP_MEC_RS64_CNTL, 0xC904),
        (gc12::CP_DEBUG, 0x307F),
        (g.ctx0_base_lo, 0x28EF),
        (g.ctx0_base_hi, 0x28F0),
        (g.ctx0_start_lo, 0x290F),
        (g.ctx0_end_lo, 0x292F),
        (g.agp_base, 0x2878),
        (g.agp_bot, 0x2877),
        (g.agp_top, 0x2876),
        (g.aperture_low, 0x2879),
        (g.aperture_high, 0x287A),
        (g.default_addr_lsb, 0x2808),
        (g.default_addr_msb, 0x2809),
        (g.fault_default_lo, 0x2834),
        (g.fault_default_hi, 0x2835),
        (g.fault_cntl, 0x282C),
        (g.fault_cntl2, 0x282D),
        (g.fault_status, 0x2830),
        (g.l1_tlb, 0x287B),
        (g.l2_cntl, 0x2824),
        (g.l2_cntl2, 0x2825),
        (g.l2_cntl3, 0x2826),
        (g.l2_cntl4, 0x283D),
        (g.l2_cntl5, 0x2843),
        (g.context0_cntl, 0x2884),
        (member(g.context1_cntl, 14, 1), 0x2893),
        (member(g.eng0_req, hub41::GART_ENGINE, 1), 0x28B8),
        (member(g.eng0_ack, hub41::GART_ENGINE, 1), 0x28CA),
        (member(g.eng0_range_lo, 17, 2), 0x28ED),
        // MMHUB's fault registers (mmhub_4_1_0_offset.h:746, :754).
        (hub41::MM.fault_status, 0x1A4F0),
        (hub41::MM.fault_cntl, 0x1A4EC),
    ] {
        assert_eq!(dword(&TABLE, r), Some(dw), "{}", r.name);
    }
    // The old names are MMHUB's set, and the two sets differ in exactly what Linux's two files differ in.
    assert_eq!((hub41::FAULT_CNTL, hub41::CONTEXT0_CNTL, Some(hub41::BANK_SELECT_RESERVED_CID2)), (hub41::MM.fault_cntl, hub41::MM.context0_cntl, hub41::MM.cid2));
    assert!(hub41::MM.semaphore && !g.semaphore && g.cid2.is_none() && hub41::MM.before_fault_enable.is_none());
    assert_eq!((hub41::MM.l2_cntl3_default, g.l2_cntl3_default), (0x8010_0007, 0x8012_0007));
    assert_eq!((hub41::l2_cntl3(&hub41::MM), hub41::l2_cntl3(g)), (0x8013_0009, 0x8013_0009), "init.txt lines 55 and 9215");
    assert!(Kind::Gc < Kind::Mm, "a flush of all hubs takes GFXHUB first (bit 0 of vmhubs_mask)");
}

/// One operation carried out against the card's answers of the trace: (dword, value) per write.
fn carry(op: Op, answer: &dyn Fn(u32) -> u32, out: &mut Vec<(u32, u32)>) {
    let dw = dword(&TABLE, op.reg()).unwrap();
    match op {
        Op::Write(_, v) => out.push((dw, v)),
        Op::Modify(_, f) => out.push((dw, f(answer(dw)))),
        Op::Pulse(_, bits) => out.extend([(dw, answer(dw) | bits), (dw, answer(dw) & !bits)]),
    }
}

/// The writes of a fixture's section in order, HDP flushes and the wait left out: (dword, value).
fn fixture_writes(text: &str) -> Vec<(u32, u32)> {
    text.lines()
        .filter_map(|l| {
            let t: Vec<&str> = l.split(' ').collect();
            let hex = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
            (t.get(1) == Some(&"W")).then(|| (hex(t[3]), hex(t[4])))
        })
        .collect()
}

const GC_BOOT: &str = include_str!("../../../amdgpu/src/rdna4/linuxseq/gc-boot.txt");
const GC_HUB: &str = include_str!("../../../amdgpu/src/rdna4/linuxseq/gc-hub.txt");

#[test]
fn the_engines_starts_and_pulses_are_the_traces() {
    // The headers' fields that give the trace's 0xc00 and 0x1c000 (lines 9157-9158): the low word 0x3000 to
    // 0x3003, the high word 0x70000, whose upper bits fall out of the 32-bit shift as in Linux's C.
    let s = Start { lo: 0x3000, hi: 0x7_0000 };
    assert_eq!((s.counter(), s.counter_hi()), (0xC00, 0x1_C000));
    assert_eq!(Start { lo: 0x3003, hi: 0x7_0000 }.counter(), 0xC00);
    assert_eq!(Start { lo: 0x10, hi: 0x3 }.counter(), 0xC000_0004, "the two low bits of the high word are bits 30 and 31");
    assert_eq!(Start { lo: u32::MAX, hi: u32::MAX }.counter_hi(), 0x3FFF_FFFF);
    assert_eq!([gc12::grbm_select(0, 0, 0, 0), gc12::grbm_select(0, 1, 0, 0), gc12::grbm_select(1, 0, 0, 0), gc12::grbm_select(1, 3, 0, 0)], [0, 1, 4, 7]);
    assert_eq!(gc12::grbm_select(1, 2, 3, 5), 0x356);
    // CP_ME_CNTL reads back what was written, reset bits included (line 9173 reads line 9165's value).
    let me_cntl = core::cell::Cell::new(0x1514_A000u32);
    let answer = |dw: u32| match dw {
        0xA803 => me_cntl.get(),
        0xC904 => 0x4003_0000,
        0x2632 => 0x8035,
        other => panic!("a read of {other:#x} is not in the trace"),
    };
    let mut w = Vec::new();
    carry(Op::Modify(gc12::DB_MEM_CONFIG, gc12::golden_db_mem_config), &answer, &mut w);
    gc12::config_gfx_rs64(&Starts { pfp: s, me: s, mec: s }, |op| -> Result<(), ()> {
        carry(op, &answer, &mut w);
        if op.reg().offset == gc12::CP_ME_CNTL.offset {
            me_cntl.set(w.last().unwrap().1);
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(w, fixture_writes(GC_BOOT), "every write of gc-boot, in order");
    assert_eq!(w.len(), 34);
    // No pulse touches a halt bit, and Linux leaves pipe 0 selected.
    assert_eq!(gc12::PFP_PIPE_RESETS | gc12::ME_PIPE_RESETS, 0x003C_0000);
    assert_eq!((gc12::GFX_HALTS, gc12::MEC_PIPE_RESETS, gc12::f::MEC_HALT.mask), (0x1500_0000, 0x000F_0000, 0x4000_0000));
    assert!(w.iter().filter(|(d, _)| *d == 0xA803).all(|(_, v)| v & gc12::GFX_HALTS == gc12::GFX_HALTS));
    assert_eq!(w.iter().rfind(|(d, _)| *d == 0xA900), Some(&(0xA900, 0)));
    assert!(gc12::autoload_complete(0, 0x8000_003F) && !gc12::autoload_complete(0, 0x3F) && !gc12::autoload_complete(0x8000_0000, 0x8000_003F));
    assert_eq!(gc12::golden_db_mem_config(0x35), 0x8035);
    assert_eq!(gc12::RS64_REGS.len(), 9);
    // The golden list of revision 0 is not built: this card is revision 1 (discovery line 6 reads STRAP0
    // 0x310c7550, whose bits 27:24 Linux takes as `rev_id`).
    assert_eq!((crate::devinfo::CHIP_REV, (0x310C_7550u32 >> 24) & 0xF), (1, 1));
}

#[test]
fn gfxhubs_start_writes_what_linux_wrote_and_mmhubs_values() {
    let l = hub41::layout(0x3FB0, 0x8000, 0).unwrap();
    let c = Chosen { table: 0x3_DAB0_0000, scratch: 0x3_DAAF_F000, dummy: 0x63FF_F000 };
    // What the card answered to each read of the section (lines 9207-9357).
    let gc_answer = |dw: u32| match dw {
        0x282D => 0x0002_0000,
        0x287B => 0x0000_0501,
        0x2824 => 0x0008_0602,
        0x2825 => 0,
        0x2884..=0x2893 => 0x03FF_FC00,
        0x307F => 0x2040_0000,
        0x282C => 0x3FFF_FFFC,
        other => panic!("a read of {other:#x} is not in the trace"),
    };
    let mut w = Vec::new();
    hub41::gart_enable(&hub41::GC, &l, &c, |op| -> Result<(), ()> {
        carry(op, &gc_answer, &mut w);
        Ok(())
    })
    .unwrap();
    assert_eq!(w.len(), 140, "lines 9192-9351 hold 140 writes");
    hub41::fault_enable_default(&hub41::GC, |op| -> Result<(), ()> {
        carry(op, &gc_answer, &mut w);
        Ok(())
    })
    .unwrap();
    assert_eq!(w[140..], [(0x307F, 0x2040_8000), (0x282C, 0x3FFF_FFFC)], "lines 9355 and 9357");
    w.push((0x28B8, hub41::invalidate_req(0)));
    assert_eq!(w, fixture_writes(GC_HUB), "every write of gc-hub, in order");
    // The same choices give MMHUB the same values at its own places, but for the one bit of FAULT_CNTL2 the two
    // hubs read differently (0x000a0000 there).
    let mm_answer = |dw: u32| match dw {
        0x1A4ED => 0x0002_0000,
        0x1A55B => 0x0000_0501,
        0x1A4E4 => 0x0008_0602,
        0x1A4E5 => 0,
        _ => 0x03FF_FC00,
    };
    let mut m = Vec::new();
    hub41::gart_enable(&hub41::MM, &l, &c, |op| -> Result<(), ()> {
        carry(op, &mm_answer, &mut m);
        Ok(())
    })
    .unwrap();
    assert_eq!(m.iter().map(|x| x.1).collect::<Vec<_>>(), w[..140].iter().map(|x| x.1).collect::<Vec<_>>());
    let (mut gc_regs, mut mm_regs) = (Vec::new(), Vec::new());
    hub41::gart_enable_regs(&hub41::GC, |r| gc_regs.push(r.name.replacen("GC", "", 1)));
    hub41::gart_enable_regs(&hub41::MM, |r| mm_regs.push(r.name.replacen("MM", "", 1)));
    assert_eq!(gc_regs, mm_regs, "one sequence over two sets: the same registers by name");
    // The fault control's two forms: GFXHUB's also writes the two crash bits as 0.
    assert_eq!((hub41::gc_fault_cntl(0xC000_0000), hub41::fault_cntl(0xC000_0000)), (0x0000_1FFC, 0xC000_1FFC));
    assert_eq!(hub41::fault_clear(0x3FFF_FFFC), 0x3FFF_FFFD);
}

#[test]
fn a_hub_is_switched_off_as_linux_switches_it_off() {
    for (h, cntl0, l1, l2, cntl3) in [(&hub41::GC, 0x2884, 0x287B, 0x2824, 0x2826), (&hub41::MM, 0x1A564, 0x1A55B, 0x1A4E4, 0x1A4E6)] {
        let answer = |dw: u32| if dw == l1 { 0x0000_1859 } else { 0x0008_0E01 };
        let mut w = Vec::new();
        hub41::gart_disable(h, |op| -> Result<(), ()> {
            carry(op, &answer, &mut w);
            Ok(())
        })
        .unwrap();
        let contexts: Vec<(u32, u32)> = (0..16).map(|i| (cntl0 + i, 0)).collect();
        assert_eq!(w[..16], contexts[..], "{}", h.name);
        assert_eq!(w[16..], [(l1, 0x0000_1818), (l2, 0x0008_0E00), (cntl3, 0)], "{}", h.name);
    }
}

fn fault_iv(client: u8, vmid_src: bool, addr: u64, flags: u32) -> Iv {
    Iv { client_id: client, src_id: 0, ring_id: if vmid_src { 0 } else { 1 }, vmid: 3, vmid_src, pasid: 0x8001, src_data: [(addr >> 12) as u32, (addr >> 44) as u32 & 0xF | flags, 0, 0], ..Iv::default() }
}

#[test]
fn a_fault_is_worded_as_linuxs_print_functions_word_it() {
    assert_eq!((fault12::taker(0x0A, 0), fault12::taker(0x12, 0), fault12::taker(0x0A, 1), fault12::taker(0x14, 0)), (Some(Kind::Gc), Some(Kind::Mm), None, None));
    // GFXHUB: SDMA0 (13) writing, a permission fault, more faults behind it.
    let status = st::CID.put(0, 13) | st::RW.put(0, 1) | st::PERMISSION_FAULTS.put(0, 2) | st::MORE_FAULTS.put(0, 1);
    let iv = fault_iv(fault12::CLIENT_GFX, false, 0x8123_4567_8000, fault12::DATA_WRITE);
    assert!(fault12::reads(&iv).dummy_first);
    let f = Fault::new(&iv, Kind::Gc, status);
    assert_eq!((f.addr, f.write, f.retry, f.client(), f.cid()), (0x8123_4567_8000, true, false, "SDMA0", 13));
    assert_eq!(
        format!("{}", f.linux()),
        "[gfxhub] page fault (src_id:0 ring:1 vmid:3 pasid:32769)\n  in page starting at address 0x0000812345678000 from client 10\nGCVM_L2_PROTECTION_FAULT_STATUS:0x00041A21\n\t Faulty UTCL2 client ID: SDMA0 (0xd)\n\t MORE_FAULTS: 0x1\n\t WALKER_ERROR: 0x0\n\t PERMISSION_FAULTS: 0x2\n\t MAPPING_ERROR: 0x0\n\t RW: 0x1"
    );
    assert_eq!(format!("{f}"), "gfxhub page fault at 0x0000812345678000 (vmid 3, pasid 32769): SDMA0 (client id 0xd) tried to write; the page does not allow it (permission faults 0x2), more faults followed it");
    // MMHUB: its clients go by id and read/write; VCNU (19) reading an unmapped page.
    let status = st::CID.put(0, 19) | st::MAPPING_ERROR.put(0, 1) | st::WALKER_ERROR.put(0, 5);
    let iv = fault_iv(fault12::CLIENT_VMC, true, 0x1000, fault12::DATA_READ);
    assert!(!fault12::reads(&iv).dummy_first);
    let f = Fault::new(&iv, Kind::Mm, status);
    assert_eq!(
        format!("{}", f.linux()),
        "[mmhub] page fault (src_id:0 ring:0 vmid:3 pasid:32769)\n  in page starting at address 0x0000000000001000 from client 18\nMMVM_L2_PROTECTION_FAULT_STATUS_LO32:0x0000270A\n\t Faulty UTCL2 client ID: VCNU (0x13)\n\t MORE_FAULTS: 0x0\n\t WALKER_ERROR: 0x5\n\t PERMISSION_FAULTS: 0x0\n\t MAPPING_ERROR: 0x1\n\t RW: 0x0"
    );
    assert_eq!(format!("{f}"), "mmhub page fault at 0x0000000000001000 (vmid 3, pasid 32769): VCNU (client id 0x13) tried to read; no page is mapped there (mapping error), the page table walk failed (walker error 0x5)");
    // Names: past either table and the table's empty rows are "unknown", as Linux prints.
    assert_eq!((fault12::GFX_CLIENTS.len(), fault12::GFX_CLIENTS[20], fault12::mm_client(55, false), fault12::mm_client(55, true), fault12::mm_client(56, false), fault12::mm_client(18, false), fault12::mm_client(18, true)), (21, "PA", Some("VCNRD"), None, None, None, Some("VCNWR")));
    assert_eq!(Fault::new(&iv, Kind::Gc, st::CID.put(0, 21)).client(), "unknown");
    assert_eq!(Fault::new(&iv, Kind::Mm, st::CID.put(0, 1)).client(), "unknown");
    // A status of 0: Linux prints its two lines and no status; the plain line says what is known.
    let f = Fault::new(&fault_iv(fault12::CLIENT_GFX, false, 0x2000, fault12::DATA_WRITE), Kind::Gc, 0);
    assert_eq!(format!("{}", f.linux()).lines().count(), 2);
    assert!(format!("{f}").ends_with("a write (the entry's flag), the hub's status read 0: no engine and no kind known"), "{f}");
    // Client and vmid_src disagreeing: the registers are the client's hub, the name vmid_src's, both said.
    let odd = Fault::new(&fault_iv(fault12::CLIENT_GFX, true, 0x3000, 0), Kind::Gc, st::CID.put(0, 1));
    let text: String = format!("{odd}");
    assert!(format!("{}", odd.linux()).starts_with("[mmhub] page fault") && text.contains("the entry's client 10 is gfxhub's") && odd.client() == "DB", "{text}");
    // The address's upper four bits come from the second data word, under its flags.
    let hi = Fault::new(&fault_iv(fault12::CLIENT_GFX, false, 0xF000_0000_0000 | 0x5000, fault12::DATA_RETRY | fault12::DATA_EXE), Kind::Gc, 0);
    assert_eq!((hi.addr, hi.retry, hi.write), (0xF000_0000_5000, true, false));
}

#[test]
fn ten_faults_in_five_seconds_are_printed_and_the_rest_counted() {
    let mut r = RateLimit::new();
    let printed: Vec<bool> = (0..25u64).map(|i| r.allow(1000 + i * 100).0).collect();
    assert_eq!((printed.iter().filter(|p| **p).count(), printed[..10].iter().all(|p| *p), r.missed, r.missed_total), (10, true, 15, 15));
    // Still inside the five seconds: silent. Past them: printed again, and the interval's count is handed out once.
    assert_eq!(r.allow(1000 + fault12::RATE_INTERVAL_US), (false, None));
    assert_eq!(r.allow(1001 + fault12::RATE_INTERVAL_US), (true, Some(16)));
    assert_eq!((r.allow(1002 + fault12::RATE_INTERVAL_US), r.missed, r.missed_total), ((true, None), 0, 16));
}
