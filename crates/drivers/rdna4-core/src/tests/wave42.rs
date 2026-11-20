//! Wave 42: the arithmetic of NBIF, GMC/MMHUB and IH against what the trace of Linux's own start shows on this card
//! (plan/hw/gaming-pc-gpu-2026-11-18, init.txt lines 9-205 and 624-670): every value a function here computes from
//! what the card answered is the value Linux wrote.

use crate::hub41::{self, Chosen, LayoutStop, Op};
use crate::ih7::{self, Iv, Ring};
use crate::ipfacts::TABLE;
use crate::regmap::{dword, reach_of, Reach};
use crate::{nbreg, pte12};
use std::vec::Vec;

#[test]
fn the_registers_lie_where_the_trace_names_them() {
    for (r, dw) in [
        (nbreg::STRAP2, 0x404_D102),
        (nbreg::REMAP_HDP_MEM_FLUSH, 0xE4D),
        (nbreg::REMAP_HDP_REG_FLUSH, 0xE4E),
        (nbreg::DF_NCS_CONFIG1, 0x1240_0901),
        (nbreg::DOORBELL_APER_EN, 0xDE0),
        (nbreg::BIOS_SCRATCH_4, 0x50),
        (nbreg::INTERRUPT_CNTL, 0xE11),
        (nbreg::INTERRUPT_CNTL2, 0xE12),
        (nbreg::DOORBELL_ENTRY_1, 0xEEC),
        (hub41::FB_OFFSET, 0x1A4C7),
        (hub41::CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32, 0x1A5CF),
        (hub41::BANK_SELECT_RESERVED_CID2, 0x1A500),
        (nbreg::member(hub41::INVALIDATE_ENG0_SEM, hub41::GART_ENGINE, 1), 0x1A586),
        (nbreg::member(hub41::INVALIDATE_ENG0_REQ, hub41::GART_ENGINE, 1), 0x1A598),
        (nbreg::member(hub41::INVALIDATE_ENG0_ACK, hub41::GART_ENGINE, 1), 0x1A5AA),
        (nbreg::member(hub41::INVALIDATE_ENG0_ADDR_RANGE_LO32, 17, 2), 0x1A5CD),
        (nbreg::member(hub41::CONTEXT1_CNTL, 14, 1), 0x1A573),
        (ih7::RB_CNTL, 0x1120),
        (ih7::RB_WPTR_ADDR_LO, 0x1126),
        (ih7::DOORBELL_RPTR_RING1, 0x1133),
        (ih7::CNTL2, 0x1161),
        (ih7::RING1_CLIENT_CFG_DATA, 0x1224),
    ] {
        assert_eq!(dword(&TABLE, r), Some(dw), "{}", r.name);
    }
    // The two registers of the common hw_init that lie past BAR5, and the remap's fixed place inside it.
    assert!(matches!(reach_of(0x404_D102), Reach::NeedsIndirect(_)) && matches!(reach_of(0x1240_0901), Reach::NeedsIndirect(_)));
    assert_eq!((nbreg::REMAPPED_MEM_FLUSH_BYTE, nbreg::REMAPPED_REG_FLUSH_BYTE), (0x7F000, 0x7F004));
    assert_eq!(nbreg::REMAPPED_MEM_FLUSH_BYTE / 4, 0x1FC00, "the flush register of every HDP line of the trace");
}

#[test]
fn the_index_pair_reaches_three_addresses_and_no_other() {
    assert_eq!(nbreg::index_targets(&TABLE), [Some(0x03B0_0000 | 0x0301_0024), Some(0x1013_4408), Some(0x4900_2404)]);
    // Neither is a register of BAR5: both lie past it, where only the pair reaches.
    for t in nbreg::index_targets(&TABLE).into_iter().flatten() {
        assert!(t >= crate::regmap::BAR5_BYTES && t.is_multiple_of(4), "{t:#x}");
    }
}

#[test]
fn nbifs_values_are_linuxs() {
    assert_eq!(nbreg::STRAP_NO_SOFT_RESET_DEV0_F2.put(0x0313_2000, 0), 0x0313_2000, "line 10: the bit is clear already");
    assert_eq!(nbreg::STRAP_NO_SOFT_RESET_DEV0_F2.put(0x0313_2080, 0), 0x0313_2000);
    assert_eq!((nbreg::strap2(0x0313_2080), nbreg::strap2(0x0313_2000)), (0x0313_2000, 0x0313_2000));
    assert_eq!(nbreg::df_ncs_config1(0x0A40_0001), 0x0A41_0031, "line 14");
    assert_eq!(nbreg::BIF_DOORBELL_APER_EN.put(0, 1), 1, "line 16");
}

#[test]
fn the_layout_is_the_cards() {
    let l = hub41::layout(0x3FB0, 0x8000, 0).unwrap();
    assert_eq!((l.vram_start, l.vram_end(), l.vram_bytes), (0x80_0000_0000, 0x83_FAFF_FFFF, 16304 << 20));
    assert_eq!((l.gart_start, l.gart_end(), l.base_offset), (0, 0x1FFF_FFFF, 0));
    assert_eq!((l.agp_start, l.agp_end), (0xFFFF_FFFF_FFFF, 0));
    // A card whose VRAM starts below 512 MiB gets its GART above VRAM on a 4 GiB boundary, as Linux places it.
    let low = hub41::layout(0x1000, 0, 0).unwrap();
    assert_eq!(low.gart_start, 0x1_0000_0000);
    assert_eq!(hub41::layout(0, 0x8000, 0), Err(LayoutStop::Memsize(0)));
    assert_eq!(hub41::layout(u32::MAX, 0x8000, 0), Err(LayoutStop::Memsize(u32::MAX)));
    assert_eq!(hub41::layout(0x3FB0, u32::MAX, 0), Err(LayoutStop::Hub { base: u32::MAX, offset: 0 }));
    assert!(matches!(hub41::layout(0x3FB0, 0x00FF_FFFF, 0), Err(LayoutStop::VramPastHole { .. })));
}

/// The whole of `mmhub_v4_1_0_gart_enable` with the card's answers of the trace, as (dword, value) writes.
fn gart_enable_writes(c: &Chosen) -> Vec<(u32, u32)> {
    let l = hub41::layout(0x3FB0, 0x8000, 0).unwrap();
    // What the card answered to each read of the sequence (init.txt lines 48-155).
    let answer = |dw: u32| match dw {
        0x1A4ED => 0x000A_0000,
        0x1A55B => 0x0000_0501,
        0x1A4E4 => 0x0008_0602,
        0x1A4E5 => 0,
        0x1A564..=0x1A573 => 0x03FF_FC00,
        other => panic!("a read of {other:#x} is not in the trace"),
    };
    let mut out = Vec::new();
    hub41::gart_enable(&hub41::MM, &l, c, |op| -> Result<(), ()> {
        match op {
            Op::Write(r, v) => out.push((dword(&TABLE, r).unwrap(), v)),
            Op::Modify(r, f) => {
                let dw = dword(&TABLE, r).unwrap();
                out.push((dw, f(answer(dw))));
            }
            Op::Pulse(r, _) => panic!("a hub's start pulses nothing ({})", r.name),
        }
        Ok(())
    })
    .unwrap();
    out
}

#[test]
fn gart_enable_writes_what_linux_wrote() {
    // Linux's own choices: the table at VRAM offset 0x3_DAB0_0000, the default page at 0x3_DAAF_F000, the dummy
    // page at 0x63FF_F000.
    let w = gart_enable_writes(&Chosen { table: 0x3_DAB0_0000, scratch: 0x3_DAAF_F000, dummy: 0x63FF_F000 });
    // Lines 33-66.
    let head: [(u32, u32); 28] = [
        (0x1A5CF, 0xDAB0_0001),
        (0x1A5D0, 0x3),
        (0x1A5EF, 0),
        (0x1A5F0, 0),
        (0x1A60F, 0x0001_FFFF),
        (0x1A610, 0),
        (0x1A558, 0),
        (0x1A557, 0x00FF_FFFF),
        (0x1A556, 0),
        (0x1A559, 0x0020_0000),
        (0x1A55A, 0x0020_FEBF),
        (0x1A4C8, 0x003D_AAFF),
        (0x1A4C9, 0),
        (0x1A4F4, 0x0006_3FFF),
        (0x1A4F5, 0),
        (0x1A4ED, 0x000E_0000),
        (0x1A55B, 0x0000_1859),
        (0x1A4E4, 0x0008_0E01),
        (0x1A4E5, 0x3),
        (0x1A4E6, 0x8013_0009),
        (0x1A4FD, 0x1),
        (0x1A503, 0x3FE0),
        (0x1A564, 0x03FF_FC01),
        (0x1A4F7, 0xFFFF_FFFF),
        (0x1A4F8, 0xF),
        (0x1A4F9, 0),
        (0x1A4FA, 0),
        (0x1A4FB, 0),
    ];
    assert_eq!(w[..28], head);
    assert_eq!(w[28], (0x1A4FC, 0));
    // Lines 67-156: contexts 1 to 15, five writes each.
    for i in 0..15u32 {
        let at = 29 + 5 * i as usize;
        assert_eq!(w[at..at + 5], [(0x1A565 + i, 0x03FF_FC07), (0x1A5F1 + 2 * i, 0), (0x1A5F2 + 2 * i, 0), (0x1A611 + 2 * i, 0xFFFF_FFFF), (0x1A612 + 2 * i, 0xF)], "context {}", i + 1);
    }
    // Lines 157-192: the eighteen ranges.
    for i in 0..18u32 {
        let at = 104 + 2 * i as usize;
        assert_eq!(w[at..at + 2], [(0x1A5AB + 2 * i, 0xFFFF_FFFF), (0x1A5AC + 2 * i, 0x1F)], "engine {i}");
    }
    assert_eq!(w.len(), 140, "lines 33-192 hold 140 writes");
    assert_eq!(hub41::fault_cntl(0x3FFF_FFFC), 0x3FFF_FFFC, "line 196");
    assert_eq!(hub41::invalidate_req(0), 0x00F8_0001, "line 200");
    assert_eq!(0x1010_5C17 | hub41::RESERVED_CACHE_PRIVATE_INVALIDATION, 0x1210_5C17, "line 204");
}

#[test]
fn a_page_entry_is_gfx12s() {
    assert_eq!(pte12::GART_FLAGS, 3 << 54 | 1 << 4 | 1 << 63);
    assert_eq!(pte12::FILL_BYTE, 0x10);
    let e = pte12::bound(0x1_2345_6000).unwrap();
    assert_eq!(e, 0x1_2345_6000 | 0x80C0_0000_0000_0077);
    let d = pte12::decode(e);
    assert_eq!((d.addr, d.valid, d.system, d.snooped, d.executable, d.readable, d.writeable), (0x1_2345_6000, true, true, true, true, true, true));
    assert_eq!((d.mtype, d.is_pte, d.prt, d.frag), (3, true, false, 0));
    // Linux's fresh table: every byte 0x10, an entry that is not valid.
    let fresh = pte12::decode(u64::from_le_bytes([pte12::FILL_BYTE; 8]));
    assert!(!fresh.valid && !fresh.system);
    // An unbound entry: the dummy page, no flag.
    assert_eq!(pte12::unbound(0x63FF_F000), 0x63FF_F000);
    assert!(!pte12::decode(pte12::unbound(0x63FF_F000)).valid);
    // The card reaches 44 bits and whole pages only.
    assert_eq!(pte12::bound((1 << 44) - 4096), Ok(((1 << 44) - 4096) | pte12::BOUND_FLAGS));
    assert_eq!(pte12::bound(1 << 44), Err(pte12::PteError::PastCeiling(1 << 44)));
    assert_eq!(pte12::bound(0x1001), Err(pte12::PteError::Unaligned(0x1001)));
}

#[test]
fn the_ih_values_are_linuxs() {
    // Lines 625, 629: off.
    assert_eq!(ih7::toggled(Ring::Ring0, 0x4061_0000, false), 0x4061_0000);
    assert_eq!(ih7::toggled(Ring::Ring1, 0x4041_0000, false), 0x4041_0000);
    // Lines 638, 647: the rings' control with MSI on.
    assert_eq!(ih7::rb_cntl(Ring::Ring0, 0x4061_0000, true), 0xC031_0120);
    assert_eq!(ih7::rb_cntl(Ring::Ring1, 0x4041_0000, true), 0xC010_0320);
    // Lines 643, 650, 652.
    assert_eq!((ih7::DOORBELL_RING0, ih7::DOORBELL_RING1), (0x2F0, 0x2F2));
    assert_eq!((ih7::doorbell_rptr(ih7::DOORBELL_RING0), ih7::doorbell_rptr(ih7::DOORBELL_RING1)), (0x1000_02F0, 0x1000_02F2));
    assert_eq!(ih7::doorbell_range(0), 0x0005_7801);
    // Line 634, 654, 656, 658, 660, 662.
    assert_eq!(ih7::interrupt_cntl(0x0001_0000), 0x0001_0000);
    assert_eq!(ih7::f::CLIENT18_IS_STORM_CLIENT.put(0, 1), 0x0004_0000);
    assert_eq!(ih7::f::FLOOD_CNTL_ENABLE.put(0, 1), 0x8);
    assert_eq!(ih7::f::MSI_STORM_DELAY.put(0, 3), 0x3);
    assert_eq!(ih7::f::CFG_INDEX.put(0, 0), 0);
    assert_eq!(ih7::ring1_client_cfg(0), 0x0001_000A);
    // Lines 664, 666: on.
    assert_eq!(ih7::toggled(Ring::Ring0, 0x4031_0120, true), 0x4033_0121);
    assert_eq!(ih7::toggled(Ring::Ring1, 0x4010_0220, true), 0x4010_0221);
    // Lines 669-670.
    assert_eq!(ih7::self_int(0x8, 0x4010_0221), (0x108, 0x4010_0221));
    // ih_v7_0_irq_disable's first step takes the enable back and leaves the timeout (ih_v7_0.c:433-440, :92-118).
    assert_eq!(ih7::self_int_off(0x108, 0x4010_0221), (0x8, 0x4010_0221));
    assert_eq!(ih7::self_int_off(0x8, 0x4010_0221 | ih7::f::RB_USED_INT_THRESHOLD.mask), (0x8, 0x4010_0221));
    assert_eq!((ih7::RING_BYTES / ih7::ENTRY_BYTES, ih7::PTR_MASK), (8192, 0x3_FFFF));
}

/// Entries of the trace of 18.11. (trace-load.txt, `amdgpu_iv` events; the trace holds the decoded fields, so the
/// dwords are built by Linux's layout and must decode to exactly what Linux printed).
#[test]
fn entries_decode_as_linux_printed_them() {
    // (trace line, client, source, ring, timestamp, src_data[0])
    let seen: [(u32, u8, u8, u8, u64, u32); 7] = [
        (62122, 4, 9, 0, 25_997_017_620, 0x0000_0006),
        (108353, 20, 181, 5, 26_027_898_750, 0),
        (108354, 20, 181, 20, 26_027_901_895, 0),
        (108357, 10, 49, 16, 26_030_656_633, 0),
        (108358, 16, 119, 0, 26_030_680_255, 0),
        (154371, 4, 87, 0, 26_098_435_588, 0x039A_EE10),
        (153873, 4, 79, 0, 26_097_363_931, 0x038A_942C),
    ];
    for (line, client_id, src_id, ring_id, timestamp, d0) in seen {
        let dw = [u32::from(client_id) | u32::from(src_id) << 8 | u32::from(ring_id) << 16, timestamp as u32, (timestamp >> 32) as u32, 0, d0, 0, 0, 0];
        let iv = Iv::decode(&dw);
        assert_eq!(iv, Iv { client_id, src_id, ring_id, timestamp, src_data: [d0, 0, 0, 0], ..Iv::default() }, "trace line {line}");
        assert_eq!(iv.encode(), dw, "trace line {line}");
    }
    // The fields no entry of the trace used: VMID and its source, the timestamp's source, PASID, node.
    let dw = [0x8F12_3456, 0xDEAD_BEEF, 0x8000_ABCD, 0x0042_BEEF, 1, 2, 3, 4];
    let iv = Iv::decode(&dw);
    assert_eq!((iv.client_id, iv.src_id, iv.ring_id, iv.vmid, iv.vmid_src), (0x56, 0x34, 0x12, 0xF, true));
    assert_eq!((iv.timestamp, iv.timestamp_src, iv.pasid, iv.node_id, iv.src_data), (0xABCD_DEAD_BEEF, true, 0xBEEF, 0x42, [1, 2, 3, 4]));
    assert_eq!(iv.encode(), dw);
    // After an overflow reading goes on one entry past the write pointer, around the ring's end.
    assert_eq!(ih7::after_overflow(0x100), 0x120);
    assert_eq!(ih7::after_overflow(ih7::RING_BYTES - 32), 0);
}
