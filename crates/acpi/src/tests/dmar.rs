//! GP B7: the DMAR table in a whole table set, and the survey of its units against register fakes that refuse
//! every access Linux would not make (another offset, another page, any write: there is no write path at all).

use crate::dmar::{self, read_unit, survey, verdict_text, DmaVerdict, UnitState};
use crate::parse::parse;
use crate::testkit::*;
use alloc::vec::Vec;
use core::cell::RefCell;
use gp_platform::dmar as rules;

/// Where the Gaming-PC-shaped set keeps its tables (beside the Mini-PC world's offsets).
const DMAR_AT: u64 = at::BASE + 0xE000;

/// The Mini-PC world with the Gaming-PC's DMAR added to the root tables.
fn world_with(table: Vec<u8>) -> FakeMem {
    let mut m = minipc();
    let list = [at::FADT, at::MADT, at::HPET, at::MCFG, at::SRAT, at::SSDT1, at::SSDT2, DMAR_AT];
    m.put(at::XSDT, xsdt(&list));
    m.put(at::RSDT, rsdt(&list.map(|x| x as u32)));
    m.put(DMAR_AT, table);
    m
}

/// An IOMMU register page: reads of the offsets Linux reads at start-up answer, everything else panics.
struct Unit {
    base: u64,
    ver: u32,
    cap: u64,
    ecap: u64,
    gsts: u32,
    pmen: u32,
}

const GP_CAP: u64 = 0xE9DE_008C_EE69_0462;

fn unit(base: u64, gsts: u32, pmen: u32) -> Unit {
    Unit { base, ver: 0x70, cap: GP_CAP, ecap: 0x001A_CA9A_00F0_EFDE, gsts, pmen }
}

fn reader<'a>(units: &'a [Unit], seen: &'a RefCell<Vec<u64>>) -> impl FnMut(u64) -> Option<u32> + 'a {
    move |addr| {
        seen.borrow_mut().push(addr);
        let u = units.iter().find(|u| (u.base..u.base + 0x1000).contains(&addr)).unwrap_or_else(|| panic!("read outside every unit: {addr:#x}"));
        Some(match addr - u.base {
            0x00 => u.ver,
            0x08 => u.cap as u32,
            0x0C => (u.cap >> 32) as u32,
            0x10 => u.ecap as u32,
            0x14 => (u.ecap >> 32) as u32,
            0x1C => u.gsts,
            0x64 => {
                assert!(rules::has_protected_regions(u.cap), "PMEN read on a unit without protected regions");
                u.pmen
            }
            o => panic!("read of register {o:#x}: not one of version, capabilities, status, PMEN"),
        })
    }
}

#[test]
fn the_table_is_read_with_the_others() {
    let a = parse(&mut world_with(rules::gaming_pc()), at::RSDP);
    assert!(a.notes.is_empty(), "{:?}", a.notes);
    let d = a.dmar.as_ref().expect("DMAR parsed");
    assert_eq!((d.host_address_width, d.units.len(), d.rmrr), (42, 2, 0));
    assert!(a.tables.iter().any(|t| &t.header.signature == b"DMAR" && t.used && t.checksum_ok == Some(true)));
    // The Mini-PC's own set has none, and nothing else about it changed.
    let m = parse(&mut minipc(), at::RSDP);
    assert!(m.dmar.is_none() && m.notes.is_empty() && m.ivrs.is_some());
}

#[test]
fn a_bad_table_is_noted_not_used() {
    let mut t = rules::gaming_pc();
    t[9] ^= 1;
    let a = parse(&mut world_with(t), at::RSDP);
    assert!(a.dmar.is_none());
    assert!(a.notes.iter().any(|n| &n.signature == b"DMAR" && n.what == "bad_checksum"));
    // A structure that runs past the end: the unit before it is kept and the problem is a note.
    let t = rules::DmarBuilder::new(42, 0).drhd(1, 0, 0, 0xFC81_0000, &[]).raw(&[0, 0, 0x40, 0]).build();
    let a = parse(&mut world_with(t), at::RSDP);
    assert_eq!(a.dmar.as_ref().unwrap().units.len(), 1);
    assert!(a.notes.iter().any(|n| &n.signature == b"DMAR" && n.what == "structure_length"));
}

#[test]
fn survey_of_the_gaming_pc_with_translation_off() {
    let d = rules::parse(&rules::gaming_pc()).dmar.unwrap();
    let units = [unit(0xFC80_0000, 0, 0), unit(0xFC81_0000, 0, 0)];
    let seen = RefCell::new(Vec::new());
    let s = survey(&d, &mut reader(&units, &seen));
    assert_eq!(s.verdict, DmaVerdict::Free);
    assert!(s.units.iter().all(|u| matches!(u.2, UnitState::Present { version: (7, 0), translation: false, .. })));
    // Six reads per unit, each inside its first page, in a fixed order.
    assert_eq!(*seen.borrow(), [0xFC80_0008, 0xFC80_000C, 0xFC80_0010, 0xFC80_0014, 0xFC80_0000, 0xFC80_001C, 0xFC80_0064, 0xFC81_0008, 0xFC81_000C, 0xFC81_0010, 0xFC81_0014, 0xFC81_0000, 0xFC81_001C, 0xFC81_0064]);
    let lines = s.lines();
    assert_eq!(lines[0], "acpi: DMAR width 42 flags 0x5 (platform opt-in true), units 2, rmrr 0, atsr 0, satc 1");
    assert_eq!(lines[1], "acpi: DMAR unit 0xfc800000 ver 7.0 gsts 0x00000000 translation off interrupt_remapping off protected_memory off");
    assert_eq!(lines[2], "acpi: DMAR unit 0xfc810000 ver 7.0 gsts 0x00000000 translation off interrupt_remapping off protected_memory off (all devices)");
    assert_eq!(lines[3], "acpi: DMA free: no IOMMU translates or protects memory");
}

#[test]
fn translation_or_a_protected_region_blocks() {
    let d = rules::parse(&rules::gaming_pc()).dmar.unwrap();
    let seen = RefCell::new(Vec::new());
    let on = [unit(0xFC80_0000, 0, 0), unit(0xFC81_0000, rules::GSTS_TES | rules::GSTS_RTPS, 0)];
    let s = survey(&d, &mut reader(&on, &seen));
    assert_eq!(s.verdict, DmaVerdict::TranslationOn);
    assert!(s.verdict.blocks_dma());
    assert!(s.lines()[2].contains("gsts 0xc0000000 translation on"));
    assert_eq!(s.lines()[3], "acpi: DMA BLOCKED: the firmware left DMA translation on; no DMA driver starts");

    let pmr = [unit(0xFC80_0000, 0, rules::PMEN_EPM | rules::PMEN_PRS), unit(0xFC81_0000, 0, 0)];
    let s = survey(&d, &mut reader(&pmr, &seen));
    assert_eq!(s.verdict, DmaVerdict::ProtectedMemoryOn);
    assert!(s.lines()[1].ends_with("protected_memory on"));

    // Interrupt remapping alone does not block DMA, but the line names it.
    let ir = [unit(0xFC80_0000, 0, 0), unit(0xFC81_0000, rules::GSTS_IRES | rules::GSTS_QIES, 0)];
    let s = survey(&d, &mut reader(&ir, &seen));
    assert_eq!(s.verdict, DmaVerdict::Free);
    assert!(s.lines()[2].contains("interrupt_remapping on (plain interrupts blocked)"));
}

#[test]
fn a_unit_that_cannot_be_read_is_unknown_and_does_not_block() {
    let d = rules::parse(&rules::gaming_pc()).dmar.unwrap();
    // The register guard refuses the second unit's page.
    let units = [unit(0xFC80_0000, 0, 0)];
    let seen = RefCell::new(Vec::new());
    let mut inner = reader(&units, &seen);
    let s = survey(&d, &mut |a| if a >= 0xFC81_0000 { None } else { inner(a) });
    assert_eq!(s.units[1].2, UnitState::Unreadable);
    assert_eq!(s.verdict, DmaVerdict::Unknown);
    assert!(!s.verdict.blocks_dma());
    assert_eq!(s.lines()[2], "acpi: DMAR unit 0xfc810000 not readable");
    assert!(verdict_text(s.verdict).starts_with("state unknown"));

    // Nothing answers (all ones): no PMEN read follows from the all-ones capability word.
    let seen = RefCell::new(Vec::new());
    let s = survey(&d, &mut |a| {
        seen.borrow_mut().push(a & 0xFFF);
        Some(u32::MAX)
    });
    assert!(s.units.iter().all(|u| u.2 == UnitState::Absent));
    assert!(!seen.borrow().contains(&0x64));
    assert_eq!(s.verdict, DmaVerdict::Unknown);
    assert_eq!(s.lines()[1], "acpi: DMAR unit 0xfc800000 does not answer (all ones)");

    // A unit without protected regions is never asked for PMEN (the fake would panic).
    let mut no_pmr = unit(0xFED9_0000, 0, 0);
    no_pmr.cap &= !(rules::CAP_PLMR | rules::CAP_PHMR);
    let seen = RefCell::new(Vec::new());
    let r = read_unit(&rules::Drhd { flags: 1, size: 0, segment: 0, address: 0xFED9_0000 }, &mut reader(core::slice::from_ref(&no_pmr), &seen)).unwrap();
    assert_eq!(r.pmen, None);
    // An address at the very top does not wrap.
    assert_eq!(read_unit(&rules::Drhd { flags: 0, size: 0, segment: 0, address: u64::MAX - 4 }, &mut |_| Some(0)), None);
}

#[test]
fn without_a_boot_the_gate_is_open() {
    // Host tests never run `live::init`: the state is "no table", as on the Mini-PC.
    assert!(dmar::found().is_none());
    assert_eq!(dmar::verdict(), DmaVerdict::NoTable);
    assert_eq!(dmar::dma_gate(), Ok(()));
}
