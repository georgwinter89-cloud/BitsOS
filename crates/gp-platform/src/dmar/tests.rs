use super::*;

const OFF: UnitState = UnitState::Present {
    version: (7, 0),
    translation: false,
    root_table_set: false,
    queued_invalidation: false,
    interrupt_remapping: false,
    compat_interrupts: false,
    protected_memory: false,
};

/// The capability words Linux printed for both units on the Gaming-PC.
const GP_CAP: u64 = 0xE9DE_008C_EE69_0462;
const GP_ECAP: u64 = 0x001A_CA9A_00F0_EFDE;

fn regs(gsts: u32, pmen: Option<u32>) -> Option<UnitRegs> {
    Some(UnitRegs { ver: 0x70, cap: GP_CAP, ecap: GP_ECAP, gsts, pmen })
}

#[test]
fn the_gaming_pcs_table() {
    let t = gaming_pc();
    assert_eq!(t.len(), 152, "the machine's table is 152 bytes");
    assert_eq!(t.iter().fold(0u8, |a, b| a.wrapping_add(*b)), 0);
    let p = parse(&t);
    assert!(p.problems.is_empty(), "{:?}", p.problems);
    let d = p.dmar.unwrap();
    assert_eq!(d.host_address_width, 42);
    assert!(d.platform_opt_in() && d.intr_remap());
    assert_eq!(d.units.len(), 2);
    assert_eq!((d.units[0].address, d.units[0].include_all(), d.units[0].reg_bytes()), (0xFC80_0000, false, 4096));
    assert_eq!((d.units[1].address, d.units[1].include_all(), d.units[1].segment), (0xFC81_0000, true, 0));
    assert_eq!((d.rmrr, d.atsr, d.satc, d.unknown), (0, 0, 1, 0), "No RMRR found, No ATSR found, one SATC");
}

#[test]
fn the_gaming_pcs_capabilities_have_protected_regions() {
    assert!(has_protected_regions(GP_CAP), "so the protected-memory register is read there");
    assert!(!has_protected_regions(GP_CAP & !(CAP_PLMR | CAP_PHMR)));
}

#[test]
fn status_bits() {
    assert!(translation_on(GSTS_TES));
    assert!(!translation_on(!GSTS_TES));
    assert_eq!(unit_state(regs(0, Some(0))), OFF);
    assert_eq!(unit_state(None), UnitState::Unreadable);
    assert_eq!(unit_state(Some(UnitRegs { ver: u32::MAX, cap: u64::MAX, ecap: u64::MAX, gsts: u32::MAX, pmen: None })), UnitState::Absent);
    // All ones in only one of the two words is a (strange) unit, as in Linux.
    assert!(matches!(unit_state(Some(UnitRegs { ver: 0x10, cap: u64::MAX, ecap: 0, gsts: 0, pmen: None })), UnitState::Present { version: (1, 0), .. }));
    let all = unit_state(regs(GSTS_TES | GSTS_RTPS | GSTS_QIES | GSTS_IRES | GSTS_CFIS, Some(PMEN_EPM | PMEN_PRS)));
    assert_eq!(
        all,
        UnitState::Present {
            version: (7, 0),
            translation: true,
            root_table_set: true,
            queued_invalidation: true,
            interrupt_remapping: true,
            compat_interrupts: true,
            protected_memory: true
        }
    );
    // The enable bit alone is a request; the status bit says the region is on.
    assert!(matches!(unit_state(regs(0, Some(PMEN_EPM))), UnitState::Present { protected_memory: false, .. }));
    assert!(matches!(unit_state(regs(0, None)), UnitState::Present { protected_memory: false, .. }));
}

#[test]
fn verdicts() {
    let on = unit_state(regs(GSTS_TES | GSTS_RTPS, Some(0)));
    let pmr = unit_state(regs(0, Some(PMEN_PRS)));
    assert_eq!(verdict(None), DmaVerdict::NoTable);
    assert_eq!(verdict(Some(&[OFF, OFF])), DmaVerdict::Free);
    assert_eq!(verdict(Some(&[])), DmaVerdict::Unknown, "a table without a unit is not a clean bill");
    assert_eq!(verdict(Some(&[OFF, UnitState::Unreadable])), DmaVerdict::Unknown);
    assert_eq!(verdict(Some(&[UnitState::Absent, OFF])), DmaVerdict::Unknown);
    assert_eq!(verdict(Some(&[OFF, pmr])), DmaVerdict::ProtectedMemoryOn);
    // Translation on any unit wins, wherever it stands in the list.
    for units in [[on, OFF, pmr], [OFF, on, UnitState::Unreadable], [UnitState::Absent, pmr, on]] {
        assert_eq!(verdict(Some(&units)), DmaVerdict::TranslationOn);
    }
    for v in [DmaVerdict::NoTable, DmaVerdict::Free, DmaVerdict::Unknown] {
        assert!(!v.blocks_dma(), "{v:?}");
    }
    assert!(DmaVerdict::TranslationOn.blocks_dma() && DmaVerdict::ProtectedMemoryOn.blocks_dma());
    assert_eq!(DmaVerdict::TranslationOn.name(), "translation_on");
}

#[test]
fn every_gsts_value_gives_translation_exactly_by_bit_31() {
    // The other bits must never be read as translation: walk every single bit and its complement.
    for bit in 0..32 {
        let g = 1u32 << bit;
        assert_eq!(verdict(Some(&[unit_state(regs(g, Some(0)))])).blocks_dma(), bit == 31, "bit {bit}");
        assert_eq!(verdict(Some(&[unit_state(regs(!g, Some(0)))])).blocks_dma(), bit != 31, "all but bit {bit}");
    }
}

#[test]
fn interrupt_remapping_without_compat_format_is_named() {
    assert!(!blocks_plain_interrupts(&OFF));
    assert!(blocks_plain_interrupts(&unit_state(regs(GSTS_IRES, None))));
    assert!(!blocks_plain_interrupts(&unit_state(regs(GSTS_IRES | GSTS_CFIS, None))));
    assert!(!blocks_plain_interrupts(&UnitState::Unreadable));
}

#[test]
fn malformed_tables() {
    let what = |t: &[u8]| parse(t).problems.iter().map(|p| p.what).collect::<Vec<_>>();
    assert_eq!(what(&[]), ["too_short"]);
    assert_eq!(what(&gaming_pc()[..47]), ["too_short"]);
    assert!(parse(&gaming_pc()[..47]).dmar.is_none());
    // Header only: a table, no unit.
    let p = parse(&gaming_pc()[..48]);
    assert!(p.problems.is_empty() && p.dmar.unwrap().units.is_empty());
    // A structure header cut in the middle.
    assert_eq!(what(&gaming_pc()[..50]), ["structure_cut"]);
    // The first unit cut: nothing kept from it, and no read past the end.
    let p = parse(&gaming_pc()[..60]);
    assert_eq!(p.problems, [Problem { what: "structure_length", at: 48 }]);
    assert!(p.dmar.unwrap().units.is_empty());
    // Cut inside the second unit: the first stays.
    let p = parse(&gaming_pc()[..80]);
    assert_eq!(p.problems.len(), 1);
    assert_eq!(p.dmar.unwrap().units.len(), 1);
    // A zero-length structure ends the walk (Linux: "Invalid 0-length structure").
    let t = DmarBuilder::new(39, 0).drhd(1, 0, 0, 0xFED9_0000, &[]).raw(&[0, 0, 0, 0]).drhd(1, 0, 0, 0xFED9_1000, &[]).build();
    let p = parse(&t);
    assert_eq!(p.problems, [Problem { what: "zero_length", at: 64 }]);
    assert_eq!(p.dmar.unwrap().units.len(), 1);
    // Lengths 1 to 3 cannot hold their own header.
    for len in 1..4u8 {
        let t = DmarBuilder::new(39, 0).raw(&[0, 0, len, 0, 0, 0, 0, 0]).build();
        assert_eq!(what(&t), ["structure_length"], "length {len}");
    }
    // A unit shorter than its fixed part, one without an address, and an unknown type: named, walk goes on.
    let t = DmarBuilder::new(39, 0).raw(&[0, 0, 8, 0, 1, 0, 0, 0]).drhd(1, 0, 0, 0, &[]).other(9, &[1, 2, 3, 4]).drhd(1, 0, 0, 0xFED9_0000, &[]).build();
    let p = parse(&t);
    assert_eq!(p.problems.iter().map(|x| x.what).collect::<Vec<_>>(), ["drhd_short", "drhd_no_address"]);
    let d = p.dmar.unwrap();
    assert_eq!((d.units.len(), d.unknown), (1, 1));
}

#[test]
fn a_hostile_table_is_bounded() {
    let mut b = DmarBuilder::new(48, 0);
    for i in 0..200u64 {
        b = b.drhd(0, 0xFF, 0, 0x1000 * (i + 1), &[]);
    }
    let p = parse(&b.build());
    let d = p.dmar.unwrap();
    assert_eq!(d.units.len(), MAX_UNITS);
    assert_eq!(p.problems.len(), 1);
    assert_eq!(p.problems[0].what, "too_many_units");
    assert!(d.units.iter().all(|u| u.reg_bytes() == 1 << 20), "a wild size field maps at most 1 MiB");
    // Every cut of the real shape, and every single flipped byte, parses without a panic.
    let t = gaming_pc();
    for cut in 0..t.len() {
        let _ = parse(&t[..cut]);
    }
    for i in 36..t.len() {
        for v in [0u8, 1, 0x7F, 0xFF] {
            let mut m = t.clone();
            m[i] = v;
            let p = parse(&m);
            // A flipped type byte can turn the third structure into a unit, never more.
            assert!(p.dmar.is_some_and(|d| d.units.len() <= 3));
        }
    }
}

#[test]
fn width_field() {
    assert_eq!(parse(&DmarBuilder::new(39, 0).build()).dmar.unwrap().host_address_width, 39);
    // Stored 0xFF: 256 bits, no overflow.
    let mut t = DmarBuilder::new(1, 0).build();
    t[36] = 0xFF;
    assert_eq!(parse(&t).dmar.unwrap().host_address_width, 256);
}
